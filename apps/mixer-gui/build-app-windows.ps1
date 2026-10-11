<#
.SYNOPSIS
    在 Windows 上构建「音效板」并打出 NSIS 安装包。

.DESCRIPTION
    对应 macOS 那边的 apps/mixer-gui/build-app.sh。
    两边做的事情一样：编译 → 打包 → **验证产物**。

    音频引擎和界面是两端共用的，所以这边不需要动任何业务代码 ——
    只有打包方式和平台相关的检查不同。

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File apps\mixer-gui\build-app-windows.ps1
#>
[CmdletBinding()]
param(
    # 跳过 NSIS 安装包，只出 exe（调试用，快很多）
    [switch]$NoInstaller,
    # 构建后自动跑一次无窗口冒烟测试
    [switch]$SkipSmoke
)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..\..')
$Root = Get-Location

function Step($n, $msg) { Write-Host "`n── $n $msg ──" }
function Ok($msg)   { Write-Host "  [OK] $msg"   -ForegroundColor Green }
function Bad($msg)  { Write-Host "  [!!] $msg"   -ForegroundColor Red }
function Warn($msg) { Write-Host "  [??] $msg"   -ForegroundColor Yellow }

# ── 0. 环境检查 ────────────────────────────────────────────────
Step '0/4' '环境检查'

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Bad '找不到 cargo。请先装 Rust：https://rustup.rs'
    exit 1
}
Ok "cargo $((cargo --version) -split ' ' | Select-Object -Index 1)"

if (-not (Test-Path 'node_modules')) {
    Warn '没有 node_modules —— 先跑 npm install（只为拿 Tauri CLI）'
    npm install
    if ($LASTEXITCODE -ne 0) { Bad 'npm install 失败'; exit 1 }
}
Ok 'Tauri CLI 依赖就绪'

# WebView2 是 Tauri 在 Windows 上的运行时依赖。
# Win11 自带；Win10 可能没有 —— 装的时候会静默失败，界面一片空白，很难查。
$wv2 = Get-ItemProperty -Path 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}' -ErrorAction SilentlyContinue
if ($null -eq $wv2) {
    Warn '没检测到 WebView2 运行时。界面可能是空白 —— 去下面这个地址装一下：'
    Write-Host '       https://developer.microsoft.com/microsoft-edge/webview2/'
} else {
    Ok "WebView2 运行时已装（$($wv2.pv)）"
}

# ── 1. 构建 ────────────────────────────────────────────────────
Step '1/4' '构建'

$bundles = if ($NoInstaller) { 'app' } else { 'nsis' }
& "$Root\node_modules\.bin\tauri.cmd" build --bundles $bundles
if ($LASTEXITCODE -ne 0) { Bad 'tauri build 失败'; exit 1 }

$exe = Join-Path $Root 'target\release\mixer-gui.exe'
if (-not (Test-Path $exe)) { Bad "没找到产物：$exe"; exit 1 }
Ok "exe：$([math]::Round((Get-Item $exe).Length / 1MB, 1)) MB"

# ── 2. 冒烟测试 ────────────────────────────────────────────────
Step '2/4' '冒烟测试（验证二进制真能跑）'

if ($SkipSmoke) {
    Warn '已跳过'
} else {
    # 用一个临时目录当音效库，避免依赖用户的实际配置
    $sounds = Join-Path $env:TEMP 'mixer_smoke_sounds'
    New-Item -ItemType Directory -Force -Path $sounds | Out-Null

    Write-Host '  跑 --smoke（无窗口，跑通界面调用的全部命令 + 异常路径）…'
    $out = & $exe --smoke --dir $sounds 2>&1 | Out-String
    $passed = ([regex]::Matches($out, '(?m)^\s*✅')).Count
    $failed = ([regex]::Matches($out, '(?m)^\s*❌')).Count

    if ($out -match '❌') {
        Bad "冒烟测试有失败项（通过 $passed / 失败 $failed）"
        $out -split "`n" | Where-Object { $_ -match '❌' } | Select-Object -First 8 | ForEach-Object { Write-Host "     $_" }
        exit 1
    }
    Ok "冒烟测试通过（$passed 项）"
}

# ── 3. 安装包 ──────────────────────────────────────────────────
Step '3/4' '安装包'

if ($NoInstaller) {
    Warn '按参数要求跳过了'
} else {
    $nsis = Get-ChildItem -Path (Join-Path $Root 'target\release\bundle\nsis') -Filter '*.exe' -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if ($null -eq $nsis) {
        Bad '没找到 NSIS 安装包（target\release\bundle\nsis\*.exe）'
        exit 1
    }
    Ok "安装包：$($nsis.Name)（$([math]::Round($nsis.Length / 1MB, 1)) MB）"
    Write-Host "     $($nsis.FullName)"
}

# ── 4. 下一步 ──────────────────────────────────────────────────
Step '4/4' '下一步'

Write-Host @'
  装完之后还需要一件事：**虚拟声卡**。

  游戏和语音软件只能从「麦克风」读声音，虚拟声卡扮演的就是那个麦克风。

    1. 去 https://vb-audio.com/Cable/ 下载 VB-CABLE（个人使用免费）
    2. 解压后**右键 → 以管理员身份运行** VBCABLE_Setup_x64.exe，点 Install Driver
    3. **重启电脑**（驱动要重启才生效）

  然后打开音效板：
    · 「输出到（虚拟声卡）」选 **CABLE Input**
    · 游戏里的麦克风选 **CABLE Output**

  ⚠️ VB-CABLE 的命名是**反的**：本程序输出到 CABLE Input，
     而游戏选麦克风要选 CABLE Output。名字看着别扭，但就是这样。
     程序里如果没检测到虚拟声卡，会直接把这段引导显示出来。

  想先确认整条链路通不通：界面上点「自检」。
'@

Write-Host "`n完成。`n"
