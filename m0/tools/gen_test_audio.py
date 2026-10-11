#!/usr/bin/env python3
"""M0 测试素材生成器 —— 精确控制 RMS 电平，自带峰值限幅。

为什么不用 ffmpeg 的 sine 滤波器：
    ffmpeg `sine` 生成的是峰值 0.125 的波形（RMS -21 dBFS），不是满幅。
    用它做电平实验会得到完全错误的参考值。这里用 numpy 自己合成并标定。

用法：
    python3 gen_test_audio.py synth                 # 合成"音乐"素材（推荐首选）
    python3 gen_test_audio.py speech                # 用 macOS say 生成真人语音（对照组）
    python3 gen_test_audio.py file <你的音乐文件>    # 用你自己的音频，最真实

输出：../work/m0_{音乐|语音}_{前缀}_{电平}dBFS.wav
"""
import os
import subprocess
import sys
import wave

import numpy as np

SR = 48000
DUR = 10.0                      # 每个文件 10 秒
LEVELS = [-30, -24, -18, -12, -6]
CEILING_DB = -1.0               # 限幅上限
WORK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "work")


# ---------------------------------------------------------------- 工具

def rms_db(x):
    return 20 * np.log10(np.sqrt(np.mean(x.astype(np.float64) ** 2)) + 1e-12)


def peak_db(x):
    return 20 * np.log10(np.max(np.abs(x)) + 1e-12)


def limit(x, ceiling_db=CEILING_DB, lookahead_ms=2.0, release_ms=80.0):
    """前视峰值限幅器（模拟真实产品里的处理，保证不削波）。支持单声道与立体声。"""
    ceil = 10 ** (ceiling_db / 20)
    la = max(1, int(SR * lookahead_ms / 1000))
    stereo = x.ndim == 2
    # 用各声道最大值构造包络，保证两声道共用同一增益（不破坏声像）
    mono = np.max(np.abs(x), axis=1) if stereo else np.abs(x)
    n = len(mono)
    padded = np.concatenate([mono, np.full(la, mono[-1])])
    env = np.maximum.reduce([padded[k:k + n] for k in range(la)])   # 正确的前视
    target = np.minimum(1.0, ceil / (env + 1e-9))
    # 释放平滑（增益下降要快、恢复要慢）
    a = np.exp(-1.0 / (SR * release_ms / 1000))
    gain = np.empty(n)
    g = 1.0
    for i in range(n):
        g = min(target[i], g * a + target[i] * (1 - a))
        gain[i] = g
    return (x * gain[:, None] if stereo else x * gain), gain


def normalize_to(x, target_db, apply_limit=True):
    """标定到指定 RMS（dBFS），必要时限幅并迭代补偿"""
    y = x / (np.sqrt(np.mean(x.astype(np.float64) ** 2)) + 1e-12)
    y *= 10 ** (target_db / 20)
    for _ in range(4):
        if apply_limit and peak_db(y) > CEILING_DB:
            y, _ = limit(y)
        cur = rms_db(y)
        if abs(cur - target_db) < 0.05:
            break
        y *= 10 ** ((target_db - cur) / 20)
    return y


def write_wav(path, x):
    """x: (N,) 或 (N,2) float  → 16bit PCM"""
    if x.ndim == 1:
        x = np.stack([x, x], axis=1)
    data = np.clip(x, -1.0, 1.0)
    pcm = (data * 32767.0).astype("<i2")
    with wave.open(path, "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(pcm.tobytes())


# ---------------------------------------------------------------- 合成"音乐"

def _env(n, attack, decay):
    a = np.linspace(0, 1, max(1, int(n * attack)))
    d = np.exp(-np.linspace(0, 6, n - len(a)))
    return np.concatenate([a, d])[:n]


def synth_music(dur=DUR, bpm=100, seed=7):
    """合成一段有鼓组、贝斯、和弦垫的音乐 —— 和弦垫是降噪算法的重点攻击对象"""
    rng = np.random.default_rng(seed)
    n = int(SR * dur)
    out = np.zeros(n)
    beat = 60.0 / bpm
    eighth = beat / 2

    def add(sig, t):
        i = int(t * SR)
        j = min(n, i + len(sig))
        if i < n:
            out[i:j] += sig[: j - i]

    # 和弦进行 Am - F - C - G（每小节 4 拍）
    chords = [
        [220.00, 261.63, 329.63],   # Am
        [174.61, 220.00, 261.63],   # F
        [261.63, 329.63, 392.00],   # C
        [196.00, 246.94, 293.66],   # G
    ]

    bar = 0.0
    while bar < dur:
        ch = chords[int(bar / (beat * 4)) % 4]
        # 和弦垫：慢起音、长延续 —— 稳态成分，最容易被当成噪声
        pad_len = int(beat * 4 * SR)
        t = np.arange(pad_len) / SR
        pad = sum(np.sin(2 * np.pi * f * t) for f in ch) / len(ch)
        add(pad * _env(pad_len, 0.25, 0.0) * 0.35, bar)
        # 贝斯
        for k in range(4):
            bl = int(beat * SR * 0.9)
            tt = np.arange(bl) / SR
            b = np.sin(2 * np.pi * ch[0] / 2 * tt) + 0.3 * np.sin(4 * np.pi * ch[0] / 2 * tt)
            add(b * _env(bl, 0.01, 0.0) * 0.5, bar + k * beat)
        # 鼓组
        for k in range(8):
            tt0 = bar + k * eighth
            if k % 4 in (0, 2) and (k // 2) % 2 == 0 or k == 0:
                kl = int(0.25 * SR)
                tt = np.arange(kl) / SR
                f = 130 * np.exp(-tt * 22) + 45
                add(np.sin(2 * np.pi * np.cumsum(f) / SR) * _env(kl, 0.002, 0.0) * 0.9, tt0)
            if k % 4 in (1, 3):
                sl = int(0.20 * SR)
                add(rng.normal(0, 1, sl) * _env(sl, 0.001, 0.0) * 0.35, tt0)
            hl = int(0.06 * SR)
            h = rng.normal(0, 1, hl)
            h = np.diff(np.concatenate([[0], h]))       # 简易高通
            add(h * _env(hl, 0.001, 0.0) * 0.18, tt0)
        bar += beat * 4

    # 轻微立体声展宽
    d = int(0.004 * SR)
    left = out.copy()
    right = np.concatenate([np.zeros(d), out[:-d]])
    st = np.stack([left * 0.92 + right * 0.08, right * 0.92 + left * 0.08], axis=1)
    # 首尾 30ms 淡入淡出
    f = int(0.03 * SR)
    for ch in range(2):
        st[:f, ch] *= np.linspace(0, 1, f)
        st[-f:, ch] *= np.linspace(1, 0, f)
    return st


# ---------------------------------------------------------------- 语音

SPEECH_TEXT = (
    "喂，能听到吗？现在测试的是语音链路。"
    "这一段是正常说话的音量，如果队友听得到这一句，说明人声是通的。"
    "接下来我们会把音乐一起送过去，看看会不会被降噪当成噪音处理掉。"
    "一二三四五六七八九十，测试结束。"
)


VOICE_PREF = ["Tingting", "Sinji", "Meijia", "Flo (Chinese (China mainland))",
              "Eddy (Chinese (China mainland))", "Reed (Chinese (China mainland))"]


def _pick_voice():
    try:
        listing = subprocess.run(["say", "-v", "?"], capture_output=True, text=True).stdout
    except Exception:
        return None
    for v in VOICE_PREF:
        if v.split(" (")[0] in listing:
            return v
    return None


def synth_speech():
    """用 macOS 内置 TTS 生成真语音（对照组：降噪应当保留它）"""
    import tempfile
    with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as t:
        tmp = t.name
    os.unlink(tmp)                       # say 自己要创建这个文件
    cmd = ["say", "-o", tmp, "--data-format=LEI16@48000"]
    v = _pick_voice()
    if v:
        cmd += ["-v", v]
    cmd.append(SPEECH_TEXT)
    subprocess.run(cmd, check=True)
    raw = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", tmp, "-f", "f32le", "-ac", "1", "-ar", str(SR), "-"],
        capture_output=True, check=True).stdout
    os.unlink(tmp)
    x = np.frombuffer(raw, dtype=np.float32).astype(np.float64)
    # 循环/裁剪到 DUR
    if len(x) < int(SR * DUR):
        reps = int(np.ceil(SR * DUR / len(x)))
        x = np.tile(x, reps)
    x = x[: int(SR * DUR)]
    f = int(0.03 * SR)
    x[:f] *= np.linspace(0, 1, f)
    x[-f:] *= np.linspace(1, 0, f)
    return x


# ---------------------------------------------------------------- 主流程

def emit(sig, prefix, outdir):
    print(f"\n{'='*70}\n素材: {prefix}    原始 RMS {rms_db(sig):.2f} dBFS  峰值 {peak_db(sig):.2f} dBFS\n{'='*70}")
    results = []
    for lv in LEVELS:
        y = normalize_to(sig if sig.ndim == 1 else sig, lv)
        path = os.path.join(outdir, f"m0_{prefix}_{abs(lv):02d}dBFS.wav")
        write_wav(path, y)
        # 回读验证
        with wave.open(path) as w:
            rb = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2").astype(np.float64) / 32768.0
        if w.getnchannels() == 2:
            rb = rb.reshape(-1, 2)
        results.append((os.path.basename(path), lv, rms_db(rb), peak_db(rb)))
    print(f"{'文件':<40} {'目标':>8} {'实测RMS':>9} {'峰值':>8}")
    for name, tgt, r, p in results:
        flag = "✅" if abs(r - tgt) < 0.3 else "⚠️"
        print(f"{name:<40} {tgt:>7} dB {r:>8.2f} dB {p:>7.2f} dB  {flag}")
    return results


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)
    os.makedirs(WORK, exist_ok=True)
    mode = sys.argv[1]
    if mode == "synth":
        emit(synth_music(), "音乐", WORK)
    elif mode == "speech":
        emit(synth_speech(), "语音", WORK)
    elif mode == "file":
        if len(sys.argv) < 3:
            sys.exit("用法: gen_test_audio.py file <音频文件>")
        src = sys.argv[2]
        raw = subprocess.run(
            ["ffmpeg", "-v", "error", "-i", src, "-t", str(DUR),
             "-f", "f32le", "-ac", "2", "-ar", str(SR), "-"],
            capture_output=True, check=True).stdout
        x = np.frombuffer(raw, dtype=np.float32).astype(np.float64).reshape(-1, 2)
        if len(x) < int(SR * DUR):
            x = np.tile(x, (int(np.ceil(SR * DUR / len(x))), 1))[: int(SR * DUR)]
        emit(x, "自选", WORK)
    else:
        print(__doc__)
        sys.exit(1)


if __name__ == "__main__":
    main()
