#!/usr/bin/env python3
"""CoreAudio 设备音量读写工具（纯 ctypes，无第三方依赖）

用途：
  - 排查虚拟声卡的异常衰减（例如豆包虚拟设备固定 -18 dB）
  - M0 电平实验前把虚拟设备音量归一到 1.0
  - 列出所有设备及其 输入/输出 音量

用法：
  python3 ca_volume.py list
  python3 ca_volume.py get   <设备名关键字>
  python3 ca_volume.py set   <设备名关键字> <0.0-1.0>
"""
import ctypes
import ctypes.util
import sys

ca = ctypes.CDLL(ctypes.util.find_library("CoreAudio"))
cf = ctypes.CDLL(ctypes.util.find_library("CoreFoundation"))


class Addr(ctypes.Structure):
    _fields_ = [
        ("mSelector", ctypes.c_uint32),
        ("mScope", ctypes.c_uint32),
        ("mElement", ctypes.c_uint32),
    ]


def fourcc(s: str) -> int:
    return int.from_bytes(s.encode("ascii"), "big")


K_SYSTEM = 1
SCOPE = {"in": fourcc("inpt"), "out": fourcc("outp"), "glob": fourcc("glob")}
EL_MAIN = 0
P_DEVICES = fourcc("dev#")
P_NAME = fourcc("lnam")
P_UID = fourcc("uid ")
P_VOL = fourcc("volm")
P_MUTE = fourcc("mute")

ca.AudioObjectGetPropertyDataSize.argtypes = [
    ctypes.c_uint32, ctypes.POINTER(Addr), ctypes.c_uint32, ctypes.c_void_p,
    ctypes.POINTER(ctypes.c_uint32)]
ca.AudioObjectGetPropertyDataSize.restype = ctypes.c_int32

ca.AudioObjectGetPropertyData.argtypes = [
    ctypes.c_uint32, ctypes.POINTER(Addr), ctypes.c_uint32, ctypes.c_void_p,
    ctypes.POINTER(ctypes.c_uint32), ctypes.c_void_p]
ca.AudioObjectGetPropertyData.restype = ctypes.c_int32

ca.AudioObjectSetPropertyData.argtypes = [
    ctypes.c_uint32, ctypes.POINTER(Addr), ctypes.c_uint32, ctypes.c_void_p,
    ctypes.c_uint32, ctypes.c_void_p]
ca.AudioObjectSetPropertyData.restype = ctypes.c_int32

ca.AudioObjectHasProperty.argtypes = [ctypes.c_uint32, ctypes.POINTER(Addr)]
ca.AudioObjectHasProperty.restype = ctypes.c_bool

cf.CFStringGetLength.argtypes = [ctypes.c_void_p]
cf.CFStringGetLength.restype = ctypes.c_long
cf.CFStringGetCString.argtypes = [ctypes.c_void_p, ctypes.c_char_p,
                                  ctypes.c_long, ctypes.c_uint32]
cf.CFStringGetCString.restype = ctypes.c_bool


def cfstr(ref) -> str | None:
    if not ref:
        return None
    n = cf.CFStringGetLength(ref)
    buf = ctypes.create_string_buffer(n * 4 + 1)
    if cf.CFStringGetCString(ref, buf, len(buf), 0x08000100):
        return buf.value.decode("utf-8", "replace")
    return None


def _size(obj, addr):
    sz = ctypes.c_uint32(0)
    err = ca.AudioObjectGetPropertyDataSize(obj, ctypes.byref(addr), 0, None,
                                            ctypes.byref(sz))
    return err, sz.value


def get_devices():
    addr = Addr(P_DEVICES, SCOPE["glob"], EL_MAIN)
    err, sz = _size(K_SYSTEM, addr)
    if err:
        raise RuntimeError(f"枚举设备失败: {err}")
    n = sz // 4
    buf = (ctypes.c_uint32 * n)()
    err = ca.AudioObjectGetPropertyData(K_SYSTEM, ctypes.byref(addr), 0, None,
                                        ctypes.byref(ctypes.c_uint32(sz)), buf)
    if err:
        raise RuntimeError(f"读取设备列表失败: {err}")
    return list(buf)


def get_cfstring(obj, selector):
    addr = Addr(selector, SCOPE["glob"], EL_MAIN)
    if not ca.AudioObjectHasProperty(obj, ctypes.byref(addr)):
        return None
    err, sz = _size(obj, addr)
    if err or sz != ctypes.sizeof(ctypes.c_void_p):
        return None
    ref = ctypes.c_void_p()
    err = ca.AudioObjectGetPropertyData(obj, ctypes.byref(addr), 0, None,
                                        ctypes.byref(ctypes.c_uint32(sz)),
                                        ctypes.byref(ref))
    return None if err else cfstr(ref.value)


def channel_count(obj, scope):
    """用 stream 配置估算通道数（简化：尝试 element 1/2 是否有音量）"""
    addr = Addr(P_VOL, SCOPE[scope], EL_MAIN)
    if ca.AudioObjectHasProperty(obj, ctypes.byref(addr)):
        return [EL_MAIN]
    chans = []
    for el in range(1, 9):
        a = Addr(P_VOL, SCOPE[scope], el)
        if ca.AudioObjectHasProperty(obj, ctypes.byref(a)):
            chans.append(el)
    return chans


def get_vol(obj, scope, el):
    addr = Addr(P_VOL, SCOPE[scope], el)
    val = ctypes.c_float(0.0)
    err = ca.AudioObjectGetPropertyData(obj, ctypes.byref(addr), 0, None,
                                        ctypes.byref(ctypes.c_uint32(4)),
                                        ctypes.byref(val))
    return None if err else val.value


def set_vol(obj, scope, el, v):
    addr = Addr(P_VOL, SCOPE[scope], el)
    val = ctypes.c_float(v)
    return ca.AudioObjectSetPropertyData(obj, ctypes.byref(addr), 0, None, 4,
                                         ctypes.byref(val))


def db(v):
    import math
    return 20 * math.log10(v) if v and v > 0 else float("-inf")


def cmd_list():
    print(f"{'ID':>4}  {'设备名':<38} {'in':>22} {'out':>22}")
    print("-" * 92)
    for d in get_devices():
        name = get_cfstring(d, P_NAME) or "?"
        cells = []
        for scope in ("in", "out"):
            chans = channel_count(d, scope)
            if not chans:
                cells.append("—")
            else:
                vs = [get_vol(d, scope, el) for el in chans]
                vs = [v for v in vs if v is not None]
                if not vs:
                    cells.append("—")
                else:
                    txt = "/".join(f"{v:.3f}" for v in vs)
                    cells.append(f"{txt}  ({db(max(vs)):+.1f}dB)")
        print(f"{d:>4}  {name:<38} {cells[0]:>22} {cells[1]:>22}")


def match(keyword):
    hits = [d for d in get_devices()
            if keyword.lower() in (get_cfstring(d, P_NAME) or "").lower()
            or keyword.lower() in (get_cfstring(d, P_UID) or "").lower()]
    return hits


def cmd_get(keyword):
    for d in match(keyword):
        print(f"\n设备 {d}: {get_cfstring(d, P_NAME)}  (UID: {get_cfstring(d, P_UID)})")
        for scope in ("in", "out"):
            chans = channel_count(d, scope)
            for el in chans:
                v = get_vol(d, scope, el)
                label = "main" if el == EL_MAIN else f"ch{el}"
                print(f"  {scope:3s} {label:5s} = {v}  ({db(v):+.1f} dB)" if v is not None
                      else f"  {scope:3s} {label:5s} = 不支持")


def cmd_set(keyword, value):
    v = float(value)
    for d in match(keyword):
        print(f"\n设置设备 {d}: {get_cfstring(d, P_NAME)} → {v}")
        for scope in ("in", "out"):
            for el in channel_count(d, scope):
                old = get_vol(d, scope, el)
                err = set_vol(d, scope, el, v)
                label = "main" if el == EL_MAIN else f"ch{el}"
                print(f"  {scope:3s} {label:5s}: {old} → {get_vol(d, scope, el)}"
                      f"  {'✅' if err == 0 else f'❌ err={err}'}")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)
    c = sys.argv[1]
    if c == "list":
        cmd_list()
    elif c == "get" and len(sys.argv) > 2:
        cmd_get(sys.argv[2])
    elif c == "set" and len(sys.argv) > 3:
        cmd_set(sys.argv[2], sys.argv[3])
    else:
        print(__doc__)
        sys.exit(1)
