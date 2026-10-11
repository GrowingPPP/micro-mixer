#!/bin/bash
# 「双击启动」路径的全功能验证
#
# 为什么必须单独有这么一个测试：
#   本项目踩过一个**只在双击启动时才出现**的致命问题 —— 缺少
#   NSMicrophoneUsageDescription 导致 macOS 拒绝麦克风权限，录音全为零。
#   而从终端直接跑二进制时，TCC 会把权限归因给**终端**（终端本来就有权限），
#   所以所有命令行测试都正常通过，完全掩盖了这个 bug。
#
#   结论：**必须用 `open` 启动来验**，那才是用户真实的使用方式。
#
#   判据里最关键的一条是 peak_db —— 麦克风没权限时它是 -100.0（全零），
#   正常时有真实底噪（约 -50 ~ -60 dBFS）。
#
# 用法: m0/tools/app_verify.sh

set -uo pipefail

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

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

APP="$ROOT/target/release/bundle/macos/音效板.app"
LOG=/tmp/app_verify.log
READY=/tmp/app_verify_ready.txt
LANIP=$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || echo "")

pass=0; fail=0
ok ()   { echo "  ✅ $1"; pass=$((pass+1)); }
bad ()  { echo "  ❌ $1"; fail=$((fail+1)); }

[ -d "$APP" ] || { echo "先打包: apps/mixer-gui/build-app.sh"; exit 1; }

# 等上一个实例**彻底退干净**再启动。
# 不等的话单实例守卫会把新的启动挡掉，脚本会误报一堆失败（实测踩过）。
pkill -f "MacOS/mixer-gui" 2>/dev/null
for _ in $(seq 1 20); do
  pgrep -f "MacOS/mixer-gui" >/dev/null || break
  sleep 0.5
done
sleep 1
rm -f "$LOG" "$READY"

echo "双击路径验证    设备: $LANIP"
echo

# ── 前置检查：这两项缺任何一个，双击后都拿不到麦克风权限
echo "── 打包产物自检 ──"
/usr/libexec/PlistBuddy -c "Print :NSMicrophoneUsageDescription" "$APP/Contents/Info.plist" >/dev/null 2>&1 \
  && ok "Info.plist 有 NSMicrophoneUsageDescription" \
  || bad "Info.plist 缺 NSMicrophoneUsageDescription —— 系统会拒绝麦克风权限"
codesign -v "$APP" 2>/dev/null \
  && ok "代码签名有效（TCC 权限绑定签名）" \
  || bad "代码签名无效 —— 打包后需要重新 ad-hoc 签名"

# ── 用 open 启动（等同双击）
echo
echo "── 启动并观察 ──"
open --stdout "$LOG" --stderr "$LOG" \
     --env MIXER_AUTOSTART=start \
     --env MIXER_READY_FILE="$READY" "$APP"

# ⚠️ 不能只等"前端就绪"文件：App 是先上报就绪、**之后**才绑定热键和启动遥控的，
#    只等就绪文件会让下面的日志检查撞进竞态窗口（表现为随机报"热键未绑定"）。
#    这里等到真正的就绪行出现为止。
for _ in $(seq 1 40); do
  [ -f "$READY" ] && grep -q "卡片" "$READY" 2>/dev/null \
    && grep -q "\[遥控\] 已就绪" "$LOG" 2>/dev/null && break
  sleep 1
done
sleep 1

grep -q "菜单栏" "$LOG" 2>/dev/null && ok "菜单栏图标已创建" || bad "菜单栏图标未创建"
grep -q "前端就绪" "$LOG" 2>/dev/null && ok "前端已渲染（WebView 正常）" || bad "前端未就绪"
grep -q "\[热键\] 绑定成功" "$LOG" 2>/dev/null && ok "全局热键已绑定" || bad "热键未绑定"
grep -q "\[遥控\] 已就绪" "$LOG" 2>/dev/null && ok "手机遥控已启动" || bad "手机遥控未启动"

# ── 遥控接口 + 麦克风电平原位检查
sleep 2
ST=$(curl -s --max-time 3 http://127.0.0.1:8765/status 2>/dev/null)
if [ -n "$ST" ]; then
  ok "本机遥控接口可达"
  PEAK=$(echo "$ST" | python3 -c "import sys,json;print(json.load(sys.stdin).get('peak_db',-999))" 2>/dev/null)
  # 这一条是**权限 bug 的照妖镜**：没权限时麦克风全零 → peak_db 精确等于 -100
  if python3 -c "import sys; sys.exit(0 if float('$PEAK') > -95 else 1)" 2>/dev/null; then
    ok "麦克风有真实信号（peak ${PEAK} dBFS）—— 权限正常"
  else
    bad "麦克风信号为 ${PEAK} dBFS（全零）—— 多半是没有麦克风权限"
  fi
else
  bad "本机遥控接口不可达"
fi

if [ -n "$LANIP" ]; then
  CODE=$(curl -s --max-time 3 -o /dev/null -w '%{http_code}' "http://$LANIP:8765/" 2>/dev/null)
  [ "$CODE" = "200" ] && ok "局域网地址可达（手机用的就是它）HTTP $CODE" \
                      || bad "局域网地址不可达（HTTP ${CODE}）—— 检查防火墙"
fi

pgrep -f "MacOS/mixer-gui" >/dev/null && ok "进程存活" || bad "进程已退出"

# ── 最关键的一条：真的触发一次音效，从环回录音验频
#    （光看"引擎在跑"是不够的 —— 那只是状态，不是声音）
if command -v ffmpeg >/dev/null 2>&1; then
  echo
  echo "── 触发音效并录音验频 ──"
  pkill -f "MacOS/mixer-gui" 2>/dev/null; sleep 1
  REC_IDX=$(avf_index "豆包会议记录虚拟设备")
[ -n "$REC_IDX" ] || { echo "找不到虚拟声卡的录音索引"; exit 1; }
WAV=/tmp/app_verify_audio.wav; rm -f "$WAV"
  ffmpeg -hide_banner -loglevel error -f avfoundation -i ":$REC_IDX" -t 16 \
         -ac 2 -ar 48000 -y "$WAV" >/dev/null 2>&1 &
  REC=$!
  sleep 1
  open --stdout "$LOG" --stderr "$LOG" \
       --env XDG_CONFIG_HOME=/tmp/mixercfg --env MIXER_AUTOSTART=start "$APP"
  for _ in $(seq 1 40); do
    curl -s --max-time 1 -o /dev/null http://127.0.0.1:8765/status 2>/dev/null && break
    sleep 1
  done
  sleep 1
  curl -s --max-time 2 -o /dev/null http://127.0.0.1:8765/play/2   # 槽位2 = 警报 880Hz
  sleep 3
  pkill -f "MacOS/mixer-gui" 2>/dev/null
  wait $REC 2>/dev/null

  FREQ=$(python3 "$(dirname "$0")/app_verify_freq.py" "$WAV")
  if python3 -c "import sys; sys.exit(0 if abs(float('$FREQ')-880)<60 else 1)" 2>/dev/null; then
    ok "触发音效 → 环回录到 ${FREQ} Hz（期望 880）"
  else
    bad "触发音效后环回只录到 ${FREQ} Hz（期望 880）—— 声音没送到虚拟麦克风"
  fi
fi

pkill -f "MacOS/mixer-gui" 2>/dev/null


echo
echo "──────────────────────────────"
echo "通过 $pass / 失败 $fail"
[ "$fail" -eq 0 ] || { echo; echo "启动日志："; sed 's/^/  /' "$LOG"; exit 1; }
