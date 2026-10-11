#!/bin/bash
# 打包 macOS .app —— 必须走这个脚本，不要直接 tauri build。
#
# 为什么：
#  1. `tauri build` 把项目里的 Info.plist 合并进 .app 之后，**原来的 ad-hoc 签名会失效**
#     （codesign 报 "code has no resources but signature indicates they must be present"）。
#  2. 而 macOS 的**隐私权限（TCC）是绑定代码签名的** —— 签名无效时系统不会弹权限窗，
#     也不会授权，程序只能录到一片静音（不报错、不崩溃）。
#  3. 所以打包后**必须重新签名**。
#
# 签名的选择（按优先级）：
#   a) 环境变量 MIXER_SIGN_ID 指定的身份
#   b) 钥匙串里的 **Developer ID Application** 证书（可公证、可分发给别人）
#   c) 钥匙串里的任意 Apple 代码签名证书（**TCC 身份稳定**，本机不再反复弹权限）
#   d) ad-hoc（-）—— 兜底。缺点：**签名指纹每次都不一样**，
#      而 TCC 权限绑定签名 ⇒ 每次重新打包后麦克风权限都要重新授权一次。
#
# 所以能用真证书就别用 ad-hoc。设 MIXER_SIGN_ID 可以强制指定。
#
# 另外 Info.plist 里的 NSMicrophoneUsageDescription 是**必需**的：
# 缺了它 tccd 会直接拒绝（日志原文：
# "Refusing authorization request for service kTCCServiceMicrophone
#  ... without NSMicrophoneUsageDescription key"）。
set -euo pipefail
cd "$(dirname "$0")"

# 脚本环境里可能没有 cargo（终端才会 source ~/.cargo/env）
if ! command -v cargo >/dev/null 2>&1; then
  # shellcheck disable=SC1091
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
fi
command -v cargo >/dev/null 2>&1 || { echo "找不到 cargo，请先安装 Rust"; exit 1; }

echo "── 1/4 打包 ──"
../../node_modules/.bin/tauri build "$@"

APP="../../target/release/bundle/macos/音效板.app"

# ── 把许可证放进 .app ──
#
# ⚠️ 必须在**签名之前**做。往已签名的 bundle 里加文件会让签名失效，
#    而 TCC 权限绑定签名 —— 结果是装上去麦克风录到全零，且不报错。
#
# 放 Contents/Resources/ 是 macOS 的惯例（许可证要随二进制一起分发）。
REPO_ROOT="$(cd ../.. && pwd)"
mkdir -p "$APP/Contents/Resources"
cp "$REPO_ROOT/LICENSE" "$APP/Contents/Resources/LICENSE"
cp "$REPO_ROOT/THIRD-PARTY-NOTICES.md" "$APP/Contents/Resources/THIRD-PARTY-NOTICES.md"
echo "  已放入 LICENSE 与第三方声明"

echo "── 2/4 重新签名 ──"

pick_identity() {
  # 优先用户显式指定
  if [ -n "${MIXER_SIGN_ID:-}" ]; then echo "$MIXER_SIGN_ID"; return; fi
  local ids
  ids=$(security find-identity -v -p codesigning 2>/dev/null || true)
  # 首选 Developer ID（能公证、能分发）
  local did
  did=$(printf '%s\n' "$ids" | grep -o '"Developer ID Application: [^"]*"' | head -1 | tr -d '"')
  if [ -n "$did" ]; then echo "$did"; return; fi
  # 退而求其次：任意 Apple 开发/分发证书（本机 TCC 身份稳定）
  local any
  any=$(printf '%s\n' "$ids" | grep -oE '"(Apple Development|Apple Distribution|Mac Developer): [^"]*"' | head -1 | tr -d '"')
  if [ -n "$any" ]; then echo "$any"; return; fi
  echo "-"   # ad-hoc 兜底
}

IDENTITY=$(pick_identity)
if [ "$IDENTITY" = "-" ]; then
  echo "  ⚠ 没找到代码签名证书 → 用 ad-hoc。"
  echo "    后果：**每次重新打包后，麦克风权限都要重新授权一次**（TCC 绑定签名）。"
  echo "    想要稳定：在 Xcode 里登录 Apple ID，或从开发者门户申请 'Developer ID Application' 证书。"
  codesign --force --deep --sign - "$APP"
else
  echo "  用证书：$IDENTITY"
  # ⚠️ 强化运行时必须带 entitlements —— 否则**麦克风会被静默拒绝**
  #    （不报错、不弹窗，只是录音全是零，peak 精确等于 -100 dBFS）。
  #    光有 Info.plist 的 NSMicrophoneUsageDescription 不够。
  ENT="$(pwd)/entitlements.plist"
  codesign --force --deep --options runtime \
           --entitlements "$ENT" --sign "$IDENTITY" "$APP"
  echo "  已附加 entitlements: audio-input"
fi

echo "── 3/4 校验 ──"
codesign -v "$APP" && echo "  ✅ 签名有效"
# 把签名指纹打出来 —— 它是 TCC 识别"这是同一个 App"的依据，
# 指纹变了权限就要重新授权。想确认"这次打包会不会又要弹窗"就看它。
CDHASH=$(codesign -dv --verbose=4 "$APP" 2>&1 | grep -o 'CandidateCDHash sha256=[0-9a-f]*' | head -1 | cut -d= -f2 | cut -c1-16)
echo "  ℹ 签名指纹 (CDHash): ${CDHASH:-未知}"  
/usr/libexec/PlistBuddy -c "Print :NSMicrophoneUsageDescription" "$APP/Contents/Info.plist" >/dev/null \
  && echo "  ✅ 麦克风用途说明存在" \
  || { echo "  ❌ 缺少 NSMicrophoneUsageDescription —— 系统会拒绝麦克风权限"; exit 1; }

# 三条目录用途说明：开了强化运行时之后，读 ~/Downloads 这类受保护目录**必须有它**，
# 否则文件读取会**永久阻塞**（不报错、不返回，界面表现为"点了启动没反应"）。
for k in NSDownloadsFolderUsageDescription NSDesktopFolderUsageDescription NSDocumentsFolderUsageDescription; do
  /usr/libexec/PlistBuddy -c "Print :$k" "$APP/Contents/Info.plist" >/dev/null 2>&1 \
    || { echo "  ❌ 缺少 $k —— 开了强化运行时后读受保护目录会卡死"; exit 1; }
done
echo "  ✅ 目录用途说明齐全（强化运行时下读 ~/Downloads 必需）"

# 强化运行时的签名必须带 audio-input，否则麦克风静默失效
if codesign -d --entitlements - "$APP" 2>/dev/null | grep -q "audio-input"; then
  echo "  ✅ entitlements 含 audio-input（强化运行时下麦克风必需）"
else
  echo "  ⚠ 当前签名没有 audio-input entitlement"
  echo "     —— 如果你用的是 ad-hoc 签名，这不影响；用了真证书 + 强化运行时就会录不到声音。"
fi
# ── 4/4 打 DMG ────────────────────────────────────────────────
#
# ⚠️ 顺序很关键：**必须在重新签名之后**做 dmg。
#    如果交给 `tauri build` 顺手打包，dmg 里装的会是签名失效的那个版本
#    （tauri build 合并 Info.plist 之后原签名就废了），用户装上去拿不到麦克风权限。
echo
echo "── 4/4 打 DMG ──"

VERSION=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$APP/Contents/Info.plist" 2>/dev/null || echo "0.0.0")
DMG_DIR="$(cd ../.. && pwd)/target/release/bundle/dmg"
DMG="$DMG_DIR/音效板_${VERSION}.dmg"
mkdir -p "$DMG_DIR"

STAGE=$(mktemp -d)
cp -R "$APP" "$STAGE/"
# 拖拽安装的惯例：旁边放一个 Applications 的快捷方式
ln -s /Applications "$STAGE/Applications"
# 盘上再摆一份，用户不用进 app 包就能看到
cp "$REPO_ROOT/LICENSE" "$STAGE/LICENSE"
cp "$REPO_ROOT/THIRD-PARTY-NOTICES.md" "$STAGE/第三方依赖声明.md"

# 没公证的 App 直接双击会被 Gatekeeper 拦，用户不知道怎么办 —— 写在盘里
cat > "$STAGE/首次打开请看这里.txt" <<'NOTE'
首次打开请这样做
=================

这个 App 没有经过 Apple 公证（公证需要付费的开发者账号），
所以**直接双击会被系统拦住**，提示"无法验证开发者"。

正确做法：
  1. 把「音效板」拖进旁边的 Applications 文件夹
  2. 打开「应用程序」，**按住 Control 键点它 → 选「打开」**
  3. 弹窗里再点一次「打开」
做完这一次，以后就能正常双击启动了（系统会记住）。


还有一件事：这个 App 需要系统里有一个「虚拟声卡」
================================================

游戏和语音软件只能从「麦克风」读声音。虚拟声卡扮演的就是那个麦克风 ——
我们把音效写进去，游戏从它读出来。

macOS 系统自带的没有，所以要装一个。**推荐 BlackHole（免费、开源）**：

    brew install --cask blackhole-2ch

没装 Homebrew 的话，去 https://existential.audio/blackhole/ 下载安装包。
装完**重启一次**。

App 启动后会自己检测：没装的话界面上会直接给出安装命令，点一下就能复制。
NOTE

rm -f "$DMG"
hdiutil create -quiet -volname "音效板 $VERSION" \
  -srcfolder "$STAGE" -ov -format UDZO "$DMG" \
  || { echo "  ❌ 生成 DMG 失败"; rm -rf "$STAGE"; exit 1; }
echo "  已生成: $(basename "$DMG")（$(du -h "$DMG" | cut -f1)）"

# ── 验 DMG ──
# 光看"文件生成了"不算数 —— 要挂起来确认里面的 App 签名**仍然有效**，
# 以及拖拽安装需要的东西都在。
echo
echo "── 验证 DMG ──"
# ⚠️ 别去解析 `hdiutil attach` 的输出 —— 加了 -quiet 它**什么都不打印**，
#    而卷名里带空格（"音效板 0.1.0"）又会让按字段切分变得很脆。
#    直接指定挂载点，稳。
MP=$(mktemp -d)
if ! hdiutil attach -quiet -nobrowse -readonly -mountpoint "$MP" "$DMG"; then
  echo "  ❌ 挂载失败"; rm -rf "$STAGE" "$MP"; exit 1
fi

DMG_OK=1
[ -d "$MP/音效板.app" ] && echo "  ✅ 里面有 App" || { echo "  ❌ 没有 App"; DMG_OK=0; }
[ -f "$MP/音效板.app/Contents/Resources/LICENSE" ] \
  && echo "  ✅ App 包内含 LICENSE" \
  || { echo "  ❌ App 包里没有 LICENSE"; DMG_OK=0; }
[ -L "$MP/Applications" ] && echo "  ✅ 有 Applications 快捷方式（可拖拽安装）" || { echo "  ⚠ 缺 Applications 快捷方式"; DMG_OK=0; }
[ -f "$MP/首次打开请看这里.txt" ] && echo "  ✅ 有首次打开说明" || echo "  ⚠ 缺说明文件"
[ -f "$MP/LICENSE" ] && echo "  ✅ 有 LICENSE（MIT）" || { echo "  ❌ 缺 LICENSE"; DMG_OK=0; }
[ -f "$MP/第三方依赖声明.md" ] && echo "  ✅ 有第三方依赖声明" || { echo "  ❌ 缺第三方依赖声明"; DMG_OK=0; }

# **最关键的一条**：dmg 里的 App 签名必须仍然有效（TCC 权限绑定签名）
if codesign -v "$MP/音效板.app" 2>/dev/null; then
  echo "  ✅ DMG 里的 App 签名有效"
else
  echo "  ❌ DMG 里的 App 签名无效 —— 装上去拿不到麦克风权限"; DMG_OK=0
fi
if codesign -d --entitlements - "$MP/音效板.app" 2>/dev/null | grep -q "audio-input"; then
  echo "  ✅ DMG 里的 App 带 audio-input entitlement"
else
  echo "  ❌ DMG 里的 App 缺 audio-input entitlement"; DMG_OK=0
fi
if /usr/libexec/PlistBuddy -c "Print :NSMicrophoneUsageDescription" \
     "$MP/音效板.app/Contents/Info.plist" >/dev/null 2>&1; then
  echo "  ✅ DMG 里的 Info.plist 用途说明齐全"
else
  echo "  ❌ DMG 里的 Info.plist 缺用途说明"; DMG_OK=0
fi

hdiutil detach -quiet "$MP" 2>/dev/null
rm -rf "$STAGE" "$MP"
[ "$DMG_OK" = "1" ] || { echo "  ❌ DMG 校验未通过"; exit 1; }

echo
if [ "$IDENTITY" != "-" ]; then
  echo "  💡 用真证书签名 ⇒ **TCC 权限绑定这个身份，重新打包不会重置**，"
  echo "     麦克风授权只需给一次。"
else
  echo "  💡 现在是 ad-hoc 签名 ⇒ 每次重新打包都会换指纹，麦克风权限要重新授权。"
fi
echo
echo "产物:"
echo "  App: $(cd ../.. && pwd)/target/release/bundle/macos/音效板.app"
echo "  DMG: $DMG   ← 把这个发给别人"
