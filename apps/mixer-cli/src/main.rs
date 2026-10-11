//! `mixer` —— 命令行版：把音频播进麦克风。
//!
//! 这是产品的**无头内核**，UI 之后套在上面。
//!
//! ```text
//! mixer list                                                 # 列出设备
//! mixer passthrough                                          # 麦克风透传（验证链路）
//! mixer play --file 笑.mp3 --file 警报.mp3 --hotkeys F1,F2     # 全局热键触发的音效板
//! ```


use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use audio_core::decode::decode_file;
use audio_core::device::{AudioBackend, DeviceId, Direction, StreamFormat};
use audio_core::engine::{EngineParts, PlaybackControl, SfxSlot};
use backend_cpal::CpalBackend;
use hotkeys::Hotkeys;

use app_config::Config;
use remote as remote_ctl;

#[derive(Parser, Debug)]
#[command(
    name = "mixer",
    version,
    about = "把本地音频播进麦克风，让队友听到",
    long_about = "把本地音频播进麦克风，让队友听到。\n\n日常使用只需要：mixer play（读 ~/.config/mixer/config.toml）"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 列出所有音频设备（含虚拟声卡）
    List,

    /// 端到端自检：送已知信号进虚拟设备，从环回录回来验证整条链路
    Selftest {
        /// 虚拟设备名，省略则自动探测
        #[arg(short, long)]
        device: Option<String>,
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

    /// 音效板：麦克风 + 多路音效实时混音，全局热键触发（**不传参数就读配置**）
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,symphonia=off")).init();
    let cli = Cli::parse();
    let backend = CpalBackend::new();

    match cli.cmd {
        Cmd::List => list_devices(&backend),
        Cmd::Selftest { device, freq, secs } => cmd_selftest(&backend, device, freq, secs),
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
                input.or(cfg.input_device),
                output.or(cfg.output_device),
                Vec::new(),
                None,
                false,
                0.0,
                0.0,
                false,
                buffer_ms,
                duration,
                None,
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

            // 音效来源优先级：--file/--dir > 配置里的 sound_dirs
            let mut files = file;
            if let Some(d) = dir {
                files.extend(collect_audio_files(&d)?);
            }
            if files.is_empty() {
                files = cfg.collect_sounds()?;
            }
            if files.is_empty() {
                anyhow::bail!(
                    "没有音效。三种做法任选：\n  \
                     · mixer play --file 笑.mp3\n  \
                     · mixer play --dir ~/Sounds\n  \
                     · mixer config set sound_dirs ~/Sounds   （之后直接 `mixer play`）"
                );
            }

            let hotkeys = hotkeys.or_else(|| {
                if cfg.hotkeys.is_empty() {
                    None
                } else {
                    Some(cfg.hotkeys.join(","))
                }
            });

            run(
                &backend,
                input.or(cfg.input_device),
                output.or(cfg.output_device),
                files,
                hotkeys,
                r#loop,
                mic_gain.or(cfg.mic_gain_db).unwrap_or(0.0),
                sfx_gain.or(cfg.sfx_gain_db).unwrap_or(-6.0),
                normalize,
                buffer_ms.or(cfg.buffer_ms).unwrap_or(20),
                duration,
                remote.map(|port| (port, remote_bind)),
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
        Some(s) => s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        None if !cfg.hotkeys.is_empty() => cfg.hotkeys.clone(),
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
    println!("  ✓ 主线程检查通过（macOS 要求热键管理器在主线程创建）");

    let hk = Hotkeys::bind_to_slots(&specs).context("热键绑定失败")?;
    for (spec, slot) in hk.bindings() {
        println!("  {:<18} → 槽位 {}", spec, slot + 1);
    }
    println!("\n  现在按一下上面任意一个键。本程序会打印收到的时间。");
    println!();
    println!("  ⚠️ MacBook 键盘注意：F1~F12 默认是媒体键（亮度/音量），");
    println!("     必须**按住 fn** 才是真正的 F1。若 fn+F1 仍无反应，可以：");
    println!("       · 系统设置 → 键盘 → 键盘快捷键 → 功能键 → 把 F1/F2 用作标准功能键");
    println!("       · 或者改用组合键，例如 --keys \"Cmd+Shift+1,Cmd+Shift+2\"");
    println!("  先在这里试；再到游戏里（全屏）试 —— 有些游戏会吞掉全局热键。");

    // 轮询放**工作线程** —— 主线程必须腾出来跑应用事件循环。
    //
    // 实测结论：只跑裸 CFRunLoop（CFRunLoopRunInMode）**收不到** Carbon 热键事件，
    // 必须是完整的应用事件循环。GUI 之所以一直正常，就是因为 tao 在跑 NSApplication。
    let names = specs.clone();
    std::thread::Builder::new()
        .name("hotkey-poll".into())
        .spawn(move || {
            let t0 = Instant::now();
            let mut count = 0usize;
            loop {
                if let Some(slot) = hk.poll() {
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

    println!("  主线程正在运行 macOS 应用事件循环（热键能收到的前提）。");
    println!("  Ctrl-C 退出。\n");
    hotkeys::run_app_event_loop();
    Ok(())
}

// ─────────────────────────────────────────── list

/// 端到端自检：跑一遍链路，把结构化结果排版成表格。
fn cmd_selftest(
    backend: &CpalBackend,
    device: Option<String>,
    freq: f32,
    secs: f32,
) -> Result<()> {
    // 自动重试：这块虚拟声卡偶发抽风，重开一次流通常就好
    let r = selftest::run_with_retry(backend, device, freq, secs, 3)?;

    println!("\n═══ 端到端自检 ═══");
    println!("  虚拟设备 : {}", r.device);
    println!(
        "  送入信号 : {} Hz 正弦，{} 秒（@{}Hz）",
        r.freq_sent, r.secs_sent, r.sample_rate
    );
    println!("  录音来源 : 同一设备的环回输入侧\n");
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
        r.captured_samples as f32 / 2.0 / r.sample_rate as f32,
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
        }
        println!();
    }
    println!("提示：虚拟声卡在输入侧和输出侧各是一个设备，名字通常相同。\n");
    Ok(())
}

// ─────────────────────────────────────────── 主流程

#[allow(clippy::too_many_arguments)]
fn run(
    b: &CpalBackend,
    input: Option<String>,
    output: Option<String>,
    files: Vec<PathBuf>,
    hotkey_spec: Option<String>,
    looping: bool,
    mic_gain: f32,
    sfx_gain: f32,
    normalize: bool,
    buffer_ms: u32,
    duration: Option<u64>,
    remote: Option<(u16, String)>,
) -> Result<()> {
    // 1. 解析设备
    let in_id = match input {
        Some(n) => DeviceId::new(n),
        None => b.default_input().context("系统没有默认输入设备")?,
    };
    let out_id = match output {
        Some(n) => DeviceId::new(n),
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

    // 3. 解码所有音效（非实时线程，可以慢）
    let mut slots: Vec<SfxSlot> = Vec::new();
    // ⚠️ 单个坏文件不能拖垮整批：网上下的素材里混着空文件/截断文件/其实是网页的 .mp3
    //    太常见了。早期版本遇到第一个就 `?` 返回，整个音效板起不来。
    let mut skipped: Vec<(String, String)> = Vec::new();
    for p in &files {
        let t0 = Instant::now();
        let mut audio = match decode_file(p, out_fmt.sample_rate) {
            Ok(a) => a,
            Err(e) => {
                let name = p
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let reason = e
                    .chain()
                    .last()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| e.to_string());
                println!("  ⏭  跳过 {name}：{reason}");
                skipped.push((name, reason));
                continue;
            }
        };
        if audio.frames() == 0 {
            let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            println!("  ⏭  跳过 {name}：文件里没有音频数据");
            skipped.push((name, "文件里没有音频数据".into()));
            continue;
        }
        if normalize {
            audio.normalize_peak(0.708); // -3 dBFS
        }
        let name = p
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "未命名".into());
        println!(
            "  ✅ [{}] {:<26} {:.2}秒  峰值 {:>5.1}dBFS  解码 {}ms",
            slots.len() + 1,
            truncate(&name, 26),
            audio.duration_secs,
            20.0 * audio.peak().max(1e-9).log10(),
            t0.elapsed().as_millis()
        );
        slots.push(SfxSlot::from_decoded(name, &audio, looping));
    }
    if slots.is_empty() {
        anyhow::bail!(
            "这 {} 个文件全都无法解码。第一个原因：{}",
            files.len(),
            skipped.first().map(|(_, r)| r.as_str()).unwrap_or("未知")
        );
    }
    if !skipped.is_empty() {
        println!(
            "\n  ⚠ {} 个文件被跳过（共 {} 个可用）—— 不影响其余音效使用",
            skipped.len(),
            slots.len()
        );
    }
    let slot_names: Vec<String> = slots.iter().map(|s| s.name.clone()).collect();

    // 4. 绑定全局热键
    let specs: Vec<String> = match &hotkey_spec {
        Some(s) => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        None if !slots.is_empty() => (1..=slots.len()).map(|i| format!("F{i}")).collect(),
        None => Vec::new(),
    };
    let hotkeys = if specs.is_empty() {
        None
    } else if specs.len() != slots.len() {
        println!(
            "  ⚠ 热键数量（{}）与音效数量（{}）不一致，跳过热键绑定",
            specs.len(),
            slots.len()
        );
        None
    } else {
        match Hotkeys::bind_to_slots(&specs) {
            Ok(h) => {
                println!("\n  全局热键已绑定：");
                for (spec, slot) in h.bindings() {
                    println!("    {:<18} → [{}] {}", spec, slot + 1, slot_names[*slot]);
                }
                Some(Arc::new(h))
            }
            Err(e) => {
                println!("  ⚠ 热键绑定失败：{e}");
                println!("     （该键位可能已被系统或其他程序占用；终端数字键仍可触发）");
                None
            }
        }
    };

    // 5. 组装引擎
    let parts = EngineParts::build(in_fmt, out_fmt, slots, buffer_ms);
    let control = parts.control.clone();
    let stats = parts.stats.clone();
    control.mic_gain_db.set(mic_gain);
    control.sfx_gain_db.set(sfx_gain);

    let mut capture = parts.capture;
    let mut render = parts.render;

    // 6. 开流 —— **先输出后输入**。
    //    顺序很关键：输出侧先跑起来开始消费，输入侧才开始生产；
    //    反过来的话启动那一瞬输入会把环形缓冲灌满，导致持续溢出。
    // 诊断：MIXER_DUMP=/tmp/x.f32 可把交给声卡的原始数据存下来
    let dump_path = std::env::var("MIXER_DUMP").ok();
    let dump_limit: usize = std::env::var("MIXER_DUMP_FRAMES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(48_000 * 6);
    let mut dump_w = dump_path
        .as_ref()
        .and_then(|p| std::fs::File::create(p).ok())
        .map(std::io::BufWriter::new);
    let mut dumped = 0usize;

    let out_stream = b.open_output(
        &out_id,
        Some(out_fmt),
        Box::new(move |data: &mut [f32]| {
            render.process(data);
            if let Some(w) = dump_w.as_mut() {
                if dumped < dump_limit {
                    use std::io::Write;
                    for s in data.iter() {
                        let _ = w.write_all(&s.to_le_bytes());
                    }
                    dumped += data.len();
                }
            }
        }),
    )?;
    let in_stream = b.open_input(
        &in_id,
        Some(in_fmt),
        Box::new(move |data: &[f32]| capture.process(data)),
    )?;

    if in_stream.format() != in_fmt {
        println!("  ⚠ 实际输入格式 {} 与预期 {} 不一致", in_stream.format(), in_fmt);
    }
    if out_stream.format() != out_fmt {
        println!("  ⚠ 实际输出格式 {} 与预期 {} 不一致", out_stream.format(), out_fmt);
    }

    // 手机遥控（可选）
    if let Some((port, bind)) = remote {
        let shared = Arc::new(remote_ctl::Shared::default());
        shared.set(control.clone(), slot_names.clone());
        let self_exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "mixer".into());
        let bind_all = bind == "0.0.0.0";
        match remote_ctl::serve(shared, remote_ctl::RemoteConfig { port, bind }) {
            Ok(urls) => {
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
        println!("   对着麦克风说话，队友应该能听到。q = 退出\n");
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
    let t_start = Instant::now();
    let stats_handle = std::thread::spawn(move || {
        let mut last = (0u64, 0u64, 0u64);
        let mut warmup = true;
        while running_t.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(1000));
            if !running_t.load(Ordering::Relaxed) {
                break;
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

            let fill_ms = s.ring_fill as f32 / 2.0 / 48.0;
            let play = if names_t.is_empty() {
                String::new()
            } else if s.playing && s.active_slot >= 0 {
                let n = names_t
                    .get(s.active_slot as usize)
                    .map(|s| s.as_str())
                    .unwrap_or("?");
                format!("▶ {n} {:.1}s", s.playhead as f32 / 48_000.0)
            } else {
                "■ 待机".to_string()
            };
            println!(
                "  [{:>4}s] 采集 {d_cap:>5} 帧/s  水位 {fill_ms:>5.1}ms  欠载 {d_run:<3} 溢出 {d_ovr:<3} 限幅 {:>5.1}dB  {play}",
                t_start.elapsed().as_secs(),
                s.limiter_gain_db
            );
        }
    });

    // 8. 事件循环（同时轮询热键与终端输入）
    //
    // 有热键时：轮询放工作线程，**主线程让出来跑 macOS 应用事件循环** ——
    // Carbon 热键事件只有在完整应用事件循环里才会被派发（真机实测：
    // 裸 CFRunLoop 收不到，GUI 因为有 NSApplication 所以一直正常）。
    // 没有热键时走原路径，脚本（--remote / --duration）行为完全不变。
    let quit = match hotkeys.clone() {
        Some(hk) => {
            APP_LOOP_USED.store(true, Ordering::Relaxed);
            // 必须在起工作线程**之前**清除退出标志，否则可能被提前置位而卡死
            hotkeys::reset_app_event_loop();
            let ctl = control.clone();
            let has = !slot_names.is_empty();
            let run2 = running.clone();
            std::thread::Builder::new()
                .name("cmd-loop".into())
                .spawn(move || event_loop(ctl, Some(hk), has, duration, run2))?;
            hotkeys::run_app_event_loop();
            APP_LOOP_USED.store(false, Ordering::Relaxed);
            // 工作线程退出后会自己结束；这里只需知道用户是不是主动退出的
            running.load(Ordering::Relaxed) || duration.is_some()
        }
        None => event_loop(
            control.clone(),
            None,
            !slot_names.is_empty(),
            duration,
            running.clone(),
        ),
    };

    running.store(false, Ordering::Relaxed);
    drop(in_stream);
    drop(out_stream);
    let _ = stats_handle.join();

    let s = stats.snapshot();
    let elapsed = t_start.elapsed().as_secs_f32();
    let st_run = steady.0.load(Ordering::Relaxed);
    let st_ovr = steady.1.load(Ordering::Relaxed);
    let startup_run = s.underruns.saturating_sub(st_run);
    let startup_ovr = s.overruns.saturating_sub(st_ovr);

    println!("\n── 运行报告 ──");
    println!("  时长            : {elapsed:.1}s");
    println!("  采集 / 渲染帧数 : {} / {}", s.captured_frames, s.rendered_frames);
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
        println!("  渲染/采集比     : {ratio:.4}   （稳态下应≈1.0；含启动瞬态时略偏大是正常的）");
    }
    if quit {
        println!("\n已退出。");
    }
    Ok(())
}

/// 事件循环：同时处理**全局热键**与终端输入。
fn event_loop(
    control: Arc<PlaybackControl>,
    hotkeys: Option<Arc<Hotkeys>>,
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
    control: Arc<PlaybackControl>,
    hotkeys: Option<Arc<Hotkeys>>,
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
                            control.sfx_gain_db.set(db);
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
        format!("{}…", chars[..max.saturating_sub(1)].iter().collect::<String>())
    }
}
