#!/usr/bin/env bash
# 列出可用的音频设备索引（avfoundation 输入 / audiotoolbox 输出）
# 设备索引不固定，每次插拔设备后都可能变 —— 跑脚本前先确认。
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

echo "═══════════════ 输入设备（avfoundation，录音/采集用） ═══════════════"
ffmpeg -hide_banner -f avfoundation -list_devices true -i "" 2>&1 \
  | sed -n '/AVFoundation audio devices/,$p' | grep -oE '\[[0-9]+\] .*' | sed 's/^/  /'
echo
echo "═══════════════ 输出设备（audiotoolbox，播放用） ═══════════════"
ffmpeg -hide_banner -f lavfi -i anullsrc=r=48000:cl=mono -t 0.05 \
  -f audiotoolbox -list_devices true -y /dev/null 2>&1 \
  | sed -n '/CoreAudio devices/,$p' | grep -oE '\[[0-9]+\].*' | sed 's/^/  /'
echo
echo "提示：同一台虚拟设备在两侧的索引通常不同（例如 豆包 输入=3 / 输出=5）。"
