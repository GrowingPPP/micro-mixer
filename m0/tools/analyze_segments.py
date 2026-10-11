#!/usr/bin/env python3
"""校验虚拟设备的环回透明性：自动切出每次播放，逐档核对真实增益。

切段方式：播放之间的间隙是**真正的数字静音**（-240 dBFS），
用它做分隔符远比对音乐做阈值分段可靠——音乐自身的动态不会被误判成间隙，
录音启动延迟也不影响结果。

用法: analyze_segments.py <录音> <素材前缀> <标签>
"""
import os
import sys
import wave

import numpy as np

LEVELS = [30, 24, 18, 12, 6]
WORK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "work")


def load_all(path):
    """返回 (N, ch) float 数组 —— 不平均声道（素材含 4ms 延时，平均会产生梳状抵消）"""
    with wave.open(path) as w:
        nch, sr = w.getnchannels(), w.getframerate()
        x = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2").astype(np.float64) / 32768.0
    return x.reshape(-1, nch), sr


def rms_db(x):
    """对整段（含所有声道）求 RMS，与生成端口径一致"""
    return 20 * np.log10(np.sqrt(np.mean(x ** 2)) + 1e-12)


def find_playbacks(mono, sr, silence_db=-75.0, min_gap=0.5, min_len=2.0):
    """用数字静音切出各次播放"""
    frame = int(0.02 * sr)
    n = len(mono) // frame
    if n == 0:
        return []
    env = np.abs(mono[: n * frame]).reshape(n, frame).max(axis=1)
    active = 20 * np.log10(env + 1e-12) > silence_db
    regions, start = [], None
    for i, a in enumerate(active):
        if a and start is None:
            start = i
        elif not a and start is not None:
            regions.append([start * frame, i * frame])
            start = None
    if start is not None:
        regions.append([start * frame, n * frame])
    merged = []
    for s, e in regions:
        if merged and s - merged[-1][1] < sr * min_gap:
            merged[-1][1] = e
        else:
            merged.append([s, e])
    return [(s, e) for s, e in merged if e - s > sr * min_len]


def main():
    rec_path, prefix, label = sys.argv[1], sys.argv[2], sys.argv[3]
    rec, sr = load_all(rec_path)
    mono = rec.mean(axis=1) if rec.shape[1] == 2 else rec[:, 0]
    print(f"\n{'='*76}\n环回标定: {label}    录音 {len(rec)/sr:.1f}s / {rec.shape[1]}ch / {sr}Hz\n{'='*76}")

    truths = {}
    for lv in LEVELS:
        p = os.path.join(WORK, f"m0_{prefix}_{lv:02d}dBFS.wav")
        if os.path.exists(p):
            s, _ = load_all(p)
            truths[lv] = rms_db(s)
    order = [lv for lv in LEVELS if lv in truths]

    regs = find_playbacks(mono, sr)
    print(f"检测到 {len(regs)} 次播放（期望 {len(order)} 次）\n")
    print(f"{'#':>2} {'素材真值':>10} {'环回录得':>10} {'增益':>8}  {'实际播放窗口':>20} {'时长':>6}  判定")
    gains = []
    for i, (s, e) in enumerate(regs):
        got, dur = rms_db(rec[s:e]), (e - s) / sr
        if i < len(order):
            g = got - truths[order[i]]
            gains.append(g)
            ok = "✅ 透明" if abs(g) < 1.0 else ("⚠️ 衰减" if g < 0 else "⚠️ 增益")
            print(f"{i+1:>2} {truths[order[i]]:>9.1f} dB {got:>9.1f} dB {g:>+7.1f} dB  "
                  f"{s/sr:>8.2f}-{e/sr:<9.2f}s {dur:>5.1f}s  {ok}")
        else:
            print(f"{i+1:>2} {'?':>10} {got:>9.1f} dB {'?':>8}  {s/sr:>8.2f}-{e/sr:<9.2f}s {dur:>5.1f}s")

    if regs and regs[0][0] > sr:
        noise = rms_db(rec[: regs[0][0]])
        print(f"\n播放前静音底噪: {noise:.1f} dBFS", end="  ")
        print("✅ 干净" if noise < -70 else "⚠️ 偏高，可能有其他程序在写该设备")

    if gains:
        spread = max(gains) - min(gains)
        avg = sum(gains) / len(gains)
        print(f"\n平均增益 {avg:+.2f} dB   增益极差 {spread:.2f} dB   覆盖动态范围 {max(order)-min(order)} dB")
        if spread < 1.5 and len(gains) >= 4:
            print("✅ 判定：**透明线性环回** —— 送入电平即送达电平，可用于 M0 电平实验")
        else:
            print("⚠️ 判定：增益不一致 —— 检查削波 / 播放中断 / 设备内处理")


if __name__ == "__main__":
    main()
