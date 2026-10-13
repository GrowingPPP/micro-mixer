#!/bin/bash
# 一条命令跑完所有能在本机做的检查。
#
# 分三层，从便宜到贵 —— 前面挂了就不必往后跑：
#   ① 静态：格式 / lint / 配置 schema
#   ② 逻辑：单元测试（含 Windows 分支的）
#   ③ 端到端：冒烟 / 自检 / 双击路径 / Windows 交叉编译
#
# 用法：
#   tools/check-all.sh            # 全跑
#   tools/check-all.sh --fast     # 跳过端到端（不碰音频设备，几秒钟）
set -uo pipefail
cd "$(dirname "$0")/.."

FAST=0
[ "${1:-}" = "--fast" ] && FAST=1
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

FAILED=0
pass() { printf "  ✅ %-22s %s\n" "$1" "${2:-}"; }
fail() { printf "  ❌ %-22s %s\n" "$1" "${2:-}"; FAILED=$((FAILED + 1)); }
step() { printf "\n── %s ──\n" "$1"; }

# ── ① 静态 ──
step "静态检查"

if cargo fmt --all --check >/dev/null 2>&1; then
  pass "格式"
else
  fail "格式" "跑 cargo fmt --all"
fi

CLIPPY_STATUS=0
CLIPPY_OUT=$(cargo clippy --workspace --all-targets -- -D warnings 2>&1) || CLIPPY_STATUS=$?
CLIPPY_N=$(echo "$CLIPPY_OUT" | grep -cE "^(warning|error)")
if [ "$CLIPPY_STATUS" -eq 0 ] && [ "$CLIPPY_N" -eq 0 ]; then
  pass "clippy" "0 条"
else
  fail "clippy" "$CLIPPY_N 条"
  echo "$CLIPPY_OUT" | grep -E "^(warning|error)" | head -5 | sed 's/^/       /'
fi

# 打包配置：错误只会在**目标平台**构建时才暴露，所以必须提前校验
if CONF_OUT=$(./tools/check-tauri-config.sh 2>&1); then
  pass "打包配置 schema"
else
  fail "打包配置 schema"
  echo "$CONF_OUT" | grep -E "❌|·" | head -6 | sed 's/^/       /'
fi

# ── ② 逻辑 ──
step "单元测试"

TEST_STATUS=0
TEST_OUT=$(cargo test --workspace 2>&1) || TEST_STATUS=$?
TEST_N=$(echo "$TEST_OUT" | grep -E "^test result: ok\." | awk '{s+=$4} END {print s+0}')
TEST_FAIL=$(echo "$TEST_OUT" | grep -cE "^test result: FAILED")
if [ "$TEST_STATUS" -eq 0 ] && [ "$TEST_FAIL" -eq 0 ] && [ "$TEST_N" -gt 0 ]; then
  pass "单元测试" "$TEST_N 个"
else
  fail "单元测试" "有失败项"
  echo "$TEST_OUT" | grep -E "^(failures|error)" -A 4 | head -10 | sed 's/^/       /'
fi

if JS_OUT=$(node --check apps/mixer-gui/dist/main.js && node --check apps/mixer-gui/dist/theme.js && node --check crates/remote/src/control-page.js && node --test tools/test-gui-model.js tools/test-gui-controls.js tools/test-remote-controls.js tools/test-app-packaging.js 2>&1); then
  pass "GUI 状态、控件与映射回归"
else
  fail "GUI 状态、控件与映射回归"
  echo "$JS_OUT" | tail -12
fi

if [ "$FAST" -eq 1 ]; then
  step "已跳过端到端（--fast）"
  echo
  [ "$FAILED" -eq 0 ] && echo "全绿 ✅" || echo "有 $FAILED 项没过 ❌"
  exit "$FAILED"
fi

# ── ③ 端到端 ──
step "端到端（会碰音频设备）"

# 先打包：后面几项跑的都是 .app 里的那份
if ./apps/mixer-gui/build-app.sh >/dev/null 2>&1; then
  pass "打包（签名 + DMG）"
else
  fail "打包（签名 + DMG）" "看 apps/mixer-gui/build-app.sh 的输出"
fi

# 冒烟要一个**有内容的**音效目录 —— 自己生成两个正弦 WAV，
# 不依赖外部工具，也不依赖之前手工留下的临时目录（那个坑吃过一次）
SMOKE_DIR=$(mktemp -d "${TMPDIR:-/tmp}/mixer-check-sounds.XXXXXX")
trap 'rm -rf "$SMOKE_DIR"' EXIT
python3 - "$SMOKE_DIR" <<'PYWAV'
import math, struct, sys, wave, pathlib
out = pathlib.Path(sys.argv[1])
for i, (freq, secs) in enumerate([(440.0, 1.0), (880.0, 0.5)]):
    with wave.open(str(out / f"check{i + 1}.wav"), "w") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(48000)
        frames = bytearray()
        for n in range(int(48000 * secs)):
            v = int(0.3 * 32767 * math.sin(2 * math.pi * freq * n / 48000))
            frames += struct.pack("<hh", v, v)
        w.writeframes(bytes(frames))
PYWAV

SMOKE_STATUS=0
SMOKE_OUT=$(./target/release/mixer-gui --smoke --dir "$SMOKE_DIR" 2>&1) || SMOKE_STATUS=$?
SMOKE_N=$(echo "$SMOKE_OUT" | grep -cE "^✅")
if [ "$SMOKE_STATUS" -eq 0 ] && [ "${SMOKE_N:-0}" -ge 28 ] && echo "$SMOKE_OUT" | grep -q "全部通过；音频播放真实发生"; then
  pass "冒烟" "$SMOKE_N 项"
else
  fail "冒烟" "只过了 ${SMOKE_N:-0} 项"
fi

if timeout 45 ./target/release/mixer selftest 2>&1 | grep -q "✅ 自检"; then
  pass "音频链路自检"
else
  fail "音频链路自检" "跑 ./target/release/mixer selftest 看详情"
fi

if ./target/release/mixer doctor >/dev/null 2>&1; then
  pass "环境体检"
else
  fail "环境体检" "跑 ./target/release/mixer doctor 看详情"
fi

APPV=$(./m0/tools/app_verify.sh 2>&1 | grep -E "^通过")
if echo "$APPV" | grep -q "失败 0"; then
  pass "双击路径" "$APPV"
else
  fail "双击路径" "${APPV:-没拿到结果}"
fi

if WIN_OUT=$(./tools/win-cross-check.sh 2>&1); then
  pass "Windows 交叉编译"
else
  fail "Windows 交叉编译"
  # 子脚本已缩进过自己的报错，这里直接转述它，别再按 ^error 过滤（那会一条都匹配不到）
  echo "$WIN_OUT" | tail -12 | sed 's/^/       /'
fi

# ── 结论 ──
echo
if [ "$FAILED" -eq 0 ]; then
  echo "全绿 ✅"
else
  echo "有 $FAILED 项没过 ❌"
fi

echo
echo "  跑不了的两件事（需要 Windows 机器）："
echo "    · 真实 WASAPI 音频、热键注册、NSIS 安装包构建、托盘行为"
echo "    · 到那边之后：mixer doctor → mixer selftest → 进游戏按热键"
exit "$FAILED"
