#!/usr/bin/env python3
"""从环回录音里找出**测试音**的主频（供 app_verify.sh 用）。

⚠️ 关键：必须挑**频谱幅度最大**的那个窗口，不能挑"最后一个满足条件的"。

早期的版本写的是 `if pk > 400: best = pk` —— 那是拿**频率**当筛选条件，
于是最后匹配到的窗口胜出。而麦克风是开着的，房间里的人声/键盘噪声
（振幅 10~36）和测试音（振幅 400~600）**都会满足这个条件**，
结果读到的是噪声的频点，每次还不一样（实测读到过 738 / 1183 / 937 Hz）。

正确做法：比幅度。测试音的幅度比环境噪声高一到两个数量级。
"""
import sys
import wave

import numpy as np


def main(path: str) -> None:
    try:
        with wave.open(path) as w:
            sr, nch = w.getframerate(), w.getnchannels()
            x = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2")
            x = x.astype(np.float64) / 32768.0
    except Exception:
        print("0.0")
        return

    m = x.reshape(-1, nch).mean(axis=1)
    h = int(0.25 * sr)
    best_mag, best_freq = 0.0, 0.0
    for i in range(0, len(m) - h, h):
        seg = m[i:i + h]
        if 20 * np.log10(np.sqrt(np.mean(seg ** 2)) + 1e-12) < -48:
            continue
        s = seg - seg.mean()
        n = 1 << int(np.log2(len(s)))
        sp = np.abs(np.fft.rfft(s[:n] * np.hanning(n)))
        fr = np.fft.rfftfreq(n, 1 / sr)
        band = (fr > 100) & (fr < 3000)
        if not band.any():
            continue
        j = int(np.argmax(sp[band]))
        mag = float(sp[band][j])
        # 比**幅度**，不是比频率 —— 见文件头注释
        if mag > best_mag:
            best_mag, best_freq = mag, float(fr[band][j])
    print(f"{best_freq:.1f}")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "")
