//! 麦克风混音助手 GUI —— Tauri 外壳。
//!
//! 前端是**纯静态 HTML/JS**（不引入打包器），通过 `withGlobalTauri` 暴露的
//! `window.__TAURI__.core.invoke` 调用下面的命令。
//!
//! 音频全部跑在 `host.rs` 的专用线程里，GUI 只发命令、读状态。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod host;
#[cfg(target_os = "macos")]
mod macos_audio;
mod smoke;
mod voice;
mod window_lifecycle;

use std::path::PathBuf;

use app_config::Config;
use host::{AudioHost, DeviceDto, StartConfig, StatusDto};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, State};
use voice::{VoiceParamsDto, VoicePresetDto, VoiceStatusDto};
use window_lifecycle::MainWindowEvent;

type Res<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Progress counters only have meaning inside the session that produced them.
#[derive(Default)]
struct AudioWatchdog {
    generation: u64,
    last: (u64, u64),
    stalled: u32,
}

impl AudioWatchdog {
    fn observe(&mut self, progress: (u64, u64, u64, bool)) -> Option<&'static str> {
        let (generation, rendered, captured, panicked) = progress;
        if generation == 0 {
            *self = Self::default();
            return None;
        }
        let frames = (rendered, captured);
        if self.generation != generation {
            self.generation = generation;
            self.last = frames;
            self.stalled = 0;
            return None;
        }
        let why = if panicked {
            Some("音频回调里发生过 panic")
        } else if captured == self.last.1 {
            Some("麦克风采集回调已停止推进")
        } else if rendered == self.last.0 {
            Some("音频渲染回调已停止推进")
        } else {
            None
        };
        self.last = frames;
        if why.is_some() {
            self.stalled += 1;
        } else {
            self.stalled = 0;
        }
        if self.stalled >= 3 {
            why
        } else {
            None
        }
    }
}

// ─────────────────────────────────────────── 设备与文件

#[tauri::command]
fn list_devices(host: State<'_, AudioHost>) -> Res<Vec<DeviceDto>> {
    host.list_devices().map_err(err)
}

#[tauri::command]
fn guess_output(host: State<'_, AudioHost>) -> Option<String> {
    host.guess_output()
}

/// 扫描文件夹，返回音频文件路径列表。
#[tauri::command]
fn scan_dir(path: String) -> Res<Vec<String>> {
    let p = PathBuf::from(shellexpand_tilde(&path));
    host::scan_dir(&p)
        .map(|v| v.into_iter().map(|x| x.display().to_string()).collect())
        .map_err(err)
}

/// 弹出系统原生文件夹选择框。取消时返回 None。
#[tauri::command]
fn pick_dir() -> Option<String> {
    rfd::FileDialog::new()
        .set_title("选择音效文件夹")
        .pick_folder()
        .map(|p| p.display().to_string())
}

fn shellexpand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = app_config::home_dir() {
            return h.join(rest).display().to_string();
        }
    }
    p.to_string()
}

// ─────────────────────────────────────────── 音频控制

#[tauri::command]
fn begin_playback_operation(host: State<'_, AudioHost>) -> u64 {
    // Only allocate an intent here. Slow device opens, preview decoding and
    // stream closure remain on workers; a stop can supersede them immediately.
    host.reserve_audio_request()
}

#[tauri::command]
async fn start(app: tauri::AppHandle, cfg: StartConfig, operation: Option<u64>) -> Res<bool> {
    let ticket = operation.unwrap_or_else(|| app.state::<AudioHost>().reserve_audio_request());
    tauri::async_runtime::spawn_blocking(move || {
        let host = app.state::<AudioHost>();
        let result = host.start_requested(cfg, Some(&app), ticket);
        result
            .map(|()| host.audio_request_is_current(ticket))
            .map_err(|e| {
                let message = format!("{e:#}");
                println!("[启动] ❌ 失败: {message}");
                host.record_error(message.clone());
                message
            })
    })
    .await
    .map_err(err)?
}

/// 停止音效 —— **但继续转发麦克风**（对方照常听得到你说话）。
///
/// 语义是「关掉音效」而不是「把我变成哑巴」：游戏的麦克风一直指着虚拟声卡，
/// 一旦连转发也断了，虚拟声卡里就是一片死寂，对方会突然听不到你。
#[tauri::command]
async fn stop(app: tauri::AppHandle, operation: Option<u64>) -> Res<bool> {
    let ticket = operation.unwrap_or_else(|| app.state::<AudioHost>().reserve_audio_request());
    tauri::async_runtime::spawn_blocking(move || {
        let host = app.state::<AudioHost>();
        host.stop_requested(ticket);
        host.audio_request_is_current(ticket)
    })
    .await
    .map_err(err)
}

/// 彻底释放音频设备（换设备、退出时用）。
///
/// ⚠️ 释放后虚拟声卡里就没人写了 —— 游戏的麦克风如果还指着它，对方会听不到你。
#[tauri::command]
async fn release(app: tauri::AppHandle, operation: Option<u64>) -> Res<bool> {
    let ticket = operation.unwrap_or_else(|| app.state::<AudioHost>().reserve_audio_request());
    tauri::async_runtime::spawn_blocking(move || {
        let host = app.state::<AudioHost>();
        host.release_requested(ticket);
        host.audio_request_is_current(ticket)
    })
    .await
    .map_err(err)
}

/// 一键切回真实麦克风。
///
/// **为什么需要**：游戏的麦克风一旦指向虚拟声卡，"对方能不能听到你"就完全由本程序决定了。
/// 用户不想用了（或要退出）时，如果只关掉本程序，游戏那边读到的是**一片死寂** ——
/// 而这个设置藏在游戏里，很多人根本不知道要去改。
///
/// 这里能做的是把**系统默认输入设备**切回真实麦克风（并在切换前释放本程序的设备占用），
/// 对"跟随系统默认"的程序立刻生效。**但显式指定了设备的游戏改不了** ——
/// 所以返回里带上"有没有真的改成功"和需要手动改的提醒。
#[tauri::command]
async fn revert_to_real_mic(app: tauri::AppHandle) -> Res<serde_json::Value> {
    tauri::async_runtime::spawn_blocking(move || revert_mic_inner(&app))
        .await
        .map_err(err)?
}

fn revert_mic_inner(app: &tauri::AppHandle) -> Res<serde_json::Value> {
    let host = app.state::<AudioHost>();
    #[cfg(target_os = "macos")]
    {
        let (ok, device, current) = host.revert_to_real_mic().map_err(err)?;
        Ok(serde_json::json!({
            "ok": ok,
            "device": device,
            "current": current,
            // 系统默认改了还不够 —— 显式指定了麦克风的游戏要用户自己改
            "note": "系统默认麦克风已切回。如果语音、会议、直播或游戏应用里是手动指定的麦克风，\
                     还需要进去把它也改回真实麦克风，否则对方仍然听不到你。",
        }))
    }
    // Windows 上没有这个功能，**不是遗漏，是设计如此**：
    //
    // 「切回真实麦克风」的作用是**回滚 macOS 流程改过的东西** ——
    // macOS 上为了免去游戏内设置，程序会把系统默认输入设备切成虚拟声卡，
    // 用完之后需要切回来。
    //
    // Windows 流程**压根不改系统设置**（是让用户在游戏里选麦克风），
    // 所以没有东西可回滚。要做"设为默认通信设备"得用未公开的 IPolicyConfig COM，
    // 那超出当前范围（目标里写的是「VB-CABLE 检测与安装引导」）。
    #[cfg(not(target_os = "macos"))]
    {
        let _ = host;
        Err("Windows 上不需要这个：本程序不会改动系统默认设备，\
             目标应用里的麦克风是你自己选的。想让应用自动用它，\
             去 设置 → 系统 → 声音 → 更多声音设置 → 录制，把你的设备设为默认通信设备。"
            .into())
    }
}

#[tauri::command]
async fn play(app: tauri::AppHandle, slot: usize, operation: Option<u64>) -> Res<bool> {
    let ticket = operation.unwrap_or_else(|| app.state::<AudioHost>().reserve_audio_request());
    tauri::async_runtime::spawn_blocking(move || {
        let host = app.state::<AudioHost>();
        host.play_requested(slot, ticket).map_err(err)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
async fn stop_all(app: tauri::AppHandle, operation: Option<u64>) -> Res<bool> {
    let ticket = operation.unwrap_or_else(|| app.state::<AudioHost>().reserve_audio_request());
    // 停止试听需要等待输出线程释放设备，不能阻塞 GUI 主线程。
    tauri::async_runtime::spawn_blocking(move || {
        let host = app.state::<AudioHost>();
        host.stop_all_requested(ticket)
            .map(|()| host.audio_request_is_current(ticket))
            .map_err(err)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
fn set_gains(host: State<'_, AudioHost>, mic_db: f32, sfx_db: f32) {
    host.set_gains(mic_db, sfx_db);
}

#[tauri::command]
async fn set_looping(app: tauri::AppHandle, looping: bool) -> Res<bool> {
    tauri::async_runtime::spawn_blocking(move || app.state::<AudioHost>().set_looping(looping))
        .await
        .map_err(err)
}

/// 调整单个音效的音量。
#[tauri::command]
fn set_slot_gain(host: State<'_, AudioHost>, slot: usize, db: f32) -> Res<bool> {
    host.set_slot_gain(slot, db).map_err(err)
}

/// 稳定音效 ID 同时定位主播放、监听与独立试听。
#[tauri::command]
async fn set_sound_gain(app: tauri::AppHandle, sound_id: String, db: f32) -> Res<bool> {
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<AudioHost>()
            .set_sound_gain(&sound_id, db)
            .map_err(err)
    })
    .await
    .map_err(err)?
}

/// 前端 JS 出问题时把错误送到后端打出来。
///
/// 为什么需要：打包后的 App 没有开发者控制台，**前端一旦抛异常就完全静默** ——
/// 表现只是"窗口一片空白 / 一直停在未就绪"，后端日志里一个字都没有。
/// 有了这条通道，`open --stdout` 就能直接看到前端的报错。
#[tauri::command]
fn js_error(msg: String, stack: String) {
    println!("[前端错误] {msg}");
    if !stack.is_empty() {
        for line in stack.lines().take(6) {
            println!("           {line}");
        }
    }
}

/// 产品名称与版本来自打包配置，供两端共用的关于窗口展示。
#[tauri::command]
fn app_info(app: tauri::AppHandle) -> serde_json::Value {
    serde_json::json!({
        "name": app.config().product_name.as_deref().unwrap_or("麦克风混音助手"),
        "version": app.package_info().version.to_string(),
    })
}

/// 当前平台，以及推荐的虚拟声卡方案。
///
/// 前端是**两端共用**的，但"怎么装虚拟声卡"这件事在两个平台完全不一样 ——
/// 所以由后端告诉它该显示哪一套，而不是在前端里堆 if。
#[tauri::command]
fn platform_info() -> serde_json::Value {
    platform_info_for(app_config::Platform::current())
}

/// `platform_info` 的实际内容。**平台作为参数** —— 这样两套引导文案
/// 都能在 macOS 上被单元测试覆盖（见文件末尾），
/// 否则改了文案没有任何东西会拦住你。
fn platform_info_for(p: app_config::Platform) -> serde_json::Value {
    if p == app_config::Platform::Windows {
        serde_json::json!({
            "os": "windows",
            "title": "这台电脑上没找到虚拟声卡",
            "driver": "VB-CABLE",
            "steps": [
                "去 vb-audio.com/Cable 下载 VB-CABLE（个人使用免费）",
                "解压后**右键 → 以管理员身份运行** VBCABLE_Setup_x64.exe，点 Install Driver",
                "**重启电脑**（驱动要重启才生效）",
                "回来点下面的「重新检测」",
            ],
            "url": "https://vb-audio.com/Cable/",
            "cmd": null,
            // Windows 上 VB-CABLE 的命名是**反的**，这是最容易卡住人的地方
            "naming": "⚠️ 注意 VB-CABLE 的命名是反的：本程序要输出到 **CABLE Input**，\
                       而目标应用里选麦克风要选 **CABLE Output**。名字看着别扭，但就是这样。",
            "alts": "其他选择：VoiceMeeter（免费，功能更多但更复杂）、Virtual Audio Cable（付费）。",
            "mic_privacy": {
                "title": "麦克风收不到声音？",
                "steps": [
                    "打开 设置 → 隐私和安全性 → 麦克风，确认「麦克风访问」是**开**的",
                    "往下拉到「允许桌面应用访问你的麦克风」—— **这一条最容易漏**。\
                     麦克风混音助手属于桌面应用，这个开关和上面那个是分开的，上面开了下面没开照样没声音。",
                    "如果游戏或语音软件里听不到：Windows 有**两套**默认设备 —— \
                     「默认设备」和「默认通信设备」。很多游戏读的是后者。\
                     去 设置 → 系统 → 声音 → 更多声音设置 → 录制，\
                     右键你的设备 → 设为默认通信设备。",
                ],
                "settings_page": "microphone",
                "url_label": "打开麦克风隐私设置",
            },
        })
    } else {
        serde_json::json!({
            "os": "macos",
            "title": "这台 Mac 上没找到虚拟声卡",
            "driver": "BlackHole",
            "steps": [
                "用下面的命令装 BlackHole（免费、开源），或去官网下载安装包",
                "**装完重启一次**",
                "回来点下面的「重新检测」",
            ],
            "url": "https://existential.audio/blackhole/",
            "cmd": "brew install --cask blackhole-2ch",
            "naming": null,
            "alts": "其他选择：VB-CABLE for Mac（个人免费）、Loopback（付费，功能最多）。",
            "mic_privacy": {
                "title": "麦克风收不到声音？",
                "steps": [
                    "打开 系统设置 → 隐私与安全性 → 麦克风",
                    "确认「麦克风混音助手」在列表里并且是**勾上**的",
                    "如果列表里没有它：把 App 删掉重新拖进去一次（签名变了系统会当成新 App）",
                ],
                "settings_page": "microphone",
                "url_label": "打开麦克风隐私设置",
            },
        })
    }
}

/// 打开**系统设置里的某个页面**。
///
/// 为什么不复用 `open_url`：那个命令只允许 http/https（它是暴露给前端的，
/// 收窄更安全）。而系统设置页用的是各平台私有 scheme
/// （macOS 的 `x-apple.systempreferences:`、Windows 的 `ms-settings:`）。
///
/// 所以这里做成**只认白名单键**，前端传的是 `"microphone"` 这种语义名，
/// 具体 URL 由后端拼 —— 前端没法让它去开任意地址。
/// 系统设置页的**白名单映射**。
///
/// 抽成纯函数是为了能被单元测试覆盖 —— 这是个安全边界
/// （前端只能传语义名，URL 由这里决定），不该只靠"看代码觉得对"。
fn settings_url_for(p: app_config::Platform, page: &str) -> Option<&'static str> {
    match (p, page) {
        (app_config::Platform::Unix, "microphone") => {
            Some("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")
        }
        (app_config::Platform::Unix, "sound") => {
            Some("x-apple.systempreferences:com.apple.Sound-Settings.extension")
        }
        (app_config::Platform::Windows, "microphone") => Some("ms-settings:privacy-microphone"),
        (app_config::Platform::Windows, "sound") => Some("ms-settings:sound"),
        _ => None,
    }
}

/// 打开**系统设置里的某个页面**。
///
/// 为什么不复用 `open_url`：那个命令只允许 http/https（它是暴露给前端的，
/// 收窄更安全）。而系统设置页用的是各平台私有 scheme
/// （macOS 的 `x-apple.systempreferences:`、Windows 的 `ms-settings:`）。
///
/// 所以这里做成**只认白名单键**，前端传的是 `"microphone"` 这种语义名，
/// 具体 URL 由后端拼 —— 前端没法让它去开任意地址。
#[tauri::command]
fn open_system_settings(page: String) -> Res<()> {
    let url = settings_url_for(app_config::Platform::current(), &page)
        .ok_or_else(|| format!("不认识的设置页：{page}"))?;

    // Linux 上没有统一的"系统设置"入口，映射表里也没有它 —— 上面那步就会返回 None
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    #[cfg(not(target_os = "windows"))]
    let cmd = "open";

    std::process::Command::new(cmd)
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开系统设置失败：{e}"))
}

/// 手机遥控地址的二维码矩阵。
///
/// 手机上扫一下就能打开遥控页 —— 比手输 `10.220.179.90:8765` 快得多，
/// 而遥控是**热键被游戏吞掉时的唯一退路**。
/// 返回矩阵而不是图片：前端用 canvas 画，不引入图片依赖。
#[tauri::command]
fn remote_qr(host: State<'_, AudioHost>) -> Res<serde_json::Value> {
    let urls = host.status().remote_urls;
    // 优先局域网地址（手机要用的就是它），没有就退回本机地址
    let url = urls
        .iter()
        .find(|u| !u.contains("127.0.0.1"))
        .or_else(|| urls.first())
        .cloned()
        .ok_or("遥控还没启动 —— 先点「启动」或勾上「手机遥控」")?;

    let (w, cells) = remote::qr_matrix(&url).ok_or("二维码生成失败")?;
    Ok(serde_json::json!({
        "url": url,
        "width": w,
        "cells": cells,
    }))
}

/// 端到端自检：把测试音送进虚拟设备，再从环回录回来验证整条链路。
///
/// 只能在**停止状态**跑 —— 引擎运行时两边会抢同一个设备。
/// 用 async 让它跑在异步线程上，否则 4 秒的阻塞会把界面冻住。
#[tauri::command]
async fn self_test(
    app: tauri::AppHandle,
    output_device: Option<String>,
    capture_device: Option<String>,
) -> Res<serde_json::Value> {
    let r = tauri::async_runtime::spawn_blocking(move || {
        app.state::<AudioHost>().with_suspended(Some(&app), || {
            let backend = backend_cpal::CpalBackend::new();
            selftest::run_with_capture_retry(&backend, output_device, capture_device, 440.0, 2.0, 3)
        })
    })
    .await
    .map_err(|e| format!("自检线程失败: {e}"))?
    .map_err(|e| format!("{e:#}"))?;

    Ok(serde_json::json!({
        "passed": r.passed(),
        "attempts": r.attempts,
        "summary": r.summary(),
        "device": r.device,
        "checks": r.checks.iter().map(|c| serde_json::json!({
            "name": c.name, "got": c.got, "want": c.want, "ok": c.ok,
        })).collect::<Vec<_>>(),
        "advice": r.advice,
    }))
}

/// 调试用：设置 `MIXER_AUTOSTART` 让前端自动做一件事，用于无人值守验证
/// GUI 专有路径（热键绑定、自检命令）。
///   `1` / `start` → 自动点一次「启动」
///   `selftest`    → 自动点一次「自检」，结果写入 `MIXER_READY_FILE`
#[tauri::command]
fn autostart() -> String {
    std::env::var("MIXER_AUTOSTART").unwrap_or_default()
}

/// 单独启动手机遥控（音效还没启动时也能先起服务）。
#[tauri::command]
fn start_remote(host: State<'_, AudioHost>, port: u16) -> Res<Vec<String>> {
    host.start_remote(port, "0.0.0.0".to_string()).map_err(err)
}

/// 用系统默认浏览器打开一个地址（点遥控地址时用）。
#[tauri::command]
fn open_url(url: String) -> Res<()> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("只允许 http/https 地址".into());
    }
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";
    std::process::Command::new(cmd)
        .arg(&url)
        .spawn()
        .map(|_| ())
        .map_err(err)
}

/// 试听：只放给自己听，不送进虚拟麦克风。
#[tauri::command]
async fn preview(
    app: tauri::AppHandle,
    file: String,
    gain_db: f32,
    sound_id: Option<String>,
    operation: Option<u64>,
) -> Res<bool> {
    let ticket = operation.unwrap_or_else(|| app.state::<AudioHost>().reserve_audio_request());
    tauri::async_runtime::spawn_blocking(move || {
        let host = app.state::<AudioHost>();
        match sound_id {
            Some(id) => host.preview_sound_requested(id, PathBuf::from(file), gain_db, ticket),
            None => host.preview_requested(PathBuf::from(file), gain_db, ticket),
        }
        .map(|()| host.audio_request_is_current(ticket))
        .map_err(err)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
async fn stop_preview(app: tauri::AppHandle, operation: Option<u64>) -> Res<bool> {
    let ticket = operation.unwrap_or_else(|| app.state::<AudioHost>().reserve_audio_request());
    tauri::async_runtime::spawn_blocking(move || {
        let host = app.state::<AudioHost>();
        host.stop_preview_requested(ticket);
        host.audio_request_is_current(ticket)
    })
    .await
    .map_err(err)
}

/// 多选音频文件（原生文件选择框）。
#[tauri::command]
fn add_files() -> Vec<String> {
    rfd::FileDialog::new()
        .set_title("添加音效")
        .add_filter(
            "音频",
            &[
                "mp3", "wav", "ogg", "m4a", "flac", "aac", "aiff", "caf", "opus",
            ],
        )
        .pick_files()
        .map(|v| v.into_iter().map(|p| p.display().to_string()).collect())
        .unwrap_or_default()
}

#[tauri::command]
fn status(host: State<'_, AudioHost>) -> StatusDto {
    host.status()
}

// ─────────────────────────────────────────── 变声

#[tauri::command]
fn voice_status(host: State<'_, AudioHost>) -> VoiceStatusDto {
    host.voice_status()
}

#[tauri::command]
fn voice_presets(host: State<'_, AudioHost>) -> Vec<VoicePresetDto> {
    host.voice_presets()
}

#[tauri::command]
fn voice_set_enabled(host: State<'_, AudioHost>, enabled: bool) -> Res<VoiceStatusDto> {
    host.voice_set_enabled(enabled).map_err(err)
}

#[tauri::command]
fn voice_set_bypass(host: State<'_, AudioHost>, bypass: bool) -> Res<VoiceStatusDto> {
    host.voice_set_bypass(bypass).map_err(err)
}

#[tauri::command]
fn voice_set_params(host: State<'_, AudioHost>, params: VoiceParamsDto) -> Res<VoiceStatusDto> {
    host.voice_set_params(params).map_err(err)
}

#[tauri::command]
fn voice_select_preset(host: State<'_, AudioHost>, id: String) -> Res<VoiceStatusDto> {
    host.voice_select_preset(&id).map_err(err)
}

/// 前端加载完成后主动上报 —— 用来证明 WebView 真的渲染并执行了 JS。
/// `--probe` 模式下会把它打印出来，供自动化验证。
fn sound_item_dto(item: &app_config::SoundEntry) -> serde_json::Value {
    serde_json::json!({
        "id": item.id,
        "file": item.path.display().to_string(),
        "name": item.name,
        "gain": item.gain_db,
        "tags": item.tags,
        "hotkey": item.hotkey,
    })
}

fn parse_sound_items(items: &[serde_json::Value]) -> Vec<app_config::SoundEntry> {
    items
        .iter()
        .filter_map(|v| {
            let file = v.get("file")?.as_str()?.to_string();
            if file.is_empty() {
                return None;
            }
            let path = std::path::PathBuf::from(&file);
            let name = v
                .get("name")
                .and_then(|x| x.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    path.file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "未命名".into())
                });
            let gain = v.get("gain").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
            let tags = match v.get("tags") {
                Some(serde_json::Value::Array(tags)) => tags
                    .iter()
                    .filter_map(|tag| tag.as_str().map(str::to_owned))
                    .collect(),
                Some(serde_json::Value::String(tags)) => tags
                    .split([',', '|', ';'])
                    .map(|tag| tag.trim().to_owned())
                    .filter(|tag| !tag.is_empty())
                    .collect(),
                _ => Vec::new(),
            };
            Some(app_config::SoundEntry {
                id: v
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_owned(),
                hotkey: v
                    .get("hotkey")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_owned(),
                path,
                name,
                gain_db: gain,
                tags,
            })
        })
        .collect()
}

/// 读取上次保存的配置（音效目录、设备、增益、热键）。
#[tauri::command]
fn load_config() -> serde_json::Value {
    let c = Config::load();
    saved_config_dto(&c)
}

fn saved_config_dto(c: &Config) -> serde_json::Value {
    // 音效库条目：路径 / 显示名 / 音量 / 标签
    let items: Vec<serde_json::Value> = if c.sound_files.is_empty() {
        // 没有显式列表时退回扫目录，此时没有自定义名与标签
        c.collect_sounds()
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(i, p)| {
                serde_json::json!({
                    "file": p.display().to_string(),
                    "name": p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                    "gain": c.normalized_gains(i + 1).get(i).copied().unwrap_or(0.0),
                    "tags": Vec::<String>::new(),
                })
            })
            .collect()
    } else {
        c.sound_entries().iter().map(sound_item_dto).collect()
    };
    serde_json::json!({
        "items": items,
        "monitor": c.monitor,
        "looping": c.looping,
        "monitor_device": c.monitor_device,
        "dir": c.sound_dirs.first().map(|p| p.display().to_string()).unwrap_or_default(),
        "input_device": c.input_device,
        "output_device": c.output_device,
        "buffer_ms": c.buffer_ms.unwrap_or(20),
        "mic_gain_db": c.mic_gain_db.unwrap_or(0.0),
        "sfx_gain_db": c.sfx_gain_db.unwrap_or(-6.0),
        "hotkeys": c.hotkeys.join(","),
        "path": Config::path().display().to_string(),
    })
}

/// 保存当前设置，下次打开自动恢复。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn save_config(
    items: Vec<serde_json::Value>,
    monitor: bool,
    looping: Option<bool>,
    monitor_device: Option<String>,
    dir: String,
    input_device: Option<String>,
    output_device: Option<String>,
    buffer_ms: u32,
    mic_gain_db: f32,
    sfx_gain_db: f32,
    hotkeys: String,
) -> Result<(), String> {
    save_config_at(
        &Config::path(),
        items,
        monitor,
        looping,
        monitor_device,
        dir,
        input_device,
        output_device,
        buffer_ms,
        mic_gain_db,
        sfx_gain_db,
        hotkeys,
    )
}

/// 完整配置保存只覆盖界面拥有的字段。重读磁盘后再写，变声节与用户预设
/// 原样保留；与变声持久化共用写入门，避免两份整配置互相覆盖。
#[allow(clippy::too_many_arguments)]
pub(crate) fn save_config_at(
    path: &std::path::Path,
    items: Vec<serde_json::Value>,
    monitor: bool,
    looping: Option<bool>,
    monitor_device: Option<String>,
    dir: String,
    input_device: Option<String>,
    output_device: Option<String>,
    buffer_ms: u32,
    mic_gain_db: f32,
    sfx_gain_db: f32,
    hotkeys: String,
) -> Result<(), String> {
    crate::host::with_config_write(|| {
        let mut c =
            crate::host::load_config_for_write(path).map_err(|error| format!("{error:#}"))?;
        c.sound_dirs = if dir.trim().is_empty() {
            Vec::new()
        } else {
            vec![PathBuf::from(dir)]
        };
        c.set("monitor", if monitor { "true" } else { "false" })
            .map_err(err)?;
        if let Some(looping) = looping {
            c.set("looping", if looping { "true" } else { "false" })
                .map_err(err)?;
        }
        c.set("monitor_device", monitor_device.as_deref().unwrap_or(""))
            .map_err(err)?;

        // 音效库：整组覆盖（路径/音量/名字/标签），保持并行数组一一对应
        let parsed = parse_sound_items(&items);
        c.set_sound_entries(&parsed);
        c.set("input_device", input_device.as_deref().unwrap_or("none"))
            .map_err(err)?;
        c.set("output_device", output_device.as_deref().unwrap_or("none"))
            .map_err(err)?;
        c.set("buffer_ms", &buffer_ms.to_string()).map_err(err)?;
        c.set("mic_gain_db", &mic_gain_db.to_string())
            .map_err(err)?;
        c.set("sfx_gain_db", &sfx_gain_db.to_string())
            .map_err(err)?;
        c.set("hotkeys", &hotkeys).map_err(err)?;
        c.save_to(path).map_err(err)
    })
}

#[tauri::command]
fn frontend_ready(info: String) {
    println!("[前端就绪] {info}");
    if let Ok(p) = std::env::var("MIXER_READY_FILE") {
        let _ = std::fs::write(p, &info);
    }
}

// ─────────────────────────────────────────── 菜单栏

/// 菜单栏图标 + 菜单。关闭窗口后靠它把窗口找回来、或真正退出。
fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
    let stop = MenuItem::with_id(app, "stop", "停止音效与试听", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    // 「切回真实麦克风」目前只有 macOS 实现（改的是系统默认输入设备）。
    // Windows 上要改的是"默认通信设备"，那是另一套 API —— 见 TODO。
    #[cfg(target_os = "macos")]
    let revert = MenuItem::with_id(
        app,
        "revert",
        "切回真实麦克风（系统默认输入）",
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "退出（停止音频）", true, None::<&str>)?;
    #[cfg(target_os = "macos")]
    let menu = Menu::with_items(app, &[&show, &stop, &revert, &sep, &quit])?;
    #[cfg(not(target_os = "macos"))]
    let menu = Menu::with_items(app, &[&show, &stop, &sep, &quit])?;

    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray44.png"))?;

    TrayIconBuilder::with_id("main-tray")
        .icon(icon)
        .icon_as_template(true) // macOS：按菜单栏明暗自动着色
        .tooltip("麦克风混音助手")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id.as_ref() {
            "show" => show_window(app),
            "stop" => {
                let ticket = app.state::<AudioHost>().reserve_audio_request();
                let app = app.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    match app.state::<AudioHost>().stop_all_requested(ticket) {
                        Ok(()) => println!("[菜单栏] 已停止音效与试听"),
                        Err(e) => println!("[菜单栏] 停止失败: {e}"),
                    }
                });
            }
            #[cfg(target_os = "macos")]
            "revert" => {
                let app = app.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    if let Err(e) = revert_mic_inner(&app) { println!("[菜单栏] ❌ 切回失败: {e}"); }
                });
            }
            "quit" => request_exit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                show_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn show_window(app: &tauri::AppHandle) {
    handle_main_window_event(app, MainWindowEvent::ShowRequested);
}

fn handle_main_window_event(app: &tauri::AppHandle, event: MainWindowEvent) {
    let window = app.get_webview_window("main");
    window_lifecycle::handle_main_window_event(event, window.as_ref());
}

// ─────────────────────────────────────────── 入口

/// 单实例守卫。
///
/// 用 **localhost 端口**做互斥而不是锁文件：进程无论怎么退出，操作系统都会释放端口，
/// 不会留下"死锁文件"把用户永久挡在门外。
/// 需要它的原因：两个实例会抢同一个虚拟设备（声音叠加）、并且第二个实例的热键注册
/// 必然失败（键位已被第一个占用），用户只会看到"热键莫名其妙不好使"。
const SINGLE_INSTANCE_PORT: u16 = 47821;

fn acquire_single_instance() -> Option<std::net::TcpListener> {
    std::net::TcpListener::bind(("127.0.0.1", SINGLE_INSTANCE_PORT)).ok()
}

/// Keep the native event loop responsive until thread-owned resources are released.
fn request_exit(app: &tauri::AppHandle) {
    if !app.state::<AudioHost>().begin_shutdown() {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let host = app.state::<AudioHost>();
        #[cfg(target_os = "macos")]
        if let Err(e) = host.revert_to_real_mic() {
            println!("[退出] 恢复真实麦克风失败: {e:#}");
        }
        host.release();
        host.finish_shutdown();
        app.exit(0);
    });
}

fn main() {
    // macOS 的 Carbon 热键在应用主线程创建和注销，需要先登记该线程。
    // Windows 热键由独立的消息线程拥有和派发。
    hotkeys::note_main_thread();
    // 最先装 —— 越早装，越不会漏掉启动阶段的 panic
    host::install_panic_hook();
    // 自测钩子：只有设了 MIXER_PANIC_AFTER 才生效
    audio_core::engine::arm_panic_hook_from_env();
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,symphonia=off"),
    )
    .init();

    let args: Vec<String> = std::env::args().collect();

    // 单实例守卫（探针模式除外 —— 它们本来就该能并行跑）
    let is_probe = args.iter().any(|a| a == "--smoke" || a == "--probe-close");
    let _instance_guard = if is_probe {
        None
    } else {
        match acquire_single_instance() {
            Some(l) => Some(l),
            None => {
                eprintln!(
                    "麦克风混音助手已经在运行了。\n\
                     如果找不到窗口，点菜单栏那个均衡器图标把它调出来。\n\
                     （同时开两个实例会导致声音叠加、热键失效）"
                );
                std::process::exit(0);
            }
        }
    };

    // 无窗口冒烟测试：验证 GUI 背后的全部命令，不需要屏幕录制权限
    if args.iter().any(|a| a == "--smoke") {
        let dir = args
            .iter()
            .position(|a| a == "--dir")
            .and_then(|i| args.get(i + 1))
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                app_config::home_dir()
                    .map(|h| h.join("Sounds"))
                    .unwrap_or_else(|| std::env::temp_dir().join("mixer-smoke"))
            });
        match smoke::run(dir) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("\n❌ 冒烟测试失败: {e:#}\n");
                std::process::exit(1);
            }
        }
    }

    tauri::Builder::default()
        .manage(AudioHost::new())
        .setup(|app| {
            build_tray(app.handle())?;
            println!("[菜单栏] 图标已创建");

            // 记下用户原本的默认麦克风 —— 「切回真实麦克风」要还原成它。
            // 必须在这里（用户还没点过「启动」）记录，否则记录到的可能是我们自己改过的值。
            #[cfg(target_os = "macos")]
            app.state::<AudioHost>().remember_default_input();

            // ── 热插拔看门狗 ──────────────────────────────────────────
            //
            // 耳机拔了、虚拟声卡驱动被卸载/重启 —— 这些都会让音频链路**悄悄断掉**，
            // 而用户正在游戏里，根本发现不了（他只会觉得"对方怎么不理我"）。
            // 每 2 秒检查一次，出事立刻报警。
            {
                let h = app.handle().clone();
                std::thread::Builder::new()
                    .name("hotplug".into())
                    .spawn(move || {
                        use tauri::Manager;
                        let mut last: Option<(u64, String)> = None;
                        loop {
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            let host = h.state::<AudioHost>();
                            if host.is_quitting() { break; }
                            let Some((generation, in_lost, out_lost, name)) = host.check_devices() else {
                                if let Some((generation, _)) = last.take() {
                                    host.set_device_lost_for(generation, None);
                                }
                                continue;
                            };
                            let key = format!("{in_lost}/{out_lost}/{name}");
                            if last.as_ref().is_some_and(|previous| previous.0 == generation && previous.1 == key) {
                                continue; // 同一件事不重复报
                            }
                            last = Some((generation, key));

                            if out_lost {
                                let msg = format!(
                                    "虚拟声卡「{name}」不见了 —— 对方现在听不到你。\
                                     请重新选一个输出设备，再点「启动」。"
                                );
                                if host.set_device_lost_for(generation, Some(msg.clone())) {
                                    println!("[热插拔] ❌ {msg}");
                                }
                            } else if in_lost {
                                if !host.set_device_lost_for(generation, Some(format!(
                                    "麦克风「{name}」已断开，正在自动切到系统默认麦克风…"
                                ))) {
                                    continue;
                                }
                                println!(
                                    "[热插拔] ⚠ 麦克风「{name}」已断开，自动切到系统默认麦克风…"
                                );
                                match host.recover_input_device(generation, Some(&h)) {
                                    Ok(Some((new_generation, old))) => {
                                        if host.set_device_lost_for(new_generation, Some(format!(
                                            "麦克风「{old}」已断开，已自动切到系统默认麦克风。"
                                        ))) {
                                            println!("[热插拔] ✅ 已从「{old}」切到系统默认麦克风");
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(e) => {
                                        println!("[热插拔] ❌ 自动恢复失败: {e:#}");
                                        host.set_device_lost_for(generation, Some(format!(
                                            "麦克风「{name}」已断开，且自动恢复失败：{e}"
                                        )));
                                    }
                                }
                            }
                        }
                    })
                    .ok();
            }

            // ── 音频看门狗 ──────────────────────────────────────────
            //
            // 为什么需要：这个程序大部分时间在后台跑，而**"没声音"是它最糟的失败模式** ——
            // 用户正在游戏里，对方突然听不到他，屏幕上却什么提示都没有。
            //
            // 检测两件事：
            //   ① 渲染回调不再被推进（流死了）—— 用 rendered_frames 是否还在涨判断
            //   ② 回调里 panic 过（backend-cpal 会置位）—— 被 catch_unwind 兜住了，
            //      但引擎内部状态已不可信，必须重开
            // 两者都靠**重启音频**自救，并把原因写到界面上。
            {
                let h = app.handle().clone();
                std::thread::Builder::new()
                    .name("audio-watchdog".into())
                    .spawn(move || {
                        use tauri::Manager;
                        let mut watchdog = AudioWatchdog::default();
                        loop {
                            std::thread::sleep(std::time::Duration::from_secs(1));
                            let host = h.state::<AudioHost>();
                            if host.is_quitting() { break; }
                            let progress = host.audio_progress();
                            let generation = progress.0;
                            if let Some(why) = watchdog.observe(progress) {
                                if !host.set_audio_stalled_for(generation, Some(format!("{why}，正在自动重启…"))) {
                                    watchdog = AudioWatchdog::default();
                                    continue;
                                }
                                println!("[看门狗] ⚠ {why} —— 自动重启音频");
                                match host.restart_audio(generation, Some(&h)) {
                                    Ok(Some(new_generation)) => {
                                        if host.set_audio_stalled_for(new_generation, Some(format!(
                                            "{why} —— 已自动恢复；若通话曾中断，请重新连接"
                                        ))) {
                                            println!("[看门狗] ✅ 音频已自动恢复");
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(e) => {
                                        println!("[看门狗] ❌ 自动恢复失败: {e:#}");
                                        host.set_audio_stalled_for(generation, Some(format!(
                                            "{why}，且自动恢复失败：{e}。请手动点「启动」重试"
                                        )));
                                    }
                                }
                                watchdog = AudioWatchdog::default();
                            }
                        }
                    })
                    .ok();
            }

            // 一启动就问麦克风权限，别等到用户点「启动」时才弹窗
            host::request_mic_permission();

            // 音效文件所在的目录也可能需要授权（macOS 对下载/桌面/文稿有保护）。
            // 同样提前探测 —— 否则用户点「启动」时会**毫无反应**，
            // 因为文件读取在授权确定之前是直接阻塞的。
            {
                let cfg = Config::load();
                let files: Vec<PathBuf> = if cfg.sound_files.is_empty() {
                    cfg.collect_sounds().unwrap_or_default()
                } else {
                    cfg.sound_files
                };
                host::probe_sound_file_access(&files);
            }

            // 自检探针：程序自己关窗，验证"关窗不退"真的生效
            if std::env::args().any(|a| a == "--probe-close") {
                let h = app.handle().clone();
                std::thread::spawn(move || {
                    // 等够时间：WebView 加载 + autostart 启动引擎约需 4~6 秒
                    std::thread::sleep(std::time::Duration::from_secs(9));
                    // 关窗**之前**先记一次引擎状态 —— 否则分不清"压根没启动"和"关窗把它停了"
                    let before = h.state::<AudioHost>().status();
                    if let Some(w) = h.get_webview_window("main") {
                        let _ = w.close(); // 触发 CloseRequested
                    }
                    std::thread::sleep(std::time::Duration::from_secs(4));
                    let visible = h
                        .get_webview_window("main")
                        .and_then(|w| w.is_visible().ok())
                        .unwrap_or(true);
                    let alive = h.get_webview_window("main").is_some();
                    // 关键：窗口关了，**引擎必须还在跑** —— 否则用户在游戏里就用不了
                    let after = h.state::<AudioHost>().status();
                    let verdict = if before.running && after.running {
                        "✅ 关窗前后引擎都在跑 —— 正确"
                    } else if !before.running {
                        "⚠ 关窗**之前**引擎就没启动（autostart 没生效？）"
                    } else {
                        "❌ 关窗把引擎停掉了 —— 用户在游戏里会没有声音！"
                    };
                    let msg = format!(
                        "关窗前 : running={} 设备={} 槽位={}\n\
                         关窗后 : running={} 设备={} 槽位={}  可见={visible} 存在={alive}\n\
                         {verdict}",
                        before.running,
                        before.device,
                        before.slots.len(),
                        after.running,
                        after.device,
                        after.slots.len()
                    );
                    println!("[探针] {msg}");
                    let _ = std::fs::write("/tmp/close_probe.txt", &msg);
                    request_exit(&h);
                });
            }
            Ok(())
        })
        // 关闭窗口**不退出**：隐藏即可。
        // 游戏时窗口会挡视线，但引擎必须继续跑 —— 一旦退出，对方就听不到你了。
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() != "main" {
                    return;
                }
                api.prevent_close();
                handle_main_window_event(window.app_handle(), MainWindowEvent::CloseRequested);
                #[cfg(target_os = "macos")]
                println!("[窗口已隐藏] 引擎继续运行；点 Dock 或菜单栏图标可重新打开");
                #[cfg(not(target_os = "macos"))]
                println!("[窗口已隐藏] 引擎继续运行；点系统托盘图标可重新打开");
            }
        })
        .invoke_handler(tauri::generate_handler![
            begin_playback_operation,
            list_devices,
            guess_output,
            scan_dir,
            pick_dir,
            start,
            stop,
            release,
            revert_to_real_mic,
            play,
            stop_all,
            set_gains,
            set_looping,
            set_slot_gain,
            set_sound_gain,
            add_files,
            preview,
            stop_preview,
            start_remote,
            autostart,
            self_test,
            remote_qr,
            app_info,
            platform_info,
            open_system_settings,
            js_error,
            open_url,
            status,
            voice_status,
            voice_presets,
            voice_set_enabled,
            voice_set_bypass,
            voice_set_params,
            voice_select_preset,
            frontend_ready,
            load_config,
            save_config,
        ])
        .build(tauri::generate_context!())
        .expect("构建界面失败")
        .run(|app, event| match event {
            // Dock 点击会请求恢复，即使另一个窗口可见也要聚焦现有主窗口。
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => {
                handle_main_window_event(app, MainWindowEvent::Reopen);
            }
            tauri::RunEvent::ExitRequested { api, .. }
                if !app.state::<AudioHost>().shutdown_done() =>
            {
                api.prevent_exit();
                request_exit(app);
            }
            _ => {}
        });
}

#[cfg(test)]
mod watchdog_tests {
    use super::AudioWatchdog;

    #[test]
    fn 切换会话后不能沿用旧的停滞次数或进度基线() {
        let mut watchdog = AudioWatchdog::default();
        assert_eq!(watchdog.observe((1, 100, 100, false)), None);
        assert_eq!(watchdog.observe((1, 200, 100, false)), None);
        assert_eq!(watchdog.observe((1, 300, 100, false)), None);
        assert_eq!(watchdog.observe((2, 0, 0, false)), None);
        assert_eq!(watchdog.observe((2, 100, 100, false)), None);
        assert_eq!(watchdog.observe((2, 200, 100, false)), None);
        assert_eq!(watchdog.observe((2, 300, 100, false)), None);
        assert_eq!(
            watchdog.observe((2, 400, 100, false)),
            Some("麦克风采集回调已停止推进")
        );
    }

    #[test]
    fn 释放会话后重新启动要重新累计且持续推进会清掉停滞() {
        let mut watchdog = AudioWatchdog::default();
        watchdog.observe((1, 100, 100, false));
        watchdog.observe((1, 100, 200, false));
        watchdog.observe((1, 100, 300, false));
        assert_eq!(watchdog.observe((0, 0, 0, false)), None);
        assert_eq!(watchdog.observe((2, 0, 0, false)), None);
        assert_eq!(watchdog.observe((2, 0, 100, false)), None);
        assert_eq!(watchdog.observe((2, 0, 200, false)), None);
        assert_eq!(watchdog.observe((2, 100, 300, false)), None);
        assert_eq!(watchdog.observe((2, 100, 400, false)), None);
        assert_eq!(watchdog.observe((2, 100, 500, false)), None);
        assert_eq!(
            watchdog.observe((2, 100, 600, false)),
            Some("音频渲染回调已停止推进")
        );
    }

    #[test]
    fn 回调_panic_即使进度还在推进也会被发现且不会跨会话() {
        let mut watchdog = AudioWatchdog::default();
        watchdog.observe((1, 100, 100, true));
        assert_eq!(watchdog.observe((1, 200, 200, true)), None);
        assert_eq!(watchdog.observe((1, 300, 300, true)), None);
        assert_eq!(
            watchdog.observe((1, 400, 400, true)),
            Some("音频回调里发生过 panic")
        );
        assert_eq!(watchdog.observe((2, 0, 0, false)), None);
        assert_eq!(watchdog.observe((2, 100, 100, false)), None);
    }
}

#[cfg(test)]
mod platform_info_tests {
    // 测试名用中文描述 + 保留环境变量的大写写法（APPDATA / XDG / HOME / LOCALAPPDATA…）。
    // 改成 snake_case 要把缩写降成小写，可读性反而更差 —— 这里 lint 是错的。
    #![allow(non_snake_case)]
    use super::*;
    use app_config::Platform;

    fn text_of(v: &serde_json::Value) -> String {
        // 把整份引导压成一个字符串，方便断言"该说的有没有说"
        serde_json::to_string(v).unwrap()
    }

    #[test]
    fn windows_引导必须说清_VB_CABLE_命名是反的() {
        // 这是 Windows 上最容易卡住人的地方：本程序输出到 CABLE Input，
        // 而游戏选麦克风要选 CABLE Output。不写出来用户会在两个下拉框之间反复试。
        let t = text_of(&platform_info_for(Platform::Windows));
        assert!(t.contains("CABLE Input"), "没提 CABLE Input：{t}");
        assert!(t.contains("CABLE Output"), "没提 CABLE Output：{t}");
        assert!(t.contains("反的"), "没说清命名是反的");
    }

    #[test]
    fn windows_引导必须点出桌面应用那个独立开关() {
        // Windows 的麦克风权限有**两个**开关："麦克风访问"和"允许桌面应用访问"。
        // 上面开了下面没开照样没声音，而这一点几乎没人会想到。
        let t = text_of(&platform_info_for(Platform::Windows));
        assert!(t.contains("桌面应用"), "没提「允许桌面应用访问」");
    }

    #[test]
    fn windows_引导必须提默认通信设备() {
        // Windows 有"默认设备"和"默认通信设备"两套，很多游戏读的是后者
        let t = text_of(&platform_info_for(Platform::Windows));
        assert!(t.contains("默认通信设备"), "没提默认通信设备");
    }

    #[test]
    fn windows_用_VB_CABLE_而不是_BlackHole() {
        let t = text_of(&platform_info_for(Platform::Windows));
        assert!(t.contains("VB-CABLE"), "Windows 上该推 VB-CABLE");
        assert!(!t.contains("BlackHole"), "Windows 上不该出现 BlackHole");
        assert!(!t.contains("brew "), "Windows 上不该出现 brew 命令");
    }

    #[test]
    fn macos_用_BlackHole_而不是_VB_CABLE() {
        let t = text_of(&platform_info_for(Platform::Unix));
        assert!(t.contains("BlackHole"), "macOS 上该推 BlackHole");
        assert!(t.contains("brew install"), "macOS 上该给 brew 命令");
    }

    #[test]
    fn 引导里的设置页键必须在白名单里() {
        // 前端只传语义名，URL 由后端拼。所以"引导里写的键"和"白名单认的键"
        // 必须对得上 —— 对不上的话用户在界面上点按钮会直接报错。
        for p in [Platform::Windows, Platform::Unix] {
            let v = platform_info_for(p);
            let page = v["mic_privacy"]["settings_page"].as_str().unwrap_or("");
            assert!(!page.is_empty(), "{p:?} 没给设置页键");
            assert!(
                settings_url_for(p, page).is_some(),
                "{p:?} 的引导写了设置页「{page}」，但白名单不认这个键"
            );
        }
    }

    #[test]
    fn 白名单拒绝一切非法键与跨平台键() {
        // 这是安全边界：前端只能传语义名，传别的必须被拒
        for bad in [
            "file:///etc/passwd",
            "http://evil.example",
            "../../etc",
            "",
            "microphone/../sound",
            "MICROPHONE", // 大小写敏感，不做模糊匹配
        ] {
            for p in [Platform::Windows, Platform::Unix] {
                assert!(
                    settings_url_for(p, bad).is_none(),
                    "{p:?} 竟然接受了非法键 {bad:?}"
                );
            }
        }
    }

    #[test]
    fn 平台之间不串用_scheme() {
        // Windows 必须拿 ms-settings:，macOS 必须拿 x-apple.*
        // 串了的话点按钮会静默失败
        let w = settings_url_for(Platform::Windows, "microphone").unwrap();
        assert!(w.starts_with("ms-settings:"), "Windows 拿到的是 {w}");
        let m = settings_url_for(Platform::Unix, "microphone").unwrap();
        assert!(m.starts_with("x-apple."), "macOS 拿到的是 {m}");
    }

    #[test]
    fn 不许把系统设置的真实_URL_下发到前端() {
        // 前端拿到 URL 就能自己拼任意地址；下发语义名则由后端把关
        for p in [Platform::Windows, Platform::Unix] {
            let t = text_of(&platform_info_for(p));
            assert!(
                !t.contains("ms-settings:"),
                "{p:?} 把 ms-settings: 下发到前端了"
            );
            assert!(
                !t.contains("x-apple.systempreferences:"),
                "{p:?} 把 x-apple.systempreferences: 下发到前端了"
            );
        }
    }
}

#[cfg(test)]
mod sound_item_dto_tests {
    use super::*;

    #[test]
    fn loop_preference_is_a_boolean_in_the_gui_config_dto() {
        for looping in [false, true] {
            let config = Config::parse(&format!(
                "looping = {looping}\nsound_files = [\"fixture.wav\"]\n"
            ));
            assert_eq!(
                saved_config_dto(&config)["looping"],
                serde_json::json!(looping)
            );
        }
        assert_eq!(
            saved_config_dto(&Config::default())["looping"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn structured_tags_ids_and_empty_hotkey_survive_gui_json_roundtrip() {
        let item = app_config::SoundEntry {
            id: "stable-sound-id".into(),
            path: PathBuf::from("声音,a.wav"),
            name: "A/B 鼓点".into(),
            gain_db: -12.0,
            hotkey: String::new(),
            tags: vec![
                "a,b".into(),
                "x|y".into(),
                "鼓;点".into(),
                "；中文，标签".into(),
            ],
        };
        let dto = sound_item_dto(&item);
        let saved = parse_sound_items(&[serde_json::from_str(&dto.to_string()).unwrap()]);
        assert_eq!(saved, vec![item]);
    }

    #[test]
    fn legacy_tag_text_remains_readable() {
        let parsed = parse_sound_items(&[serde_json::json!({
            "file": "legacy.wav", "tags": " old, second| third; ",
        })]);
        assert_eq!(parsed[0].tags, ["old", "second", "third"]);
    }
}
