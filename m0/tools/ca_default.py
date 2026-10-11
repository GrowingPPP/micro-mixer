#!/usr/bin/env python3
"""切换/查询系统默认输出设备（纯 ctypes，无第三方依赖）。

用途：验证「监听」功能时，需要把监听那一路单独引到一个**能录音**的设备上。
把默认输出切到虚拟声卡、同时把 App 的主输出指到别处，
录音里就只剩监听的声音 —— 干净隔离。

用法:
  python3 ca_default.py get               # 打印当前默认输出（id + 名字）
  python3 ca_default.py list              # 列出所有输出设备
  python3 ca_default.py set <id|关键字>    # 切默认输出
"""
import ctypes, ctypes.util, sys

cf = ctypes.CDLL(ctypes.util.find_library('CoreFoundation'))
ca = ctypes.CDLL(ctypes.util.find_library('CoreAudio'))
K_SYSTEM = 1
SCOPE_GLOBAL = int.from_bytes(b'glob', 'big')


class Addr(ctypes.Structure):
    _fields_ = [("mSelector", ctypes.c_uint32),
                ("mScope", ctypes.c_uint32),
                ("mElement", ctypes.c_uint32)]


def fourcc(s): return int.from_bytes(s.encode(), 'big')


ca.AudioObjectGetPropertyData.argtypes = [
    ctypes.c_uint32, ctypes.POINTER(Addr), ctypes.c_uint32,
    ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint32), ctypes.c_void_p]
ca.AudioObjectGetPropertyData.restype = ctypes.c_int32
ca.AudioObjectSetPropertyData.argtypes = [
    ctypes.c_uint32, ctypes.POINTER(Addr), ctypes.c_uint32,
    ctypes.c_void_p, ctypes.c_uint32, ctypes.c_void_p]
ca.AudioObjectSetPropertyData.restype = ctypes.c_int32


def dev_name(d):
    addr = Addr(fourcc('lnam'), SCOPE_GLOBAL, 0)
    obj = ctypes.c_void_p(0); size = ctypes.c_uint32(ctypes.sizeof(obj))
    if ca.AudioObjectGetPropertyData(d, ctypes.byref(addr), 0, None,
                                     ctypes.byref(size), ctypes.byref(obj)) != 0:
        return "(?)"
    if not obj.value:
        return "(无名)"
    buf = ctypes.create_string_buffer(256)
    cf.CFStringGetCString.restype = ctypes.c_bool
    cf.CFStringGetCString(ctypes.c_void_p(obj.value), buf, 256, 0x08000100)
    return buf.value.decode('utf-8', 'ignore')


def output_devices():
    addr = Addr(fourcc('dev#'), SCOPE_GLOBAL, 0)
    size = ctypes.c_uint32(0)
    ca.AudioObjectGetPropertyDataSize(K_SYSTEM, ctypes.byref(addr), 0, None, ctypes.byref(size))
    n = size.value // 4
    arr = (ctypes.c_uint32 * n)()
    ca.AudioObjectGetPropertyData(K_SYSTEM, ctypes.byref(addr), 0, None, ctypes.byref(size), arr)
    out = []
    for i in range(n):
        d = arr[i]
        # 有输出流才算输出设备
        sa = Addr(fourcc('stm#'), int.from_bytes(b'outp', 'big'), 0)
        s2 = ctypes.c_uint32(0)
        if ca.AudioObjectGetPropertyDataSize(d, ctypes.byref(sa), 0, None, ctypes.byref(s2)) == 0 and s2.value > 0:
            out.append((d, dev_name(d)))
    return out


def get_default():
    addr = Addr(fourcc('dOut'), SCOPE_GLOBAL, 0)
    dev = ctypes.c_uint32(0); size = ctypes.c_uint32(4)
    ca.AudioObjectGetPropertyData(K_SYSTEM, ctypes.byref(addr), 0, None,
                                  ctypes.byref(size), ctypes.byref(dev))
    return dev.value


def set_default(d):
    addr = Addr(fourcc('dOut'), SCOPE_GLOBAL, 0)
    v = ctypes.c_uint32(d)
    return ca.AudioObjectSetPropertyData(K_SYSTEM, ctypes.byref(addr), 0, None, 4, ctypes.byref(v)) == 0


if __name__ == '__main__':
    cmd = sys.argv[1] if len(sys.argv) > 1 else 'get'
    if cmd == 'get':
        d = get_default()
        print(f"{d}\t{dev_name(d)}")
    elif cmd == 'list':
        for d, n in output_devices():
            mark = "  ← 默认" if d == get_default() else ""
            print(f"{d}\t{n}{mark}")
    elif cmd == 'set':
        arg = sys.argv[2]
        target = None
        if arg.isdigit():
            target = int(arg)
        else:
            for d, n in output_devices():
                if arg in n:
                    target = d
                    break
        if target is None:
            print(f"找不到输出设备「{arg}」", file=sys.stderr); sys.exit(1)
        if not set_default(target):
            print("切换失败", file=sys.stderr); sys.exit(1)
        print(f"{target}\t{dev_name(target)}")
    else:
        print(__doc__); sys.exit(1)
