#!/usr/bin/env bash
# 环回标定：把 5 档已标定电平依次送入虚拟设备，录回来后逐段核对真实增益。
# 用途：确认虚拟设备是"透明环回"还是"带 AGC/归一化"——后者无法用于电平实验。
# 用法: ./loopback_verify.sh <输入索引> <输出索引> <素材前缀> [标签]
#   素材前缀: 音乐 | 语音
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

IN_IDX="$1"; OUT_IDX="$2"; PREFIX="$3"; LABEL="${4:-$PREFIX}"
TOOLS="$(cd "$(dirname "$0")" && pwd)"; W="$TOOLS/../work"
LEVELS=(30 24 18 12 06)
GAP=2

TOTAL=$(( 3 + ${#LEVELS[@]} * 10 + (${#LEVELS[@]} - 1) * GAP + 2 ))
REC="$W/verify_${LABEL}.wav"

echo "▶ 录音 ${TOTAL}s（前 3s 静音），随后依次播放 5 档电平，间隔 ${GAP}s"
ffmpeg -hide_banner -loglevel error -f avfoundation -i ":$IN_IDX" -t "$TOTAL" -ac 2 -ar 48000 -y "$REC" &
RPID=$!
sleep 3
for lv in "${LEVELS[@]}"; do
  printf "  ▸ 播放 -%s dBFS\n" "$lv"
  ffmpeg -hide_banner -loglevel error -i "$W/m0_${PREFIX}_${lv}dBFS.wav" \
    -f audiotoolbox -audio_device_index "$OUT_IDX" -y "$W/_sink" 2>/dev/null
  sleep "$GAP"
done
wait $RPID
python3 "$TOOLS/analyze_segments.py" "$REC" "$PREFIX" "$LABEL"
