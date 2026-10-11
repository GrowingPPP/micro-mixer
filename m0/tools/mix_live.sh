#!/usr/bin/env bash
# M0 实验 3：实时混音验证（用 ffmpeg 搭出的最小可用原型）
#
# 真实麦克风 + 音效文件 实时混音 → 虚拟麦克风。
# 这就是产品的核心功能，只不过用 ffmpeg 而不是自研引擎实现的。
# 用途：验证"混音后队友能同时听到人声和音效"，以及人声会不会被音效/AGC 压掉。
#
# 用法: ./mix_live.sh <真实麦克风索引> <虚拟设备输出索引> <音频文件> [麦克风增益dB] [音效增益dB]
set -u

# 按名字查出 avfoundation 的音频设备索引。
#
# ⚠️ 千万不要硬编码 ":3" 这类位置索引 —— 插拔一个设备（比如拔掉 USB 耳机）
#    就会让后面所有设备的编号整体前移，录音会静默录到**别的设备**上，
#    表现为"明明在播却什么也没录到"。实测踩过。
avf_index() {
  local name="$1"
  ffmpeg -hide_banner -f avfoundation -list_devices true -i "" 2>&1 \
    | grep -F "$name" \
    | grep -oE '\[[0-9]+\]' | head -1 | tr -d '[]'
}

MIC_IDX="$1"; OUT_IDX="$2"; AUDIO="$3"
MIC_GAIN="${4:-0}"; SFX_GAIN="${5:--6}"

if [ ! -f "$AUDIO" ]; then echo "找不到音频文件: $AUDIO"; exit 1; fi

cat <<EOF
╔══════════════════════════════════════════════════════════════════╗
║  M0 实验 3 · 实时混音（真实麦克风 + 音效）                        ║
╠══════════════════════════════════════════════════════════════════╣
║  麦克风索引 : $MIC_IDX      增益 ${MIC_GAIN} dB
║  输出索引   : $OUT_IDX
║  音效文件   : $AUDIO
║  音效增益   : ${SFX_GAIN} dB
║
║  音效会循环播放。请：                                              ║
║   1. 对着麦克风正常说话 —— 问队友能否【同时】听到人声和音效
║   2. 问队友：音效响起时，你的说话声有没有被压低（AGC 抽吸感）
║   3. 试不同音效增益，找到"音乐清楚 + 人声不被压"的平衡点
║
║  Ctrl-C 停止
╚══════════════════════════════════════════════════════════════════╝

EOF

ffmpeg -hide_banner -loglevel warning \
  -f avfoundation -i ":$MIC_IDX" \
  -stream_loop -1 -i "$AUDIO" \
  -filter_complex "\
[0:a]volume=${MIC_GAIN}dB,aresample=48000[mic];\
[1:a]volume=${SFX_GAIN}dB,aresample=48000,pan=stereo|c0=c0|c1=c0[sfx];\
[mic][sfx]amix=inputs=2:duration=first:dropout_transition=0:normalize=0[mix];\
[mix]alimiter=level_in=1:level_out=1:limit=0.891:attack=5:release=80[out]" \
  -map "[out]" -f audiotoolbox -audio_device_index "$OUT_IDX" -y /dev/null
