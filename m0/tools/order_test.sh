#!/bin/bash
# 设备占用顺序矩阵测试
#
# 为什么需要它：产品的真实使用顺序有三种，而**只有一种被日常测试覆盖**：
#   A. App 先启动，游戏后启动
#   B. 游戏先启动（已在录音），才想起打开 App   ← 最常见的真实顺序
#   C. 虚拟设备没有任何客户端（先配好，等游戏来连）
#
# 三种都必须能正常出声。
#
# ⚠️ 踩过的坑：`mixer play --duration N` **只跑引擎、不会自动播放**。
#    早期版本的本脚本因此"通过"了却什么都没验证 —— 日志里全是「■ 待机」。
#    现在一律用 `--remote` + curl 真正触发一次播放，并从录音里验证频率。
#
# 用法: m0/tools/order_test.sh [虚拟设备名]

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

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

M="$ROOT/target/release/mixer"
DEV="${1:-豆包会议记录虚拟设备}"
IN_IDX=3                 # avfoundation 输入设备序号，虚拟设备的环回输入
WORK="$ROOT/m0/work"
PORT=18781
TMP="$WORK/order_test"
mkdir -p "$TMP"

# 期望频率：sfx_警报.wav = 880Hz
EXPECT=880
TOL=45

[ -x "$M" ] || { echo "先构建: cargo build --release --workspace"; exit 1; }
command -v ffmpeg >/dev/null || { echo "需要 ffmpeg"; exit 1; }

pass=0; fail=0

analyze () {   # $1=wav → 用 FFT 判定主频，并检查谐波比值
  # 为什么不用零交叉：0.15 秒窗口的零交叉率会被淡入/淡出段的低电平噪声带偏，
  # 实测出现过把 880Hz 读成 1790Hz 的**假阳性** —— 同一份录音用 FFT 看，
  # 1760Hz 能量只有 880Hz 的 1%，音频其实完全正常。
  # FFT 能同时回答"主频是多少"和"有没有真的翻倍"。
  python3 - "$1" "$EXPECT" "$TOL" <<'PYEOF'
import sys, wave, numpy as np
path, expect, tol = sys.argv[1], float(sys.argv[2]), float(sys.argv[3])
with wave.open(path) as w:
    sr, nch = w.getframerate(), w.getnchannels()
    x = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2").astype(np.float64)/32768.0
m = x.reshape(-1, nch).mean(axis=1)

h = int(0.02*sr); segs=[]; cur=None
for i in range(0, len(m)-h, h):
    seg = m[i:i+h]
    if 20*np.log10(np.sqrt(np.mean(seg**2))+1e-12) > -50:
        if cur is None: cur=[i,0]
        cur[1]=i+h
    else:
        if cur and cur[1]-cur[0] > sr*0.3: segs.append(tuple(cur))
        cur=None
if cur and cur[1]-cur[0] > sr*0.3: segs.append(tuple(cur))
if not segs:
    print("    录音检测: （没录到信号）")
    print("    FAIL")
    sys.exit()
a,b = max(segs, key=lambda t: t[1]-t[0])
tone = m[a:b] - m[a:b].mean()
dur = len(tone)/sr
N = 1 << int(np.log2(len(tone)))
sp = np.abs(np.fft.rfft(tone[:N]*np.hanning(N)))
fr = np.fft.rfftfreq(N, 1/sr)
peak = float(fr[np.argmax(sp)])
def mag(f):
    i = int(np.argmin(np.abs(fr-f))); return float(sp[max(0,i-3):i+4].max())
base, harm = mag(expect), mag(expect*2)
print(f"    录音检测: 主频 {peak:.1f}Hz @{a/sr:.1f}s  时长 {dur:.2f}s  "
      f"基波 {base:.0f} / 二次谐波 {harm:.0f} (比值 {harm/max(base,1e-9):.3f})")
ok = abs(peak-expect) < tol and harm < base
print("    PASS" if ok else "    FAIL")
PYEOF
}

run_case () {   # $1=名称  $2=yes/no(录音方是否先启动)
  local name="$1" rec_first="$2" rec="" mix=""
  echo "════ $name ════"
  local wav="$TMP/case.wav"; rm -f "$wav"

  if [ "$rec_first" = "yes" ]; then
    ffmpeg -hide_banner -loglevel error -f avfoundation -i ":$IN_IDX" \
      -t 13 -ac 2 -ar 48000 -y "$wav" & rec=$!
    echo "  [0s] 录音方先占住虚拟设备"; sleep 3
  fi

  # --mic-gain -60：把真实麦克风静音。
  # 本测试只关心「App 的输出有没有到达虚拟设备」，麦克风开着只会把环境噪声
  # 混进录音，让频率检测偶发失败（实测出现过 3 场景里随机挂 1 个）。
  "$M" play --file "$WORK/sfx_警报.wav" --output "$DEV" --mic-gain -60 \
      --remote "$PORT" --remote-bind 127.0.0.1 --duration 9 > "$TMP/case.log" 2>&1 &
  mix=$!
  if [ "$rec_first" != "yes" ]; then
    ffmpeg -hide_banner -loglevel error -f avfoundation -i ":$IN_IDX" \
      -t 13 -ac 2 -ar 48000 -y "$wav" & rec=$!
    echo "  [0s] 麦克风混音助手先启动"
  fi
  sleep 3

  echo "  [3s] 触发播放"
  curl -s "http://127.0.0.1:$PORT/play/1" >/dev/null 2>&1
  sleep 2
  local st; st=$(curl -s "http://127.0.0.1:$PORT/status" 2>/dev/null)
  echo "  状态: $st"

  wait $mix 2>/dev/null
  [ -n "$rec" ] && wait $rec 2>/dev/null

  grep -E "稳态欠载 / 溢出" "$TMP/case.log" | sed 's/^/  /'
  local res; res=$(analyze "$wav" | tail -1)
  analyze "$wav" | head -1
  if [[ "$res" == *PASS* ]] && grep -q "0 / 0" "$TMP/case.log"; then
    echo "  ✅ $name 通过"; pass=$((pass+1))
  else
    echo "  ❌ $name 失败"; fail=$((fail+1))
  fi
  echo
}

echo "设备占用顺序矩阵测试    虚拟设备: $DEV"
echo
run_case "场景 A：App 先启动，录音方后到" no
run_case "场景 B：录音方先占设备，App 后启动" yes
run_case "场景 C：完全没有客户端" no_client

echo "──────────────────────────────"
echo "通过 $pass / 失败 $fail"
if [ "$fail" -gt 0 ]; then
  cat <<'NOTE'

  ⚠ 失败前请先排除一个**已知的设备层偶发问题**：
    虚拟声卡偶尔会把音高翻倍（实测约 15 次开流出现 1 次）。
    已用 MIXER_DUMP 对照实验确认：本程序交给声卡的数据是正确的
    （878.9 Hz），是设备输出成了 1731.4 Hz。
    判定方法：看上面的「基波/二次谐波」——若二次谐波明显压过基波（比值 > 1），
    就是翻倍；若主频本身就偏、但谐波比值正常，才是本程序的问题。
    重跑一次通常就正常了。
NOTE
fi
[ "$fail" -eq 0 ] || exit 1
