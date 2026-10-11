//! 音效板 GUI —— Tauri 外壳。
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

use std::path::PathBuf;

use app_config::Config;
use host::{AudioHost, DeviceDto, StartConfig, StatusDto};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, State};

type Res<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
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
fn start(app: tauri::AppHandle, host: State<'_, AudioHost>, cfg: StartConfig) -> Res<()> {
    // 启动失败**一定要进日志**。打包后的 App 没有控制台，错误只回到前端的话，
    // 用户看到的是一个弹一下就消失的 toast，我们事后什么都查不到。
    // （排查"点了启动没反应"时正是卡在这里：错误只在前端，日志里一片正常。）
    let r = start_inner(app, host, cfg);
    if let Err(e) = &r {
        println!("[启动] ❌ 失败: {e}");
    }
    r
}

fn start_inner(app: tauri::AppHandle, host: State<'_, AudioHost>, cfg: StartConfig) -> Res<()> {
    // 快路（设备没变就只重新激活）在 host.start 里面 —— 放那儿冒烟测试也能覆盖到。
    println!(
        "[启动] 前端送来的参数：monitor={} monitor_device={:?} output={:?} 文件 {} 个",
        cfg.monitor,
        cfg.monitor_device,
        cfg.output_device,
        cfg.files.len()
    );
    host.start(cfg, Some(&app)).map_err(|e| {
        let msg = format!("{e:#}");
        host.record_error(msg.clone());
        msg
    })
}

/// 停止音效 —— **但继续转发麦克风**（队友照常听得到你说话）。
///
/// 语义是「关掉音效」而不是「把我变成哑巴」：游戏的麦克风一直指着虚拟声卡，
/// 一旦连转发也断了，虚拟声卡里就是一片死寂，队友会突然听不到你。
#[tauri::command]
fn stop(host: State<'_, AudioHost>) {
    host.stop();
}

/// 彻底释放音频设备（换设备、退出时用）。
///
/// ⚠️ 释放后虚拟声卡里就没人写了 —— 游戏的麦克风如果还指着它，队友会听不到你。
#[tauri::command]
fn release(host: State<'_, AudioHost>) {
    host.release();
}

/// 一键切回真实麦克风。
///
/// **为什么需要**：游戏的麦克风一旦指向虚拟声卡，"队友能不能听到你"就完全由本程序决定了。
/// 用户不想用了（或要退出）时，如果只关掉本程序，游戏那边读到的是**一片死寂** ——
/// 而这个设置藏在游戏里，很多人根本不知道要去改。
///
/// 这里能做的是把**系统默认输入设备**切回真实麦克风（并在切换前释放本程序的设备占用），
/// 对"跟随系统默认"的程序立刻生效。**但显式指定了设备的游戏改不了** ——
/// 所以返回里带上"有没有真的改成功"和需要手动改的提醒。
#[tauri::command]
fn revert_to_real_mic(host: State<'_, AudioHost>) -> Res<serde_json::Value> {
    #[cfg(target_os = "macos")]
    {
        let (ok, device, current) = host.revert_to_real_mic().map_err(err)?;
        Ok(serde_json::json!({
            "ok": ok,
            "device": device,
            "current": current,
            // 系统默认改了还不够 —— 显式指定了麦克风的游戏要用户自己改
            "note": "系统默认麦克风已切回。如果游戏/语音软件里是手动指定的麦克风，\
                     还需要进去把它也改回真实麦克风，否则队友仍然听不到你。",
        }))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = host;
        Err("该功能目前只在 macOS 上实现".into())
    }
}

#[tauri::command]
fn play(host: State<'_, AudioHost>, slot: usize) -> Res<bool> {
    host.play(slot).map_err(err)
}

#[tauri::command]
fn stop_all(host: State<'_, AudioHost>) -> Res<()> {
    host.stop_all().map_err(err)
}

#[tauri::command]
fn set_gains(host: State<'_, AudioHost>, mic_db: f32, sfx_db: f32) {
    host.set_gains(mic_db, sfx_db);
}

/// 调整单个音效的音量。
#[tauri::command]
fn set_slot_gain(host: State<'_, AudioHost>, slot: usize, db: f32) -> Res<bool> {
    host.set_slot_gain(slot, db).map_err(err)
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
                       而游戏里选麦克风要选 **CABLE Output**。名字看着别扭，但就是这样。",
            "alts": "其他选择：VoiceMeeter（免费，功能更多但更复杂）、Virtual Audio Cable（付费）。",
            "mic_privacy": {
                "title": "麦克风收不到声音？",
                "steps": [
                    "打开 设置 → 隐私和安全性 → 麦克风，确认「麦克风访问」是**开**的",
                    "往下拉到「允许桌面应用访问你的麦克风」—— **这一条最容易漏**。\
                     音效板属于桌面应用，这个开关和上面那个是分开的，上面开了下面没开照样没声音。",
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
                    "确认「音效板」在列表里并且是**勾上**的",
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
async fn self_test(output_device: Option<String>) -> Res<serde_json::Value> {
    let b = backend_cpal::CpalBackend::new();
    let r = tauri::async_runtime::spawn_blocking(move || {
        selftest::run_with_retry(&b, output_device, 440.0, 2.0, 3)
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
fn preview(host: State<'_, AudioHost>, file: String, gain_db: f32) -> Res<()> {
    host.preview(PathBuf::from(file), gain_db).map_err(err)
}

#[tauri::command]
fn stop_preview(host: State<'_, AudioHost>) {
    host.stop_preview();
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

/// 前端加载完成后主动上报 —— 用来证明 WebView 真的渲染并执行了 JS。
/// `--probe` 模式下会把它打印出来，供自动化验证。
/// 读取上次保存的配置（音效目录、设备、增益、热键）。
#[tauri::command]
fn load_config() -> serde_json::Value {
    let c = Config::load();
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
                    "tags": "",
                })
            })
            .collect()
    } else {
        c.sound_items()
            .iter()
            .map(|it| {
                serde_json::json!({
                    "file": it.path.display().to_string(),
                    "name": it.name,
                    "gain": it.gain_db,
                    "tags": it.tags.join(","),
                })
            })
            .collect()
    };
    serde_json::json!({
        "items": items,
        "monitor": c.monitor,
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
    monitor_device: Option<String>,
    dir: String,
    input_device: Option<String>,
    output_device: Option<String>,
    buffer_ms: u32,
    mic_gain_db: f32,
    sfx_gain_db: f32,
    hotkeys: String,
) -> Result<(), String> {
    let mut c = Config::load();
    c.set("sound_dirs", &dir).map_err(err)?;
    c.set("monitor", if monitor { "true" } else { "false" })
        .map_err(err)?;
    c.set("monitor_device", monitor_device.as_deref().unwrap_or(""))
        .map_err(err)?;

    // 音效库：整组覆盖（路径/音量/名字/标签），保持并行数组一一对应
    let parsed: Vec<app_config::SoundItem> = items
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
            let tags: Vec<String> = v
                .get("tags")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .split([',', '|', ';'])
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            Some(app_config::SoundItem {
                path,
                name,
                gain_db: gain,
                tags,
            })
        })
        .collect();
    c.set_sound_items(&parsed);
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
    c.save().map(|_| ()).map_err(err)
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
    let stop = MenuItem::with_id(app, "stop", "全部停止音效", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    // 「切回真实麦克风」目前只有 macOS 实现（改的是系统默认输入设备）。
    // Windows 上要改的是"默认通信设备"，那是另一套 API —— 见 TODO。
    #[cfg(target_os = "macos")]
    let revert = MenuItem::with_id(
        app,
        "revert",
        "切回真实麦克风（用完不用进游戏改设置）",
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
        .tooltip("音效板")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id.as_ref() {
            "show" => show_window(app),
            "stop" => {
                let _ = app.state::<AudioHost>().stop_all();
                println!("[菜单栏] 已全部停止");
            }
            #[cfg(target_os = "macos")]
            "revert" => {
                // 用户点这个就说明他不想再用虚拟麦克风了 —— 把系统默认输入切回真实麦克风。
                // ⚠️ 显式指定了麦克风的游戏改不了，要提醒用户自己进去改。
                match app.state::<AudioHost>().revert_to_real_mic() {
                    Ok((true, dev, _)) => println!("[菜单栏] 已切回真实麦克风：{dev}"),
                    Ok((false, dev, cur)) => {
                        println!("[菜单栏] ⚠ 切到「{dev}」没生效，当前仍是「{cur}」")
                    }
                    Err(e) => println!("[菜单栏] ❌ 切回失败: {e:#}"),
                }
            }
            "quit" => {
                app.state::<AudioHost>().stop();
                app.exit(0);
            }
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
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
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

fn main() {
    // ⚠️ 必须在**主线程**登记一次（也就是这里）。
    //
    // Windows 上热键的隐藏窗口必须建在跑消息循环的线程上 —— 拿不准的话
    // `is_main_thread()` 返回 false，会把绑定调度到主线程；但要是
    // 主线程压根没登记，那个调度判断就没有依据。
    // 不登记的代价是热键**静默失效**（按了没反应，也不报错）。
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
                    "音效板已经在运行了。\n\
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
            // 而用户正在游戏里，根本发现不了（他只会觉得"队友怎么不理我"）。
            // 每 2 秒检查一次，出事立刻报警。
            {
                let h = app.handle().clone();
                std::thread::Builder::new()
                    .name("hotplug".into())
                    .spawn(move || {
                        use tauri::Manager;
                        let mut last = String::new();
                        loop {
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            let host = h.state::<AudioHost>();
                            let Some((in_lost, out_lost, name)) = host.check_devices() else {
                                if !last.is_empty() {
                                    host.set_device_lost(None);
                                    last.clear();
                                }
                                continue;
                            };
                            let key = format!("{in_lost}/{out_lost}/{name}");
                            if key == last {
                                continue; // 同一件事不重复报
                            }
                            last = key;

                            if out_lost {
                                let msg = format!(
                                    "虚拟声卡「{name}」不见了 —— 队友现在听不到你。\
                                     请重新选一个输出设备，再点「启动」。"
                                );
                                println!("[热插拔] ❌ {msg}");
                                host.set_device_lost(Some(msg));
                            } else if in_lost {
                                println!(
                                    "[热插拔] ⚠ 麦克风「{name}」已断开，自动切到系统默认麦克风…"
                                );
                                match host.recover_input_device(Some(&h)) {
                                    Ok(old) => {
                                        println!("[热插拔] ✅ 已从「{old}」切到系统默认麦克风");
                                        host.set_device_lost(Some(format!(
                                            "麦克风「{old}」已断开，已自动切到系统默认麦克风。"
                                        )));
                                    }
                                    Err(e) => {
                                        println!("[热插拔] ❌ 自动恢复失败: {e:#}");
                                        host.set_device_lost(Some(format!(
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
            // 用户正在游戏里，队友突然听不到他，屏幕上却什么提示都没有。
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
                        let mut last = 0u64;
                        let mut stalled = 0u32;
                        loop {
                            std::thread::sleep(std::time::Duration::from_secs(1));
                            let host = h.state::<AudioHost>();
                            if !host.audio_alive() {
                                // 引擎本来就没跑 —— 不算"卡死"，清掉报警
                                stalled = 0;
                                last = 0;
                                if host.status().audio_stalled.is_some() {
                                    host.set_audio_stalled(None);
                                }
                                continue;
                            }
                            let frames = host.rendered_frames();
                            let panicked = backend_cpal::engine_panicked();
                            if frames == last || panicked {
                                stalled += 1;
                                if stalled >= 3 {
                                    let why = if panicked {
                                        "音频回调里发生过 panic"
                                    } else {
                                        "音频回调已停止推进（流可能已经死了）"
                                    };
                                    println!("[看门狗] ⚠ {why} —— 自动重启音频");
                                    host.set_audio_stalled(Some(format!("{why}，正在自动重启…")));
                                    match host.restart_audio(Some(&h)) {
                                        Ok(()) => {
                                            println!("[看门狗] ✅ 音频已自动恢复");
                                            host.set_audio_stalled(Some(format!(
                                                "{why} —— 已自动恢复，如果队友那边断过，让他们重新连一下"
                                            )));
                                        }
                                        Err(e) => {
                                            println!("[看门狗] ❌ 自动恢复失败: {e:#}");
                                            host.set_audio_stalled(Some(format!(
                                                "{why}，且自动恢复失败：{e}。请手动点「启动」重试"
                                            )));
                                        }
                                    }
                                    stalled = 0;
                                    last = host.rendered_frames();
                                    continue;
                                }
                            } else {
                                stalled = 0;
                            }
                            last = frames;
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
                    h.exit(0);
                });
            }
            Ok(())
        })
        // 关闭窗口**不退出**：隐藏即可。
        // 游戏时窗口会挡视线，但引擎必须继续跑 —— 一旦退出，队友就听不到你了。
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                println!("[窗口已隐藏] 引擎继续运行；点菜单栏图标可重新打开");
            }
        })
        .invoke_handler(tauri::generate_handler![
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
            set_slot_gain,
            add_files,
            preview,
            stop_preview,
            start_remote,
            autostart,
            self_test,
            remote_qr,
            platform_info,
            open_system_settings,
            js_error,
            open_url,
            status,
            frontend_ready,
            load_config,
            save_config,
        ])
        .run(tauri::generate_context!())
        .expect("启动界面失败");
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
