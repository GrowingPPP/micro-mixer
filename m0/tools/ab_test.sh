#!/usr/bin/env bash
# M0 实验 2：降噪阈值 A/B 测试
#
# 依次把 5 档已知电平送入虚拟麦克风，同时录下环回以核对真实送达电平。
# 队友在语音软件里逐档报告"能否听到 / 是否清楚"。
#
# 用法: ./ab_test.sh <虚拟设备输入索引> <虚拟设备输出索引> <音乐|语音> <会话标签>
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

IN_IDX="$1"; OUT_IDX="$2"; PREFIX="$3"; LABEL="$4"
TOOLS="$(cd "$(dirname "$0")" && pwd)"; W="$TOOLS/../work"
LEVELS=(30 24 18 12 06)
LEAD=3; GAP=5
TOTAL=$(( LEAD + ${#LEVELS[@]} * 10 + (${#LEVELS[@]} - 1) * GAP + 3 ))
REC="$W/ab_${LABEL}.wav"

cat <<EOF
╔══════════════════════════════════════════════════════════════════╗
║  M0 实验 2 · 降噪阈值 A/B 测试                                    ║
╠══════════════════════════════════════════════════════════════════╣
║  素材: ${PREFIX}    标签: ${LABEL}
║  共 5 档，每档 10 秒，档间静音 ${GAP} 秒，全程约 ${TOTAL} 秒
║
║  ⚠️  测试期间你的麦克风被虚拟设备接管 —— 你说的话传不出去。
║      请用【文字聊天】告诉队友当前是第几档。
║
║  队友每档只需回答两点：① 听得到吗？ ② 清楚吗？(1-5 分)
╚══════════════════════════════════════════════════════════════════╝

EOF
read -r -p "按回车开始（或 Ctrl-C 取消）… " _

echo "▶ 开始录音（前 ${LEAD} 秒为静音）"
ffmpeg -hide_banner -loglevel error -f avfoundation -i ":$IN_IDX" -t "$TOTAL" -ac 2 -ar 48000 -y "$REC" &
RPID=$!
sleep $LEAD

for i in "${!LEVELS[@]}"; do
  lv="${LEVELS[$i]}"
  echo
  echo "┌──────────────────────────────────────────────┐"
  printf "│  ▶▶▶  第 %d / %d 档   ·   电平 -%s dBFS        │\n" $((i+1)) ${#LEVELS[@]} "$lv"
  echo "└──────────────────────────────────────────────┘"
  echo "     ↑ 现在告诉队友：第 $((i+1)) 档（用文字聊天）"
  ffmpeg -hide_banner -loglevel error -i "$W/m0_${PREFIX}_${lv}dBFS.wav" \
    -f audiotoolbox -audio_device_index "$OUT_IDX" -y "$W/_sink" 2>/dev/null
  [ $i -lt $((${#LEVELS[@]} - 1)) ] && { echo "     （静音 ${GAP} 秒）"; sleep $GAP; }
done

wait $RPID
echo
echo "✅ 播放结束。核对实际送达电平："
python3 "$TOOLS/analyze_segments.py" "$REC" "$PREFIX" "$LABEL"
