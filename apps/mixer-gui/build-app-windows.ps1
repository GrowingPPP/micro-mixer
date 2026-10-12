<#
.SYNOPSIS
    在 Windows 上构建「麦克风混音助手」并打出 NSIS 安装包。

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
    [switch]$SkipSmoke,
    # 只验证 WAV fixture 和冒烟结果门禁（无需编译、设备或虚拟声卡）
    [switch]$TestSmokeHarness
)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..\..')
$Root = Get-Location

function Step($n, $msg) { Write-Host "`n── $n $msg ──" }
function Ok($msg)   { Write-Host "  [OK] $msg"   -ForegroundColor Green }
function Bad($msg)  { Write-Host "  [!!] $msg"   -ForegroundColor Red }
function Warn($msg) { Write-Host "  [??] $msg"   -ForegroundColor Yellow }

function Get-TauriProductName($Config) {
    $name = $Config.productName
    if ($name -isnot [string] -or [string]::IsNullOrWhiteSpace($name) -or
        $name -match '[<>:"/\\|?*\x00-\x1f]') {
        throw 'tauri.conf.json 的 productName 必须是有效的跨平台文件名'
    }
    return $name
}

function Find-NsisInstaller([string]$Directory, [string]$ProductName, [string]$Version) {
    # 精确匹配本次产品名及版本，避免改名后把旧安装包误判为本次产物。
    return Get-ChildItem -LiteralPath $Directory -Filter "${ProductName}_${Version}_*-setup.exe" -File -ErrorAction SilentlyContinue |
           Sort-Object LastWriteTime -Descending | Select-Object -First 1
}

$tauriConfig = Get-Content -Raw -Encoding UTF8 (Join-Path $PSScriptRoot 'tauri.conf.json') | ConvertFrom-Json
$AppName = Get-TauriProductName $tauriConfig

function Get-TauriBuildArguments([bool]$WithoutInstaller) {
    if ($WithoutInstaller) { return @('build', '--no-bundle') }
    return @('build', '--bundles', 'nsis')
}

function New-SmokeWave([string]$Path) {
    # 两秒 440 Hz / -14 dBFS 的 48 kHz 立体声 PCM WAV。
    # 超过 smoke 进度检查所需的 0.8 秒，且电平足够覆盖真实播放检查。
    $rate = 48000
    $frames = $rate * 2
    $channels = 2
    $dataBytes = $frames * $channels * 2
    $stream = [IO.File]::Create($Path)
    $writer = [IO.BinaryWriter]::new($stream)
    try {
        $writer.Write([Text.Encoding]::ASCII.GetBytes('RIFF'))
        $writer.Write([int]($dataBytes + 36))
        $writer.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt '))
        $writer.Write([int]16)
        $writer.Write([int16]1)
        $writer.Write([int16]$channels)
        $writer.Write([int]$rate)
        $writer.Write([int]($rate * $channels * 2))
        $writer.Write([int16]($channels * 2))
        $writer.Write([int16]16)
        $writer.Write([Text.Encoding]::ASCII.GetBytes('data'))
        $writer.Write([int]$dataBytes)
        for ($i = 0; $i -lt $frames; $i++) {
            $value = [int16][math]::Round(0.2 * 32767 * [math]::Sin(2 * [math]::PI * 440 * $i / $rate))
            $writer.Write($value)
            $writer.Write($value)
        }
    } finally {
        $writer.Dispose()
    }
}

function Test-SmokeResult([int]$ExitCode, [string]$Output) {
    $check = [regex]::Escape([string][char]0x2705)
    $cross = [string][char]0x274C
    $passed = ([regex]::Matches($Output, "(?m)^\s*$check")).Count
    if ($ExitCode -ne 0) { throw "冒烟进程退出码 ${ExitCode}（通过 $passed 项）" }
    if ($Output.Contains($cross)) { throw "冒烟输出包含失败标记（通过 $passed 项）" }
    # 空输出、程序提前退出和部分完成均不得通过发布门禁。
    if ($passed -lt 28) { throw "冒烟检查不完整：仅通过 $passed 项，要求至少 28 项" }
    foreach ($item in @('list_devices', 'scan_dir', 'start', 'play\(0\)', 'status',
                       'stop_all', 'set_gains', 'set_slot_gain', 'preview', 'stop_preview', 'release')) {
        if ($Output -notmatch "(?m)^\s*$check\s+$item(?:\s|$)") {
            throw "冒烟检查缺少成功项：$item"
        }
    }
    if ($Output -notmatch '全部通过；音频播放真实发生') { throw '冒烟检查没有完成总结' }
    return $passed
}

if ($TestSmokeHarness) {
    $testDir = Join-Path ([IO.Path]::GetTempPath()) ('mixer_smoke_gate_' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $testDir | Out-Null
    try {
        foreach ($name in @('麦克风混音助手', '测试 名称', 'Audio Mix')) {
            if ((Get-TauriProductName ([pscustomobject]@{ productName = $name })) -cne $name) {
                throw '中文或空格产品名不能被截断'
            }
        }
        foreach ($name in @('', '   ', 'bad/name', 'bad\name', 'bad:name', 'bad*name')) {
            $rejected = $false
            try { Get-TauriProductName ([pscustomobject]@{ productName = $name }) | Out-Null } catch { $rejected = $true }
            if (-not $rejected) { throw '无效产品名未被拒绝' }
        }
        $installerDir = Join-Path $testDir 'nsis'
        New-Item -ItemType Directory -Path $installerDir | Out-Null
        $expectedInstaller = Join-Path $installerDir '测试 名称_0.1.0_x64-setup.exe'
        [IO.File]::WriteAllText($expectedInstaller, 'new package')
        foreach ($stale in @('旧名称_0.1.0_x64-setup.exe', '测试 名称_0.0.9_x64-setup.exe')) {
            $path = Join-Path $installerDir $stale
            [IO.File]::WriteAllText($path, 'stale package')
            (Get-Item -LiteralPath $path).LastWriteTime = [datetime]::Now.AddMinutes(1)
        }
        $found = Find-NsisInstaller $installerDir '测试 名称' '0.1.0'
        if ($null -eq $found -or $found.FullName -cne $expectedInstaller) { throw '安装包选择误用了旧名或旧版本' }
        if ($null -ne (Find-NsisInstaller $installerDir '不存在' '0.1.0')) { throw '缺少本次安装包时不得兜底选择旧产物' }
        $plainBuildArguments = @(Get-TauriBuildArguments $true)
        if (($plainBuildArguments -join ' ') -ne 'build --no-bundle' -or
            $plainBuildArguments -contains '--bundles') { throw 'NoInstaller 构建参数错误' }
        $installerBuildArguments = @(Get-TauriBuildArguments $false)
        if (($installerBuildArguments -join ' ') -ne 'build --bundles nsis' -or
            $installerBuildArguments -contains '--no-bundle') { throw 'NSIS 构建参数错误' }
        $wave = Join-Path $testDir 'controlled.wav'
        New-SmokeWave $wave
        $bytes = [IO.File]::ReadAllBytes($wave)
        if ($bytes.Length -ne (44 + 48000 * 2 * 2 * 2) -or
            [Text.Encoding]::ASCII.GetString($bytes, 0, 4) -ne 'RIFF' -or
            [BitConverter]::ToInt16($bytes, 22) -ne 2 -or
            [BitConverter]::ToInt32($bytes, 24) -ne 48000 -or
            [BitConverter]::ToInt16($bytes, 34) -ne 16) { throw 'WAV fixture 格式错误' }
        if ([BitConverter]::ToInt16($bytes, 48) -eq 0) { throw 'WAV fixture 必须含非零信号' }
        $items = @('list_devices', 'scan_dir', 'start', 'play(0)', 'status', 'stop_all',
                   'set_gains', 'set_slot_gain', 'preview', 'stop_preview', 'release')
        $items += 1..17 | ForEach-Object { "extra_$_" }
        $good = (($items | ForEach-Object { "$([char]0x2705) $_ 通过" }) -join "`n") +
                "`n全部通过；音频播放真实发生"
        if ((Test-SmokeResult 0 $good) -ne 28) { throw '完整成功结果应通过门禁' }
        $cases = @(
            @{ Exit = 1; Output = $good },
            @{ Exit = 0; Output = '' },
            @{ Exit = 0; Output = "$([char]0x2705) start" },
            @{ Exit = 0; Output = ($good + "`n$([char]0x274C) 失败") },
            @{ Exit = 0; Output = $good.Replace('release', 'missing_release') },
            @{ Exit = 0; Output = $good.Replace('全部通过；音频播放真实发生', '') }
        )
        foreach ($case in $cases) {
            $rejected = $false
            try { Test-SmokeResult $case.Exit $case.Output | Out-Null } catch { $rejected = $true }
            if (-not $rejected) { throw '失败或不完整结果被误判为通过' }
        }
        Ok '冒烟门禁回归通过：产品名/安装包筛选 + 构建参数 + WAV + 正常完成 + 6 类失败/不完整结果'
    } finally {
        Remove-Item -Recurse -Force $testDir
    }
    exit 0
}

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

$tauriBuildArguments = @(Get-TauriBuildArguments $NoInstaller.IsPresent)
& "$Root\node_modules\.bin\tauri.cmd" @tauriBuildArguments
if ($LASTEXITCODE -ne 0) { Bad 'tauri build 失败'; exit 1 }

$exe = Join-Path $Root 'target\release\mixer-gui.exe'
if (-not (Test-Path $exe)) { Bad "没找到产物：$exe"; exit 1 }
Ok "exe：$([math]::Round((Get-Item $exe).Length / 1MB, 1)) MB"

# ── 2. 冒烟测试 ────────────────────────────────────────────────
Step '2/4' '冒烟测试（验证二进制真能跑）'

if ($SkipSmoke) {
    Warn '已跳过'
} else {
    $smokeDir = Join-Path ([IO.Path]::GetTempPath()) ('mixer_smoke_' + [guid]::NewGuid().ToString('N'))
    $sounds = Join-Path $smokeDir 'sounds'
    New-Item -ItemType Directory -Path $sounds | Out-Null
    $smokeProcess = $null
    $out = ''
    try {
        New-SmokeWave (Join-Path $sounds 'controlled_440Hz.wav')
        $stdout = Join-Path $smokeDir 'stdout.log'
        $stderr = Join-Path $smokeDir 'stderr.log'
        Write-Host '  跑 --smoke（受控 WAV、真实音频播放与异常路径）…'
        # release exe 为 Windows GUI subsystem，显式重定向并等待进程，避免空输出假通过。
        $smokeProcess = Start-Process -FilePath $exe -ArgumentList ('--smoke --dir "{0}"' -f $sounds) `
                   -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        # 在等待之前保留句柄，Windows PowerShell 5.1 也能可靠回读退出码。
        $null = $smokeProcess.Handle
        if (-not $smokeProcess.WaitForExit(240000)) {
            $smokeProcess.Kill()
            $smokeProcess.WaitForExit()
            throw '冒烟测试超过 240 秒，已终止'
        }
        $smokeProcess.Refresh()
        $out = (Get-Content -Raw -Encoding UTF8 $stdout) + "`n" + (Get-Content -Raw -Encoding UTF8 $stderr)
        $exitCode = $smokeProcess.ExitCode
        if ($null -eq $exitCode) { throw '无法读取冒烟进程退出码' }
        $passed = Test-SmokeResult $exitCode $out
        Ok "冒烟测试通过（$passed 项，退出码 0）"
    } catch {
        Bad "冒烟测试失败：$($_.Exception.Message)"
        if ($out) { $out -split "`n" | Select-Object -Last 18 | ForEach-Object { Write-Host "     $_" } }
        exit 1
    } finally {
        if ($smokeProcess) { $smokeProcess.Dispose() }
        Remove-Item -Recurse -Force $smokeDir
    }
}

# ── 3. 安装包 ──────────────────────────────────────────────────
Step '3/4' '安装包'

if ($NoInstaller) {
    Warn '按参数要求跳过了'
} else {
    $nsis = Find-NsisInstaller (Join-Path $Root 'target\release\bundle\nsis') $AppName $tauriConfig.version
    if ($null -eq $nsis) {
        Bad "没找到本次 NSIS 安装包：${AppName}_$($tauriConfig.version)_*-setup.exe"
        exit 1
    }
    Ok "安装包：$($nsis.Name)（$([math]::Round($nsis.Length / 1MB, 1)) MB）"
    Write-Host "     $($nsis.FullName)"
}

# ── 4. 下一步 ──────────────────────────────────────────────────
Step '4/4' '下一步'

Write-Host @"
  装完之后还需要一件事：**虚拟声卡**。

  语音、会议、直播或游戏应用从「麦克风」读声音，虚拟声卡扮演的就是那个麦克风。

    1. 去 https://vb-audio.com/Cable/ 下载 VB-CABLE（个人使用免费）
    2. 解压后**右键 → 以管理员身份运行** VBCABLE_Setup_x64.exe，点 Install Driver
    3. **重启电脑**（驱动要重启才生效）

  然后打开 ${AppName}：
    · 「输出到（虚拟声卡）」选 **CABLE Input**
    · 目标应用里的麦克风选 **CABLE Output**

  ⚠️ VB-CABLE 的命名是**反的**：本程序输出到 CABLE Input，
     而目标应用选麦克风要选 CABLE Output。名字看着别扭，但就是这样。
     程序里如果没检测到虚拟声卡，会直接把这段引导显示出来。

  想先确认整条链路通不通：界面上点「自检」。
"@

Write-Host "`n完成。`n"
