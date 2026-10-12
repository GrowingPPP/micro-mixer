//! `mixer` —— 命令行版：把音频播进麦克风。
//!
//! 这是产品的**无头内核**，UI 之后套在上面。
//!
//! ```text
//! mixer list                                                 # 列出设备
//! mixer passthrough                                          # 麦克风透传（验证链路）
//! mixer play --file 笑.mp3 --file 警报.mp3 --hotkeys F1,F2     # 全局热键触发的麦克风混音助手
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use audio_core::device::{AudioBackend, Direction, StreamFormat, StreamHealth};
use audio_session::{
    resolve_device, DumpOptions, Library, MonitorOptions, Playback, Session, SessionOptions,
    SoundSpec,
};
use backend_cpal::CpalBackend;
use hotkeys::Hotkeys;

use app_config::Config;
use remote as remote_ctl;

fn take_monitor_warning(health: &StreamHealth, warned: &mut bool) -> Option<String> {
    let error = health.take_error();
    if *warned {
        return None;
    }
    let reason = error.or_else(|| health.panicked().then(|| "监听音频回调发生 panic".into()))?;
    *warned = true;
    Some(format!(
        "[监听] ⚠ 监听音频异常：{reason}。自己听到的音效可能中断；麦克风与虚拟声卡主链路继续运行。"
    ))
}

#[derive(Parser, Debug)]
#[command(
    name = "mixer",
    version,
    about = "将本地音频与麦克风人声混合，让对方听到",
    long_about = "将本地音频与麦克风人声混合，通过虚拟音频设备传给语音、会议、直播或游戏中的对方。\n\n日常使用只需要：mixer play（读 ~/.config/mixer/config.toml）"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 列出所有音频设备（含虚拟声卡）
    List,

    /// 环境体检：设备、虚拟声卡、配置、文件、权限有没有问题
    ///
    /// 比 selftest 更靠前一步 —— selftest 验证"链路通不通"，
    /// doctor 检查"该有的东西在不在"。用户遇到"没声音"时先跑这个，
    /// 大多数情况不用进游戏就能看出问题。
    Doctor,

    /// 端到端自检：送已知信号进虚拟设备，从环回录回来验证整条链路
    Selftest {
        /// 虚拟设备名，省略则自动探测
        #[arg(short, long)]
        device: Option<String>,
        /// 环回录音端点，省略时自动匹配；VB-CABLE 通常为 CABLE Output
        #[arg(long)]
        capture: Option<String>,
        /// 测试音频率
        #[arg(long, default_value_t = 440.0)]
        freq: f32,
        /// 播放时长（秒）
        #[arg(long, default_value_t = 2.0)]
        secs: f32,
    },

    /// 麦克风 → 虚拟麦克风 透传（不混音，用于验证链路）
    Passthrough {
        #[arg(short, long)]
        input: Option<String>,
        #[arg(short, long)]
        output: Option<String>,
        /// 缓冲水位（毫秒）。越大越稳，延迟越高
        #[arg(long, default_value_t = 20)]
        buffer_ms: u32,
        /// 运行秒数，省略则一直跑到 Ctrl-C
        #[arg(long)]
        duration: Option<u64>,
    },

    /// 麦克风混音助手：麦克风与当前音效实时混音，全局热键触发（**不传参数就读配置**）
    Play {
        /// 音频文件，可重复。第 i 个文件对应第 i 个热键
        #[arg(short, long)]
        file: Vec<PathBuf>,
        /// 音效文件夹：自动加载其中所有音频并按文件名排序分配热键
        #[arg(short = 'd', long)]
        dir: Option<PathBuf>,
        /// 热键列表，逗号分隔（F1,F2,Cmd+Shift+A）。省略则用配置或自动 F1..Fn
        #[arg(short = 'k', long)]
        hotkeys: Option<String>,
        #[arg(short, long)]
        input: Option<String>,
        #[arg(short, long)]
        output: Option<String>,
        /// 循环播放所有音效
        #[arg(long)]
        r#loop: bool,
        /// 麦克风增益（dB，可为负）
        #[arg(long, allow_hyphen_values = true)]
        mic_gain: Option<f32>,
        /// 音效总增益（dB，可为负）
        #[arg(long, allow_hyphen_values = true)]
        sfx_gain: Option<f32>,
        /// 峰值归一化到 -3 dBFS 后再混音
        #[arg(long, default_value_t = true)]
        normalize: bool,
        #[arg(long)]
        buffer_ms: Option<u32>,
        #[arg(long)]
        duration: Option<u64>,
        /// 开启手机遥控：局域网内用浏览器打开 http://<电脑IP>:<端口> 触发音效。
        /// 省略端口则用 8765。热键被游戏吞掉时的兜底通路。
        #[arg(long, num_args = 0..=1, default_missing_value = "8765")]
        remote: Option<u16>,
        /// 遥控监听地址，默认 0.0.0.0（手机才连得上）
        #[arg(long, default_value = "0.0.0.0")]
        remote_bind: String,
    },

    /// 查看或修改配置（~/.config/mixer/config.toml）
    Config {
        /// `set <项> <值>` 修改并保存；`path` 打印路径；不带参数则打印当前配置
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// 热键测试：按一下绑定的键，看系统有没有把按键送到本程序
    ///
    /// 开游戏之前先跑这个 —— 全屏游戏吞掉全局热键是本产品最大的未知风险。
    HotkeyTest {
        /// 热键列表，逗号分隔。省略则用配置里的，再省略则用 F1..F5
        #[arg(short = 'k', long)]
        keys: Option<String>,
    },
}

fn main() -> Result<()> {
    // macOS Carbon 管理器要求主线程；Windows 管理器由 hotkeys 的消息线程拥有。
    hotkeys::note_main_thread();
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,symphonia=off"),
    )
    .init();
    let cli = Cli::parse();
    let backend = CpalBackend::new();

    match cli.cmd {
        Cmd::List => list_devices(&backend),
        Cmd::Doctor => cmd_doctor(&backend),
        Cmd::Selftest {
            device,
            capture,
            freq,
            secs,
        } => cmd_selftest(&backend, device, capture, freq, secs),
        Cmd::Config { args } => cmd_config(&args),
        Cmd::HotkeyTest { keys } => hotkey_test(keys),
        Cmd::Passthrough {
            input,
            output,
            buffer_ms,
            duration,
        } => {
            let cfg = Config::load();
            run(
                &backend,
                RunOptions {
                    input: input.or(cfg.input_device),
                    output: output.or(cfg.output_device),
                    specs: Vec::new(),
                    looping: false,
                    mic_gain: 0.0,
                    sfx_gain: 0.0,
                    normalize: false,
                    buffer_ms,
                    duration,
                    remote: None,
                    monitor: false,
                    monitor_device: None,
                },
            )
        }
        Cmd::Play {
            file,
            dir,
            hotkeys,
            input,
            output,
            r#loop,
            mic_gain,
            sfx_gain,
            normalize,
            buffer_ms,
            duration,
            remote,
            remote_bind,
        } => {
            let cfg = Config::load();

            let mut files = file;
            if let Some(d) = dir {
                files.extend(collect_audio_files(&d)?);
            }
            let specs = playback_specs(&cfg, &files, hotkeys.as_deref())?;
            if specs.is_empty() {
                anyhow::bail!(
                    "没有音效。可用 mixer play --file 笑.mp3 或 mixer play --dir ~/Sounds；也可先设置 sound_dirs。"
                );
            }
            run(
                &backend,
                RunOptions {
                    input: input.or(cfg.input_device),
                    output: output.or(cfg.output_device),
                    specs,
                    looping: r#loop,
                    mic_gain: mic_gain.or(cfg.mic_gain_db).unwrap_or(0.0),
                    sfx_gain: sfx_gain.or(cfg.sfx_gain_db).unwrap_or(-6.0),
                    normalize,
                    buffer_ms: buffer_ms.or(cfg.buffer_ms).unwrap_or(20),
                    duration,
                    remote: remote.map(|port| (port, remote_bind)),
                    monitor: cfg.monitor,
                    monitor_device: cfg.monitor_device,
                },
            )
        }
    }
}

// ─────────────────────────────────────────── config

fn cmd_config(args: &[String]) -> Result<()> {
    let path = Config::path();
    match args.first().map(|s| s.as_str()) {
        None => {
            let c = Config::load();
            println!("\n配置文件: {}", path.display());
            if !path.exists() {
                println!("（尚未创建 —— 用 `mixer config set <项> <值>` 创建）");
            }
            println!("\n当前配置:");
            let text = c.to_text();
            let body: Vec<&str> = text
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#'))
                .collect();
            if body.is_empty() {
                println!("  （全部为默认值）");
            } else {
                for l in body {
                    println!("  {l}");
                }
            }
            println!(
                "\n可设置项: input_device / output_device / buffer_ms / mic_gain_db / \
                 sfx_gain_db / sound_dirs / hotkeys"
            );
            println!("用法: mixer config set output_device \"VB-Cable\"\n");
            Ok(())
        }
        Some("path") => {
            println!("{}", path.display());
            Ok(())
        }
        Some("set") => {
            // args = ["set", 项, 值…] —— 值可能含空格，所以按 3 段起算
            if args.len() < 3 {
                anyhow::bail!("用法: mixer config set <项> <值>");
            }
            let mut c = Config::load();
            c.set(&args[1], &args[2..].join(" "))?;
            let p = c.save()?;
            println!("✅ 已写入 {}", p.display());
            Ok(())
        }
        Some(other) => anyhow::bail!("未知子命令「{other}」。可用: set / path（或不带参数查看）"),
    }
}

// ─────────────────────────────────────────── hotkey test

fn hotkey_test(keys: Option<String>) -> Result<()> {
    let cfg = Config::load();
    let specs: Vec<String> = match keys {
        Some(s) => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        None if !cfg.hotkeys.is_empty() => cfg.hotkeys,
        None => (1..=5).map(|i| format!("F{i}")).collect(),
    };

    println!("\n═══ 热键测试 ═══");
    println!("  将绑定: {}\n", specs.join(", "));

    // macOS 的硬性要求：管理器必须在主线程创建，且主线程要跑运行循环。
    // 不满足的话热键会**静默失效** —— 用户只会看到"按了没反应"。
    if !hotkeys::is_main_thread() {
        anyhow::bail!(
            "热键管理器必须在主线程创建（当前不在主线程）。\
             macOS 的 Carbon 热键事件由主线程运行循环派发，在别的线程创建会静默失效。"
        );
    }
    println!("  ✓ 热键事件线程检查通过");

    let hk = Hotkeys::bind_to_slots(&specs).context("热键绑定失败")?;
    for (spec, slot) in hk.bindings() {
        println!("  {:<18} → 槽位 {}", spec, slot + 1);
    }
    println!("\n  现在按一下上面任意一个键。本程序会打印收到的时间。");
    println!();
    #[cfg(target_os = "macos")]
    {
        println!("  ⚠️ MacBook 键盘注意：F1~F12 默认是媒体键（亮度/音量），");
        println!("     必须按住 fn 才是真正的 F1。若 fn+F1 仍无反应，可以：");
        println!("       · 系统设置 → 键盘 → 键盘快捷键 → 功能键 → 把 F1/F2 用作标准功能键");
        println!("       · 或者改用组合键，例如 --keys \"Cmd+Shift+1,Cmd+Shift+2\"");
    }
    println!("  先在这里试；再到游戏里（全屏）试 —— 有些游戏会吞掉全局热键。");

    // 轮询放**工作线程** —— 主线程必须腾出来跑应用事件循环。
    //
    // 实测结论：只跑裸 CFRunLoop（CFRunLoopRunInMode）**收不到** Carbon 热键事件，
    // 必须是完整的应用事件循环。GUI 之所以一直正常，就是因为 tao 在跑 NSApplication。
    let names = specs;
    // ⚠️ 只把 **poller** 移进线程，`hk` 本身留在主线程作用域里活着。
    //
    // macOS 管理器保持在主线程；Windows 的原生窗口已由专属线程管理。
    let poller = hk.poller();
    std::thread::Builder::new()
        .name("hotkey-poll".into())
        .spawn(move || {
            let t0 = Instant::now();
            let mut count = 0usize;
            loop {
                if let Some(slot) = poller.poll() {
                    count += 1;
                    println!(
                        "  ✅ [{:>6.2}s] 收到 → {:<18} 槽位 {}   （累计 {} 次）",
                        t0.elapsed().as_secs_f32(),
                        names.get(slot).map(|s| s.as_str()).unwrap_or("?"),
                        slot + 1,
                        count
                    );
                    if count == 1 {
                        println!("     热键工作正常。下一步：切到游戏里（全屏）再按一次。");
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        })?;

    println!("  正在运行平台热键事件循环。");
    println!("  Ctrl-C 退出。\n");
    hotkeys::run_app_event_loop();
    Ok(())
}

// ─────────────────────────────────────────── list

/// 端到端自检：跑一遍链路，把结构化结果排版成表格。
/// 环境体检。
///
/// 和 `selftest` 的分工：selftest 验证**链路通不通**（要开音频、要几秒），
/// doctor 检查**该有的东西在不在**（秒出、不动音频设备）。
/// 用户说"没声音"时先跑这个 —— 大多数问题（虚拟声卡没装、配置里的设备没了、
/// 音效文件不存在）在这一步就能看出来，不用进游戏试。
fn cmd_doctor(backend: &CpalBackend) -> Result<()> {
    use audio_core::device::is_virtual_name;

    let cfg = app_config::Config::load();
    let mut problems: Vec<String> = Vec::new();

    println!("\n  ═══ 环境体检 ═══\n");

    // ── 平台 ──
    let os = if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else {
        "其他"
    };
    println!("  平台      : {os}");
    println!("  配置文件  : {}", app_config::Config::path().display());

    // ── 设备 ──
    let devs = backend.devices().unwrap_or_default();
    let inputs: Vec<_> = devs
        .iter()
        .filter(|d| d.direction == Direction::Input)
        .collect();
    let outputs: Vec<_> = devs
        .iter()
        .filter(|d| d.direction == Direction::Output)
        .collect();
    println!("  输入设备  : {} 个", inputs.len());
    println!("  输出设备  : {} 个", outputs.len());

    // 虚拟声卡是整件事的前提 —— 没有它一切都免谈
    let virt_out: Vec<_> = outputs
        .iter()
        .filter(|d| is_virtual_name(&d.name))
        .collect();
    let virt_in: Vec<_> = inputs.iter().filter(|d| is_virtual_name(&d.name)).collect();
    if virt_out.is_empty() || virt_in.is_empty() {
        println!("  ❌ 虚拟声卡: 没找到");
        problems.push(format!(
            "没装虚拟声卡。{}",
            if cfg!(target_os = "windows") {
                "去 vb-audio.com/Cable 下载 VB-CABLE，**以管理员身份**装完**重启**。\
                 注意它的命名是反的：本程序输出到 CABLE Input，游戏选麦克风要选 CABLE Output。"
            } else {
                "推荐 BlackHole：brew install --cask blackhole-2ch，装完重启。"
            }
        ));
    } else {
        let names: Vec<&str> = virt_out.iter().map(|d| d.name.as_str()).collect();
        println!("  ✅ 虚拟声卡: {}", names.join(" / "));
    }

    // ── 配置里的设备还在不在 ──
    //
    // 这是最常见的"上次还好好的、这次没声音"：耳机拔了、虚拟声卡重装后换了名字。
    if let Some(want) = &cfg.output_device {
        let ok = outputs.iter().any(|d| &d.name == want);
        if ok {
            println!("  ✅ 配置的输出设备「{want}」在线");
        } else {
            println!("  ❌ 配置的输出设备「{want}」**不在了**");
            problems.push(format!(
                "配置里写的输出设备「{want}」已经不存在（拔了？重装了？）。\
                 打开界面重新选一个，或跑 `mixer config set output_device <名字>`。"
            ));
        }
    }
    if let Some(want) = &cfg.input_device {
        if inputs.iter().any(|d| &d.name == want) {
            println!("  ✅ 配置的麦克风「{want}」在线");
        } else {
            println!("  ⚠ 配置的麦克风「{want}」不在了 —— 会自动回退到系统默认");
            problems.push(format!(
                "配置里的麦克风「{want}」不在了。程序会自动回退到系统默认麦克风，\
                 但建议重新选一次（`mixer config set input_device <名字>`）。"
            ));
        }
    }

    // ── 音效文件 ──
    let files: Vec<_> = if cfg.sound_files.is_empty() {
        cfg.collect_sounds().unwrap_or_default()
    } else {
        cfg.sound_files.clone()
    };
    let missing: Vec<_> = files.iter().filter(|p| !p.is_file()).collect();
    println!(
        "  音效文件  : {} 个（缺 {} 个）",
        files.len(),
        missing.len()
    );
    for m in missing.iter().take(5) {
        println!("            ✗ {}", m.display());
    }
    if files.is_empty() {
        problems.push("一个音效文件都没有。界面上点「添加音频」导入几个。".into());
    } else if !missing.is_empty() {
        problems.push(format!(
            "有 {} 个音效文件找不到了（移动或删了？）。界面上对应的卡片会显示「打不开」。",
            missing.len()
        ));
    }

    // ── 热键 ──
    if cfg.hotkeys.is_empty() {
        problems.push("没配热键 —— 游戏里就没法触发了。界面上给槽位设一个（比如 F1）。".into());
    } else {
        println!("  ✅ 热键    : {}", cfg.hotkeys.join(", "));
    }

    // ── 结论 ──
    println!();
    if problems.is_empty() {
        println!("  ✅ 没发现问题。下一步跑 `mixer selftest` 验证音频链路。\n");
    } else {
        println!("  发现 {} 个问题：", problems.len());
        for (i, p) in problems.iter().enumerate() {
            println!("\n   {}. {}", i + 1, p);
        }
        println!();
    }
    Ok(())
}

fn cmd_selftest(
    backend: &CpalBackend,
    device: Option<String>,
    capture: Option<String>,
    freq: f32,
    secs: f32,
) -> Result<()> {
    // 自动重试：这块虚拟声卡偶发抽风，重开一次流通常就好
    let r = selftest::run_with_capture_retry(backend, device, capture, freq, secs, 3)?;

    println!("\n═══ 端到端自检 ═══");
    println!("  虚拟设备 : {}", r.device);
    println!(
        "  送入信号 : {} Hz 正弦，{} 秒（@{}Hz）",
        r.freq_sent, r.secs_sent, r.sample_rate
    );
    println!("  录音来源 : 已配对的环回录音端点\n");
    let peak_txt = if r.captured_peak_db.is_finite() {
        format!("{:.1} dBFS", r.captured_peak_db)
    } else {
        "全零".to_string()
    };
    println!(
        "  诊断：输出 {} 帧（约 {:.2}s）  采集 {} 样本（{:.2}s）  环回峰值 {}  采集丢包 {} 次",
        r.played_frames,
        r.played_frames as f32 / r.sample_rate as f32,
        r.captured_samples,
        r.captured_samples as f32
            / r.capture_format.ch() as f32
            / r.capture_format.sample_rate as f32,
        peak_txt,
        r.capture_drops
    );

    // 只在**最终通过**时才说"重试成功" —— 全部失败时 attempts 也是 3，
    // 那时说"才成功"是自相矛盾的（这个 bug 写出来就踩了一次）。
    if r.attempts > 1 && r.passed() {
        println!(
            "  ⚠ 第 {} 次尝试才成功（设备开流时偶发异常，已自动重开流）",
            r.attempts
        );
    } else if r.attempts > 1 {
        println!("  （已自动重试 {} 次，均未通过）", r.attempts);
    }
    println!("\n── 结果 ──");
    for c in &r.checks {
        println!(
            "  {} {:<6} 实测 {:<16} 期望 {:<16}",
            if c.ok { "✅" } else { "❌" },
            c.name,
            c.got,
            c.want
        );
    }
    println!();
    for a in &r.advice {
        println!("  ⚠ {a}");
    }
    if r.passed() {
        println!("\n  ✅ {}", r.summary());
        Ok(())
    } else {
        println!("\n  ❌ {}", r.summary());
        std::process::exit(1);
    }
}

fn list_devices(b: &CpalBackend) -> Result<()> {
    let devs = b.devices()?;
    let din = b.default_input();
    let dout = b.default_output();
    let virt = b.guess_virtual_output();

    println!("\n后端: {}\n", b.backend_name());
    for dir in [Direction::Input, Direction::Output] {
        println!("── {}设备 ──", dir.label());
        for d in devs.iter().filter(|d| d.direction == dir) {
            let mut tags = Vec::new();
            let is_default = match dir {
                Direction::Input => din.as_ref() == Some(&d.id),
                Direction::Output => dout.as_ref() == Some(&d.id),
            };
            if is_default {
                tags.push("默认");
            }
            if virt.as_ref() == Some(&d.id) {
                tags.push("虚拟声卡");
            }
            let tag = if tags.is_empty() {
                String::new()
            } else {
                format!("  [{}]", tags.join(" / "))
            };
            println!(
                "  {:<44} {}Hz/{}ch{}",
                d.name, d.default_sample_rate, d.max_channels, tag
            );
            println!("    标识: {}", d.id);
        }
        println!();
    }
    println!("提示：同名端点用标识选择。VB-CABLE 输出为 CABLE Input，录音为 CABLE Output。\n");
    Ok(())
}

// ─────────────────────────────────────────── 主流程

struct RunOptions {
    input: Option<String>,
    output: Option<String>,
    specs: Vec<SoundSpec>,
    looping: bool,
    mic_gain: f32,
    sfx_gain: f32,
    normalize: bool,
    buffer_ms: u32,
    duration: Option<u64>,
    remote: Option<(u16, String)>,
    monitor: bool,
    monitor_device: Option<String>,
}

/// 在加载前把路径、身份、增益与热键装成条目，避免过滤坏文件后再用位置拼接。
fn playback_specs(
    cfg: &Config,
    files: &[PathBuf],
    hotkey_override: Option<&str>,
) -> Result<Vec<SoundSpec>> {
    let entries = cfg.sound_entries();
    let paths = if files.is_empty() {
        cfg.collect_sounds()?
    } else {
        files.to_vec()
    };
    let configured_files = files.is_empty() && !cfg.sound_files.is_empty();
    let mut specs: Vec<_> = paths
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, file)| {
            let occurrence = paths[..i].iter().filter(|p| **p == file).count();
            let entry = if configured_files {
                entries.get(i)
            } else {
                entries.iter().filter(|e| e.path == file).nth(occurrence)
            };
            let hotkey = entry
                .map(|e| e.hotkey.clone())
                .unwrap_or_else(|| cfg.hotkeys.get(i).cloned().unwrap_or_default());
            SoundSpec {
                id: entry
                    .map(|e| e.id.clone())
                    .unwrap_or_else(|| format!("cli-{i}-{}", file.display())),
                name: entry.map(|e| e.name.clone()).unwrap_or_default(),
                gain_db: entry.map_or(0.0, |e| e.gain_db),
                tags: entry.map(|e| e.tags.clone()).unwrap_or_default(),
                hotkey: if cfg.hotkeys.is_empty() && hotkey.is_empty() && i < 12 {
                    format!("F{}", i + 1)
                } else {
                    hotkey
                },
                file,
            }
        })
        .collect();
    if let Some(keys) = hotkey_override {
        // 空键位保留在原槽位上，可显式禁用；数量按源文件而不是加载成功数量校验。
        let keys: Vec<_> = keys.split(',').map(str::trim).collect();
        anyhow::ensure!(
            keys.len() == specs.len(),
            "热键数量（{}）与源音效数量（{}）不一致",
            keys.len(),
            specs.len()
        );
        for (spec, key) in specs.iter_mut().zip(keys) {
            spec.hotkey = key.to_owned();
        }
    }
    Ok(specs)
}

fn run(b: &CpalBackend, options: RunOptions) -> Result<()> {
    let RunOptions {
        input,
        output,
        specs,
        looping,
        mic_gain,
        sfx_gain,
        normalize,
        buffer_ms,
        duration,
        remote,
        monitor,
        monitor_device,
    } = options;
    // 1. 名称先解析为稳定标识，比较与开流都使用同一端点身份。
    let devices = b.devices()?;
    let in_id = match input {
        Some(n) => {
            resolve_device(&devices, &n, Direction::Input)?.context("指定的输入端点不存在")?
        }
        None => b.default_input().context("系统没有默认输入设备")?,
    };
    let out_id = match output {
        Some(n) => {
            resolve_device(&devices, &n, Direction::Output)?.context("指定的输出端点不存在")?
        }
        None => b
            .guess_virtual_output()
            .context("没探测到虚拟声卡，请用 --output 指定。先跑 `mixer list` 看看")?,
    };

    // 2. 协商格式（必须在开流之前，引擎要据此预分配缓冲）
    let in_fmt = b.input_format(&in_id, Some(StreamFormat::new(48_000, 1)))?;
    let out_fmt = b.output_format(&out_id, Some(StreamFormat::new(48_000, 2)))?;

    println!("┌───────────────────────────────────────────────────────────────┐");
    println!("│  真实麦克风 : {:<48}│", truncate(&in_id.to_string(), 48));
    println!("│  输出到     : {:<48}│", truncate(&out_id.to_string(), 48));
    println!(
        "│  格式       : {:<24} → {:<21}│",
        in_fmt.to_string(),
        out_fmt.to_string()
    );
    println!("│  缓冲水位   : {:<48}│", format!("{buffer_ms} ms"));
    println!("└───────────────────────────────────────────────────────────────┘");

    if in_fmt.sample_rate != out_fmt.sample_rate {
        println!(
            "  ℹ 采样率不同（{} → {}），引擎会自动重采样并做时钟漂移补偿",
            in_fmt.sample_rate, out_fmt.sample_rate
        );
    }

    // 3. GUI 和 CLI 使用同一个加载器；坏文件及其所有关联元数据一起过滤。
    let library = Library::load(&specs, out_fmt.sample_rate, looping, normalize);
    for (name, reason) in &library.skipped {
        println!("  ⏭  跳过 {name}：{reason}");
    }
    if !specs.is_empty() && library.slots.is_empty() {
        anyhow::bail!(
            "这 {} 个文件全都无法解码。第一个原因：{}",
            specs.len(),
            library
                .skipped
                .first()
                .map(|(_, r)| r.as_str())
                .unwrap_or("未知")
        );
    }
    for (i, meta) in library.meta.iter().enumerate() {
        println!(
            "  ✅ [{}] {:<26} {:.2}秒  峰值 {:>5.1}dBFS  增益 {:>5.1}dB",
            i + 1,
            truncate(&meta.spec.name, 26),
            meta.duration_secs,
            meta.peak_db,
            meta.spec.gain_db
        );
    }
    let slot_names: Vec<_> = library.meta.iter().map(|m| m.spec.name.clone()).collect();
    let keys: Vec<_> = library.meta.iter().map(|m| m.spec.hotkey.clone()).collect();
    // 监听使用其自身协商格式；持久流与主流由同一会话拥有。
    let monitor_options = if monitor && !library.slots.is_empty() {
        let device = match monitor_device {
            Some(name) => {
                resolve_device(&devices, &name, Direction::Output)?.context("监听端点不存在")?
            }
            None => b.default_output().context("系统没有监听输出设备")?,
        };
        anyhow::ensure!(
            device != out_id,
            "监听设备不能与虚拟输出相同，否则音效会被叠加两次；请设置 monitor_device"
        );
        let format = b.output_format(&device, Some(StreamFormat::stereo(out_fmt.sample_rate)))?;
        Some(MonitorOptions {
            device,
            format,
            slots: library.monitor_slots(out_fmt.sample_rate, format.sample_rate),
        })
    } else {
        None
    };

    // 4. 已成功加载条目的键位已经随元数据过滤，空键位可以单独禁用。
    // RAII 保活：`Hotkeys` 一旦 Drop 就会注销所有热键，所以它必须活到程序结束。
    // 下划线开头是**故意的** —— 这个变量就是不该被读，它的作用只是"别被释放"。
    let mut _hotkey_owner: Option<Hotkeys> = None;
    let hotkeys = if keys.iter().all(|k| k.is_empty()) {
        None
    } else {
        match Hotkeys::bind_to_slots(&keys) {
            Ok(h) => {
                println!("\n  全局热键已绑定：");
                for (spec, slot) in h.bindings() {
                    println!("    {:<18} → [{}] {}", spec, slot + 1, slot_names[*slot]);
                }
                // 轮询线程只拿事件句柄，原生管理器保持平台要求的所有权。
                let poller = Arc::new(h.poller());
                // ⚠️ 管理器必须**一直活着** —— Drop 会注销所有热键。
                //    存进外层变量，别让它在这个 match 分支结束时就掉了。
                _hotkey_owner = Some(h);
                Some(poller)
            }
            Err(e) => {
                println!("  ⚠ 热键绑定失败：{e}");
                println!("     （该键位可能已被系统或其他程序占用；终端数字键仍可触发）");
                None
            }
        }
    };

    // 5. 共用会话负责设备线程、格式契约、监听和统一播放控制。
    let dump = std::env::var_os("MIXER_DUMP").map(|path| DumpOptions {
        path: PathBuf::from(path),
        max_frames: std::env::var("MIXER_DUMP_FRAMES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(out_fmt.sample_rate as usize * 6),
    });
    let session = Session::open_with_dump(
        SessionOptions {
            input: in_id,
            output: out_id,
            input_format: in_fmt,
            output_format: out_fmt,
            buffer_ms,
            slots: library.slots,
            mic_gain_db: mic_gain,
            sfx_gain_db: sfx_gain,
            monitor: monitor_options,
        },
        dump,
    )?;
    let control = session.playback.clone();
    let stats = session.stats.clone();
    let monitor_health = session.monitor_health.clone();
    let mut remote_shared = None;

    // 手机遥控（可选）
    if let Some((port, bind)) = remote {
        let shared = Arc::new(remote_ctl::Shared::default());
        configure_remote(&shared, &control, slot_names.clone());
        let self_exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "mixer".into());
        let bind_all = bind == "0.0.0.0";
        match remote_ctl::serve(shared.clone(), remote_ctl::RemoteConfig { port, bind }) {
            Ok(urls) => {
                remote_shared = Some(shared);
                println!("\n  📱 手机遥控已开启（热键被游戏吞掉时的兜底）：");
                for u in &urls {
                    println!("     {u}");
                }
                if bind_all {
                    if let Some(h) = remote_ctl::firewall_hint(&self_exe) {
                        println!("\n  ⚠ {h}");
                    }
                }
            }
            Err(e) => println!("  ⚠ 手机遥控启动失败：{e}"),
        }
    }

    println!("\n✅ 音频链路已启动");
    if slot_names.is_empty() {
        println!("   对着麦克风说话，对方应该能听到。q = 退出\n");
    } else {
        println!(
            "   终端也能触发：数字键 1-{} = 播放   s = 全部停止   q = 退出\n",
            slot_names.len()
        );
    }

    // 7. 统计线程
    //    启动瞬态（缓冲还没填满）的欠载不计入健康度 —— 从第 2 秒起单独累计"稳态"计数。
    let running = Arc::new(AtomicBool::new(true));
    let steady = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
    let running_t = running.clone();
    let stats_t = stats.clone();
    let steady_t = steady.clone();
    let names_t = slot_names.clone();
    let monitor_health_t = monitor_health.clone();
    let t_start = Instant::now();
    let stats_handle = std::thread::spawn(move || {
        let mut last = (0u64, 0u64, 0u64);
        let mut warmup = true;
        let mut monitor_warned = false;
        while running_t.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(1000));
            if !running_t.load(Ordering::Relaxed) {
                break;
            }
            if let Some(warning) = take_monitor_warning(&monitor_health_t, &mut monitor_warned) {
                println!("  {warning}");
            }
            let s = stats_t.snapshot();
            let d_cap = s.captured_frames.saturating_sub(last.0);
            let d_run = s.underruns.saturating_sub(last.1);
            let d_ovr = s.overruns.saturating_sub(last.2);
            last = (s.captured_frames, s.underruns, s.overruns);
            if warmup {
                warmup = false;
                continue;
            }
            steady_t.0.fetch_add(d_run, Ordering::Relaxed);
            steady_t.1.fetch_add(d_ovr, Ordering::Relaxed);

            let fill_ms = frames_to_secs(s.ring_fill as u64 / 2, out_fmt.sample_rate) * 1000.0;
            let play = if names_t.is_empty() {
                String::new()
            } else if s.playing && s.active_slot >= 0 {
                let n = names_t
                    .get(s.active_slot as usize)
                    .map(|s| s.as_str())
                    .unwrap_or("?");
                format!(
                    "▶ {n} {:.1}s",
                    frames_to_secs(s.playhead, out_fmt.sample_rate)
                )
            } else {
                "■ 待机".to_string()
            };
            println!(
                "  [{:>4}s] 采集 {d_cap:>5} 帧/s  水位 {fill_ms:>5.1}ms  欠载 {d_run:<3} 溢出 {d_ovr:<3} 限幅 {:>5.1}dB  {play}",
                t_start.elapsed().as_secs(),
                s.limiter_gain_db
            );
        }
        monitor_warned
    });

    // 8. 事件循环（同时轮询热键与终端输入）
    //
    // 有热键时：轮询放工作线程，**主线程让出来跑 macOS 应用事件循环** ——
    // Carbon 热键事件只有在完整应用事件循环里才会被派发（真机实测：
    // 裸 CFRunLoop 收不到，GUI 因为有 NSApplication 所以一直正常）。
    // 没有热键时走原路径，脚本（--remote / --duration）行为完全不变。
    let quit = match hotkeys {
        Some(hk) => {
            APP_LOOP_USED.store(true, Ordering::Relaxed);
            // 必须在起工作线程**之前**清除退出标志，否则可能被提前置位而卡死
            hotkeys::reset_app_event_loop();
            let ctl = control;
            let has = !slot_names.is_empty();
            let run2 = running.clone();
            let events = std::thread::Builder::new()
                .name("cmd-loop".into())
                .spawn(move || event_loop(ctl, Some(hk), has, duration, run2))?;
            hotkeys::run_app_event_loop();
            APP_LOOP_USED.store(false, Ordering::Relaxed);
            events.join().unwrap_or(false)
        }
        None => event_loop(
            control,
            None,
            !slot_names.is_empty(),
            duration,
            running.clone(),
        ),
    };

    running.store(false, Ordering::Relaxed);
    if let Some(shared) = remote_shared {
        shared.clear();
    }
    drop(session);
    let mut monitor_warned = stats_handle.join().unwrap_or(false);
    if let Some(warning) = take_monitor_warning(&monitor_health, &mut monitor_warned) {
        println!("  {warning}");
    }

    let s = stats.snapshot();
    let elapsed = t_start.elapsed().as_secs_f32();
    let st_run = steady.0.load(Ordering::Relaxed);
    let st_ovr = steady.1.load(Ordering::Relaxed);
    let startup_run = s.underruns.saturating_sub(st_run);
    let startup_ovr = s.overruns.saturating_sub(st_ovr);

    println!("\n── 运行报告 ──");
    println!("  时长            : {elapsed:.1}s");
    println!(
        "  采集 / 渲染帧数 : {} / {}",
        s.captured_frames, s.rendered_frames
    );
    println!(
        "  稳态欠载 / 溢出 : {st_run} / {st_ovr}   {}",
        if st_run == 0 && st_ovr == 0 {
            "✅ 缓冲健康"
        } else {
            "⚠ 建议增大 --buffer-ms"
        }
    );
    println!("  启动瞬态欠载    : {startup_run}（缓冲填满前必然发生，可忽略）");
    if startup_ovr > 0 {
        println!("  启动瞬态溢出    : {startup_ovr}（应接近 0；持续不为 0 说明缓冲偏小）");
    }
    if s.captured_frames > 0 {
        let ratio = s.rendered_frames as f64 / s.captured_frames as f64;
        let expected = out_fmt.sample_rate as f64 / in_fmt.sample_rate as f64;
        println!("  渲染/采集比     : {ratio:.4}   （按实际采样率稳态应≈{expected:.4}；含启动瞬态时略偏大）");
    }
    if quit {
        println!("\n已退出。");
    }
    Ok(())
}

fn configure_remote(shared: &remote_ctl::Shared, playback: &Arc<Playback>, names: Vec<String>) {
    let play = playback.clone();
    let stop = playback.clone();
    shared.set_routed(
        playback.control.clone(),
        names,
        Arc::new(move |slot| play.play(slot)),
        Arc::new(move || stop.stop_all()),
    );
}

fn frames_to_secs(frames: u64, sample_rate: u32) -> f32 {
    frames as f32 / sample_rate.max(1) as f32
}

/// 事件循环：同时处理**全局热键**与终端输入。
fn event_loop(
    control: Arc<Playback>,
    hotkeys: Option<Arc<hotkeys::HotkeyPoller>>,
    has_slots: bool,
    duration: Option<u64>,
    running: Arc<AtomicBool>,
) -> bool {
    let r = event_loop_inner(control, hotkeys, has_slots, duration, running);

    // 让主线程的应用事件循环返回（纯 AtomicBool，无竞态）
    if app_loop_used() {
        hotkeys::quit_app_event_loop();
    }
    r
}

/// 主线程是否在跑应用事件循环（由 `run` 设置）。
static APP_LOOP_USED: AtomicBool = AtomicBool::new(false);
fn app_loop_used() -> bool {
    APP_LOOP_USED.load(Ordering::Relaxed)
}

fn event_loop_inner(
    control: Arc<Playback>,
    hotkeys: Option<Arc<hotkeys::HotkeyPoller>>,
    has_slots: bool,
    duration: Option<u64>,
    running: Arc<AtomicBool>,
) -> bool {
    let t0 = Instant::now();

    // 定时模式：不碰 stdin（脚本环境下 stdin 会立即 EOF，读它会把"没有输入"误判成"退出"），
    // 但仍轮询热键，方便验证绑定是否生效。
    if let Some(d) = duration {
        while t0.elapsed().as_secs() < d {
            if !running.load(Ordering::Relaxed) {
                return false;
            }
            if let Some(slot) = hotkeys.as_ref().and_then(|h| h.poll()) {
                control.play(slot);
                println!("  ▶ 热键触发槽位 {}", slot + 1);
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        return true;
    }

    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(l) = line else { break };
            if tx.send(l).is_err() {
                break;
            }
        }
    });

    let mut stdin_alive = true;
    loop {
        // 热键轮询间隔 15ms —— 人在游戏里感知不到，又不至于空转烧 CPU
        if let Some(slot) = hotkeys.as_ref().and_then(|h| h.poll()) {
            control.play(slot);
            println!("  ▶ 热键触发槽位 {}", slot + 1);
        }
        if !running.load(Ordering::Relaxed) {
            return false;
        }
        if !stdin_alive {
            std::thread::sleep(Duration::from_millis(15));
            continue;
        }

        match rx.recv_timeout(Duration::from_millis(15)) {
            Ok(line) => {
                let cmd = line.trim().to_lowercase();
                match cmd.as_str() {
                    "q" | "quit" | "exit" => return true,
                    "s" | "stop" => {
                        control.stop_all();
                        println!("  ⏹ 已停止全部音效");
                    }
                    "" => {
                        if has_slots {
                            control.play(0);
                            println!("  ▶ 触发槽位 1");
                        }
                    }
                    other => {
                        if let Ok(n) = other.parse::<usize>() {
                            if n >= 1 && control.play(n - 1) {
                                println!("  ▶ 触发槽位 {n}");
                            } else {
                                println!("  没有第 {n} 个音效");
                            }
                        } else if let Ok(db) = other.parse::<f32>() {
                            control.set_gains(control.control.mic_gain_db.get(), db);
                            println!("  🔊 音效增益 → {db} dB");
                        } else {
                            println!("  未知命令「{other}」。数字=播放  s=停止  q=退出");
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            // stdin 已关闭（后台运行）—— 不再当作退出信号，持续运行到 Ctrl-C
            Err(RecvTimeoutError::Disconnected) => stdin_alive = false,
        }
    }
}

/// 扫描文件夹里的音频文件（转发到共享实现，顺带打印一行提示）。
fn collect_audio_files(dir: &std::path::Path) -> Result<Vec<PathBuf>> {
    let out = app_config::scan_dir(dir)?;
    if out.is_empty() {
        println!("  ⚠ 文件夹里没有找到音频文件: {}", dir.display());
    } else {
        println!("  在 {} 里找到 {} 个候选文件", dir.display(), out.len());
    }
    Ok(out)
}

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        format!(
            "{}…",
            chars[..max.saturating_sub(1)].iter().collect::<String>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_core::engine::{EngineParts, SfxSlot};

    #[test]
    fn 监听流错误只提示一次且不改变主流健康状态() {
        let monitor_health = StreamHealth::default();
        let primary_health = StreamHealth::default();
        let mut warned = false;
        assert!(take_monitor_warning(&monitor_health, &mut warned).is_none());
        monitor_health.report_error("输出", "监听设备已断开");
        let warning = take_monitor_warning(&monitor_health, &mut warned).unwrap();
        assert!(warning.contains("监听设备已断开"));
        assert!(warning.contains("主链路继续运行"));
        monitor_health.report_error("输出", "仍未恢复");
        monitor_health.mark_panicked();
        assert!(take_monitor_warning(&monitor_health, &mut warned).is_none());
        assert!(monitor_health.take_error().is_none());
        assert!(!primary_health.panicked());
        assert!(primary_health.take_error().is_none());
    }

    #[test]
    fn 监听流只有_panic_标志时也必须单次提示() {
        let health = StreamHealth::default();
        health.mark_panicked();
        let mut warned = false;
        assert!(take_monitor_warning(&health, &mut warned)
            .unwrap()
            .contains("panic"));
        assert!(take_monitor_warning(&health, &mut warned).is_none());
    }

    fn configured_sounds() -> Config {
        Config {
            sound_files: vec!["bad.wav".into(), "good.wav".into()],
            sound_ids: vec!["bad-id".into(), "good-id".into()],
            sound_names: vec!["坏音效".into(), "有效音效".into()],
            sound_gains: vec![12.0, -20.0],
            sound_tags: vec![String::new(), "B-only".into()],
            hotkeys: vec!["F1".into(), "F2".into()],
            ..Config::default()
        }
    }

    #[test]
    fn 配置播放必须保留条目的身份增益名称标签和热键() {
        let specs = playback_specs(&configured_sounds(), &[], None).unwrap();
        assert_eq!(specs[1].id, "good-id");
        assert_eq!(specs[1].name, "有效音效");
        assert_eq!(specs[1].gain_db, -20.0);
        assert_eq!(specs[1].tags, ["B-only"]);
        assert_eq!(specs[1].hotkey, "F2");
        let selected = playback_specs(&configured_sounds(), &["good.wav".into()], None).unwrap();
        assert_eq!(selected[0].id, "good-id");
        assert_eq!(
            selected[0].hotkey, "F2",
            "显式选取文件必须按文件身份关联配置"
        );
    }

    #[test]
    fn 空热键应保持原槽位且覆盖数量按源文件校验() {
        let specs = playback_specs(&configured_sounds(), &[], Some(",F8")).unwrap();
        assert!(specs[0].hotkey.is_empty());
        assert_eq!(specs[1].hotkey, "F8");
        assert!(playback_specs(&configured_sounds(), &[], Some("F8")).is_err());
        let mut cfg = configured_sounds();
        cfg.hotkeys = vec![String::new(), "F2".into()];
        assert!(playback_specs(&cfg, &[], None).unwrap()[0]
            .hotkey
            .is_empty());
    }

    #[test]
    fn 坏文件过滤必须同时保留正确条目和键位() {
        let dir = std::env::temp_dir().join(format!("mixer-cli-load-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.wav");
        let frames = 80u32;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + frames * 2).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&8000u32.to_le_bytes());
        wav.extend_from_slice(&16000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(frames * 2).to_le_bytes());
        for _ in 0..frames {
            wav.extend_from_slice(&1000i16.to_le_bytes());
        }
        std::fs::write(&good, wav).unwrap();
        let mut cfg = configured_sounds();
        cfg.sound_files = vec![dir.join("missing.wav"), good];
        let specs = playback_specs(&cfg, &[], Some("F7,F8")).unwrap();
        let library = Library::load(&specs, 48_000, false, false);
        let _ = std::fs::remove_dir_all(dir);
        assert_eq!(library.slots.len(), 1);
        assert_eq!(library.meta[0].source_index, 1);
        assert_eq!(library.meta[0].spec.id, "good-id");
        assert_eq!(library.meta[0].spec.hotkey, "F8");
        assert_eq!(library.meta[0].spec.gain_db, -20.0);
        assert_eq!(library.slots[0].name, "有效音效");
        assert_eq!(library.slots[0].gain_db, -20.0);
    }

    #[test]
    fn 手机遥控必须走统一播放门控并触发和停止监听() {
        let f = StreamFormat::stereo(44_100);
        let slot = SfxSlot {
            name: "test".into(),
            samples: Arc::new(vec![0.4; 8000]),
            looping: true,
            gain_db: 0.0,
        };
        let mut primary = EngineParts::build(f, f, vec![slot.clone()], 20);
        let mut monitor = EngineParts::build(f, f, vec![slot], 20);
        let playback = Arc::new(Playback::new(
            primary.control.clone(),
            Some(monitor.control.clone()),
        ));
        let shared = remote_ctl::Shared::default();
        configure_remote(&shared, &playback, vec!["test".into()]);
        playback.standby();
        assert!(!shared.play(0));
        playback.activate();
        assert!(shared.play(0));
        let mut output = vec![0.0; 2000];
        primary.render.process(&mut output);
        monitor.render.process(&mut output);
        assert!(primary.control.is_playing() && monitor.control.is_playing());
        assert_eq!(playback.monitor_plays.load(Ordering::Relaxed), 1);
        shared.stop_all();
        primary.render.process(&mut output);
        monitor.render.process(&mut output);
        assert!(!primary.control.is_playing() && !monitor.control.is_playing());
    }

    #[test]
    fn 统计时间必须使用实际采样率() {
        assert_eq!(frames_to_secs(44_100, 44_100), 1.0);
        assert!((frames_to_secs(882, 44_100) * 1000.0 - 20.0).abs() < 1e-5);
    }

    #[test]
    fn 自检支持显式录音端点参数() {
        let cli = Cli::try_parse_from([
            "mixer",
            "selftest",
            "--device",
            "CABLE Input",
            "--capture",
            "CABLE Output",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Selftest {
                device, capture, ..
            } => {
                assert_eq!(device.as_deref(), Some("CABLE Input"));
                assert_eq!(capture.as_deref(), Some("CABLE Output"));
            }
            _ => panic!("应解析为 selftest"),
        }
    }
}
