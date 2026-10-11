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
        if let Ok(h) = std::env::var("HOME") {
            return format!("{h}/{rest}");
        }
    }
    p.to_string()
}

// ─────────────────────────────────────────── 音频控制

#[tauri::command]
fn start(app: tauri::AppHandle, host: State<'_, AudioHost>, cfg: StartConfig) -> Res<()> {
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
        .add_filter("音频", &["mp3", "wav", "ogg", "m4a", "flac", "aac", "aiff", "caf", "opus"])
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
    c.set(
        "monitor_device",
        monitor_device.as_deref().unwrap_or(""),
    )
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
            Some(app_config::SoundItem { path, name, gain_db: gain, tags })
        })
        .collect();
    c.set_sound_items(&parsed);
    c.set("input_device", input_device.as_deref().unwrap_or("none")).map_err(err)?;
    c.set("output_device", output_device.as_deref().unwrap_or("none")).map_err(err)?;
    c.set("buffer_ms", &buffer_ms.to_string()).map_err(err)?;
    c.set("mic_gain_db", &mic_gain_db.to_string()).map_err(err)?;
    c.set("sfx_gain_db", &sfx_gain_db.to_string()).map_err(err)?;
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
    let revert = MenuItem::with_id(
        app,
        "revert",
        "切回真实麦克风（用完不用进游戏改设置）",
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "退出（停止音频）", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &stop, &revert, &sep, &quit])?;

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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,symphonia=off")).init();

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
                std::env::var("HOME")
                    .map(|h| PathBuf::from(h).join("Sounds"))
                    .unwrap_or_else(|_| PathBuf::from("."))
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
                                println!("[热插拔] ⚠ 麦克风「{name}」已断开，自动切到系统默认麦克风…");
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

            // 一启动就问麦克风权限，别等到用户点「启动」时才弹窗
            host::request_mic_permission();

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
                        before.running, before.device, before.slots.len(),
                        after.running, after.device, after.slots.len()
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
