#!/bin/bash
# Windows 交叉编译检查（在 macOS 上做 `cargo check --target *-pc-windows-msvc`）。
#
# ## 为什么需要这个脚本
#
# `cargo check --target x86_64-pc-windows-msvc -p mixer-gui` 会卡在
# `tauri-winres` → `embed-resource` → **`llvm-rc`**：Windows 的 exe 要带
# 图标和版本信息，得用资源编译器生成 `.res`。macOS 上没有 `llvm-rc`
# （`brew install llvm` 能装，但那个包很大）。
#
# ## 桩为什么是正当的
#
# `cargo check` **不做链接** —— `.res` 只是通过 `cargo:rustc-link-arg`
# 传给链接器的，check 阶段根本不会用到它的内容。
#
# 所以这里的桩只负责"**让构建脚本别失败**"，好让 cargo 真正去检查
# **Rust 代码本身**能不能为 Windows 编译 —— 那才是我们要的答案。
#
# ⚠️ **这个桩只能用于 `cargo check`，绝不能用于出包。**
#    真的在 Windows 上构建时用的是系统自带的 `rc.exe`，不需要这套东西。
#
# 用法：
#   tools/win-cross-check.sh            # 检查整个 workspace
#   tools/win-cross-check.sh mixer-gui  # 只检查某个 crate
set -uo pipefail
cd "$(dirname "$0")/.."

# 本脚本会被单独调用（见 docs/ai/WORKFLOW.md），而非登录 shell 的 PATH 里通常没有 cargo。
# 与 tools/check-all.sh 取同一份环境；少了这行，下面会把"找不到 cargo"误报成"编译失败"。
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
if ! command -v cargo >/dev/null 2>&1; then
  echo "❌ 找不到 cargo —— 先安装 Rust 工具链，或把 cargo 加入 PATH" >&2
  exit 127
fi

TARGET="${TARGET:-x86_64-pc-windows-msvc}"
STUB_DIR="$(mktemp -d)"
trap 'rm -rf "$STUB_DIR"' EXIT

# ── 假 llvm-rc ──
#
# embed-resource 先用 `llvm-rc /?` 探测它能不能跑，再正式调用。
# 真调用形如： llvm-rc /fo out.res in.rc
# 桩要做的就是：探测返回 0；正式调用时**造一个空的 .res** 再返回 0。
cat > "$STUB_DIR/llvm-rc" <<'STUB'
#!/bin/bash
out=""
while [ $# -gt 0 ]; do
  case "$1" in
    /fo) out="$2"; shift 2 ;;
    *) shift ;;
  esac
done
[ -n "$out" ] && : > "$out"
exit 0
STUB
chmod +x "$STUB_DIR/llvm-rc"

echo "── Windows 交叉编译检查 ──"
echo "  目标: $TARGET"
echo "  说明: 用桩替代 llvm-rc（仅让构建脚本通过；check 不链接，.res 内容用不到）"
echo

if [ $# -gt 0 ]; then
  ARGS=(-p "$1")
  WHAT="crate $1"
else
  ARGS=(--workspace)
  WHAT="整个 workspace"
fi

export PATH="$STUB_DIR:$PATH"
out=$(cargo check --locked --all-targets --target "$TARGET" "${ARGS[@]}" 2>&1)
rc=$?

if [ $rc -eq 0 ]; then
  echo "  ✅ $WHAT 能为 Windows 编译通过"
  echo "$out" | grep -E "^\s+(Checking|Finished)" | tail -3 | sed 's/^/     /'
else
  echo "  ❌ $WHAT 编译失败："
  # cargo 的报错不都以 "error" 开头（目标未安装、依赖拉取失败等），
  # 匹配不到就退回首几行 —— 只说"失败"不说原因等于没报。
  detail=$(echo "$out" | grep -E "^error" -A 6 | head -40)
  [ -n "$detail" ] || detail=$(echo "$out" | head -20)
  echo "$detail" | sed 's/^/     /'
fi
exit $rc
