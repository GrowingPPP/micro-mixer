//! 音频宿主 —— GUI 与音频引擎之间的一层。
//!
//! ## 为什么要单独一层
//!
//! cpal 的 `Stream` 在 macOS 上**不是 `Send`**（内部含平台标记），
//! 所以流必须留在创建它的线程上。这里用一个**专用音频线程**持有流，
//! GUI 线程只通过 `PlaybackControl`（无锁原子量 + SPSC 命令队列）操控它。
//!
//! 好处：GUI 卡顿、重绘、甚至 WebView 崩溃都不会影响音频实时线程。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use audio_core::decode::decode_file;
use audio_core::device::{AudioBackend, DeviceId, StreamFormat};
use audio_core::engine::{EngineParts, EngineStats, PlaybackControl, SfxSlot};
use backend_cpal::CpalBackend;
use hotkeys::Hotkeys;

// ─────────────────────────────────────────── DTO

#[derive(Debug, Clone, Serialize)]
pub struct DeviceDto {
    pub name: String,
    pub direction: String,
    pub is_default: bool,
    pub channels: u16,
    pub sample_rate: u32,
    /// 看起来是不是虚拟声卡（用于"没装虚拟声卡"的引导）
    pub is_virtual: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SlotDto {
    pub index: usize,
    pub name: String,
    pub file: String,
    pub duration: f32,
    pub peak_db: f32,
    pub hotkey: String,
    /// 该音效自己的音量（dB）
    pub gain_db: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartConfig {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub files: Vec<String>,
    /// 每个音效的音量（dB），与 files 一一对应
    #[serde(default)]
    pub gains: Vec<f32>,
    /// 每个音效的显示名（空串 = 用文件名）
    #[serde(default)]
    pub names: Vec<String>,
    pub hotkeys: Vec<String>,
    pub buffer_ms: u32,
    pub mic_gain_db: f32,
    pub sfx_gain_db: f32,
    pub looping: bool,
    /// 监听：触发的音效**同时放给自己听**（走系统默认输出）。
    /// 没有它，用户在游戏里按键后什么都听不到，只能盲猜队友有没有听到。
    #[serde(default)]
    pub monitor: bool,
    /// 监听输出设备；None = 系统默认输出
    #[serde(default)]
    pub monitor_device: Option<String>,
    /// 手机遥控端口；None 表示不开启
    #[serde(default)]
    pub remote_port: Option<u16>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusDto {
    pub running: bool,
    pub device: String,
    pub input_device: String,
    pub slots: Vec<SlotDto>,
    pub playing: bool,
    pub active_slot: i32,
    pub playhead_secs: f32,
    pub ring_ms: f32,
    pub underruns: u64,
    pub overruns: u64,
    pub limiter_db: f32,
    /// 送往虚拟麦克风的峰值电平（dBFS）
    pub out_peak_db: f32,
    /// 送往虚拟麦克风的短时 RMS（dBFS）
    pub out_rms_db: f32,
    pub hotkeys_active: bool,
    pub last_error: Option<String>,
    pub uptime_secs: f32,
    pub remote_urls: Vec<String>,
    /// 本次运行中监听实际触发的次数
    pub monitor_plays: u64,
    /// 加载时被跳过的坏文件（名字 + 原因）
    pub skipped: Vec<String>,
    /// 设备掉线提示。有值 = 检测到设备不见了，界面要显著报警。
    pub device_lost: Option<String>,
    /// 最近一次 panic 的摘要（界面据此弹提示；读一次就清）。
    pub last_panic: Option<String>,
    /// 音频卡死提示（回调不再被调用 / 回调 panic 过）。有值 = 界面要报警。
    pub audio_stalled: Option<String>,
    /// 引擎是否还活着（麦克风是否正在被转发）。
    ///
    /// `running=false, passthrough=true` = **待机**：队友仍听得到你说话，但音效不响应。
    pub passthrough: bool,
}

impl StatusDto {
    pub fn stopped() -> Self {
        Self {
            running: false,
            device: String::new(),
            input_device: String::new(),
            slots: Vec::new(),
            playing: false,
            active_slot: -1,
            playhead_secs: 0.0,
            ring_ms: 0.0,
            underruns: 0,
            overruns: 0,
            limiter_db: 0.0,
            out_peak_db: -100.0,
            out_rms_db: -100.0,
            hotkeys_active: false,
            last_error: None,
            uptime_secs: 0.0,
            remote_urls: Vec::new(),
            monitor_plays: 0,
            skipped: Vec::new(),
            passthrough: false,
            device_lost: None,
            last_panic: None,
            audio_stalled: None,
        }
    }
}

// ─────────────────────────────────────────── 音效加载

const AUDIO_EXT: &[&str] = &[
    "mp3", "wav", "ogg", "m4a", "flac", "aac", "aiff", "caf", "opus",
];

/// 扫描目录里的音频文件（按名排序）。
pub fn scan_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("打不开文件夹: {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| AUDIO_EXT.contains(&e.to_lowercase().as_str()))
                    .unwrap_or(false)
        })
        .collect();
    out.sort();
    Ok(out)
}

/// 一批音效的加载结果。
#[derive(Default)]
pub struct LoadedSounds {
    pub slots: Vec<SfxSlot>,
    /// 监听用样本（仅监听开启时非空）
    pub monitor_cache: Vec<Arc<Vec<f32>>>,
    /// 解码失败被跳过的文件：(文件名, 原因)
    pub skipped: Vec<(String, String)>,
}

/// 解码一批文件为音效槽位（非实时线程，可以慢）。
///
/// ⚠️ **单个坏文件不能拖垮整批**：用户从网上下的一堆素材里，
/// 混着空文件、截断文件、其实是网页的 .mp3 太常见了。
/// 早期版本遇到第一个坏文件就 `?` 返回，导致整个音效板起不来 ——
/// 现在跳过并记录，由上层报给用户。
pub fn load_slots(
    files: &[PathBuf],
    sample_rate: u32,
    looping: bool,
    keep_for_monitor: bool,
    names: &[String],
) -> LoadedSounds {
    let mut out = LoadedSounds::default();
    for (idx, p) in files.iter().enumerate() {
        // 有自定义显示名就用它，否则退回文件名
        let name = names
            .get(idx)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                p.file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "未命名".into())
            });

        let mut audio = match decode_file(p, sample_rate) {
            Ok(a) => a,
            Err(e) => {
                // 只取最内层的原因，避免刷屏
                let reason = e
                    .chain()
                    .last()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| e.to_string());
                log::warn!("跳过无法解码的文件 {}: {reason}", p.display());
                out.skipped.push((name, reason));
                continue;
            }
        };
        if audio.frames() == 0 {
            out.skipped.push((name, "文件里没有音频数据".into()));
            continue;
        }
        audio.normalize_peak(0.708); // -3 dBFS
        if keep_for_monitor {
            out.monitor_cache.push(Arc::new(audio.samples.clone()));
        }
        out.slots.push(SfxSlot::from_decoded(name, &audio, looping));
    }
    out
}

// ─────────────────────────────────────────── 宿主

struct Running {
    control: Arc<PlaybackControl>,
    stats: Arc<EngineStats>,
    shutdown: Arc<AtomicBool>,
    audio_thread: Option<JoinHandle<()>>,
    hotkey_thread: Option<JoinHandle<()>>,
    slots_meta: Vec<SlotDto>,
    out_device: String,
    in_device: String,
    started_at: std::time::Instant,
    /// 监听状态（三条触发路径共用）
    monitor: Arc<MonitorState>,
    skipped_notes: Vec<String>,
    /// 引擎句柄 —— 停止时把引擎取走，逼 cpal 泄漏的那份闭包不再持有样本
    render_slot: Arc<Mutex<Option<audio_core::engine::RenderProcessor>>>,
    capture_slot: Arc<Mutex<Option<audio_core::engine::CaptureProcessor>>>,
    /// 音效是否处于「开启」状态。
    ///
    /// false = **待机**：麦克风照常转发（队友听得到你说话），但音效不响应、热键已注销。
    /// 这是**故意**的 —— 用户大多数时候不用音效，但游戏麦克风一直指着虚拟声卡，
    /// 一旦停止就断掉转发，队友会突然听不到他。
    active: Arc<AtomicBool>,
    /// 热键管理器。**只在创建它的线程上用**（Windows 上它不 Send，
    /// 而且 Drop 会跨线程 DestroyWindow）—— 所以不共享给轮询线程。
    hotkeys: Arc<Mutex<Option<Hotkeys>>>,
    /// 给轮询线程用的句柄。`HotkeyPoller` 是天然 Send 的：
    /// 它只读 global-hotkey 的全局事件通道，不碰管理器。
    poller: Arc<Mutex<Option<hotkeys::HotkeyPoller>>>,
    /// 启动参数指纹 —— 一样的话「启动」可以走快路：不重开设备，只是重新激活
    cfg_key: String,
    /// 本次运行的完整启动参数（热插拔恢复时要照着它重建）
    cfg: StartConfig,
}

/// 启动参数指纹。差别只在**需要重开设备/重载样本**的项上。
///
/// 增益不在里面：它们是运行时可调的（`set_gains`），改增益不该触发重启。
pub fn cfg_fingerprint(cfg: &StartConfig) -> String {
    // ⚠️ 必须带上**完整文件列表和名字**，不能只算个数 ——
    //    用户在待机时把某个音效换掉（数量不变）的话，只算个数会走快路用到旧样本。
    format!(
        "in={:?}|out={:?}|buf={:?}|loop={}|files={:?}|names={:?}",
        cfg.input_device, cfg.output_device, cfg.buffer_ms, cfg.looping, cfg.files, cfg.names
    )
}

/// 最近一次 panic 的摘要（供界面显示）。
///
/// 为什么要有它：**打包后的 App 没有控制台**，panic 默认写到 stderr 就没了 ——
/// 用户只看到"程序有点怪"，或者更糟：程序还在但没声音，什么提示都没有。
static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

/// 上次 panic 的摘要（消费式：读一次就清掉，避免界面反复弹同一条）。
pub fn take_last_panic() -> Option<String> {
    LAST_PANIC.lock().unwrap_or_else(|e| e.into_inner()).take()
}

fn record_panic(msg: &str) {
    *LAST_PANIC.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg.to_string());
}

/// 启动时先把音效文件**读一下**，让系统尽早弹目录授权窗。
///
/// ## 为什么必须做
///
/// macOS 对 `~/Downloads`、桌面、文稿有独立的隐私保护。**在授权没确定之前，
/// 对这些目录的文件读取会直接阻塞**（不报错、不超时）。
///
/// 表现极具迷惑性：用户点「启动」，界面毫无反应 —— 因为 `start()` 卡在解码那一步。
/// 而错误只回到前端、不进日志，事后完全查不到。
/// （这个坑实际踩到过：用户配置里的 mp3 在 `~/Downloads`，
/// 授权窗挂着的时候「启动」就一直没反应；点了允许之后一切正常。）
///
/// 麦克风权限早就有类似的启动探测（`request_mic_permission`），
/// 文件权限是同一个道理 —— **把弹窗挪到启动时，而不是用户点「启动」时**。
pub fn probe_sound_file_access(paths: &[PathBuf]) {
    let first = paths.iter().find(|p| p.is_file()).cloned();
    let Some(p) = first else {
        return;
    };
    std::thread::Builder::new()
        .name("file-perm".into())
        .spawn(move || {
            use std::io::Read;
            // 只读开头一小段就够触发授权判定，不用把整个文件读进来
            match std::fs::File::open(&p) {
                Ok(mut f) => {
                    let mut buf = [0u8; 4096];
                    match f.read(&mut buf) {
                        Ok(n) => println!(
                            "[文件权限] 已确认可读（{}，读到 {n} 字节）—— {}",
                            p.file_name().unwrap_or_default().to_string_lossy(),
                            p.parent().unwrap_or(&p).display()
                        ),
                        Err(e) => println!("[文件权限] ⚠ 读取失败: {e}（{}）", p.display()),
                    }
                }
                Err(e) => println!("[文件权限] ⚠ 打不开: {e}（{}）", p.display()),
            }
        })
        .ok();
}

/// panic 日志文件路径（macOS 惯例：`~/Library/Logs/<App>/`）。
pub fn panic_log_path() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(
        std::path::PathBuf::from(home)
            .join("Library/Logs/音效板")
            .join("panic.log"),
    )
}

/// 装上 panic hook：**落盘 + 上报界面**。
///
/// 为什么必须做：这个程序大部分时间在后台（菜单栏常驻），而打包后没有控制台。
/// 一次 panic 如果只是写到 stderr，就等于什么都没发生 —— 用户不知道，
/// 我们查不到。落盘 + 界面提示是唯一能让它可见的办法。
pub fn install_panic_hook() {
    use std::io::Write;

    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "位置未知".into());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "（无消息）".into());
        let thread = std::thread::current()
            .name()
            .unwrap_or("未命名线程")
            .to_string();

        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = format!("[{secs}] panic @ {loc} | 线程 {thread} | {payload}");

        // ① 落盘
        if let Some(path) = panic_log_path() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = writeln!(f, "{line}");
            }
        }
        // ② 上报界面（界面轮询 status 时取走）
        record_panic(&format!("{payload}（{loc}）"));
        // ③ 仍然打到 stderr —— 终端启动时能直接看到
        eprintln!("[PANIC] {line}");
        default(info);
    }));
}

#[derive(Default)]
pub struct AudioHost {
    running: Mutex<Option<Running>>,
    last_error: Mutex<Option<String>>,
    /// 试听代号：每次试听自增，旧的那次看到代号变了就自行停止。
    /// 用它避免连点多个音效时声音叠在一起。
    preview_gen: Arc<std::sync::atomic::AtomicU64>,
    /// 手机遥控的共享状态（跨音频重启存活）
    remote: Mutex<Option<Arc<remote::Shared>>>,
    remote_urls: Mutex<Vec<String>>,
    /// 设备掉线提示（热插拔检测到设备不见了时写这里，前端据此报警）
    device_lost: Mutex<Option<String>>,
    /// 音频卡死提示（渲染回调不再被调用，或回调里 panic 过）
    audio_stalled: Mutex<Option<String>>,
    /// 程序启动时的系统默认输入设备 —— 「切回真实麦克风」要还原成它。
    ///
    /// 只在**第一次启动音频之前**记录：之后我们再改默认设备也不会污染这个基准。
    original_default_input: Mutex<Option<(u32, String)>>,
}

impl AudioHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn list_devices(&self) -> Result<Vec<DeviceDto>> {
        let b = CpalBackend::new();
        let din = b.default_input();
        let dout = b.default_output();
        let mut out = Vec::new();
        for d in b.devices()? {
            let is_default = match d.direction {
                audio_core::device::Direction::Input => din.as_ref() == Some(&d.id),
                audio_core::device::Direction::Output => dout.as_ref() == Some(&d.id),
            };
            out.push(DeviceDto {
                name: d.name.clone(),
                direction: d.direction.label().to_string(),
                is_default,
                channels: d.max_channels,
                sample_rate: d.default_sample_rate,
                is_virtual: audio_core::device::is_virtual_name(&d.name),
            });
        }
        Ok(out)
    }

    /// 自动探测虚拟声卡输出设备名。
    pub fn guess_output(&self) -> Option<String> {
        CpalBackend::new()
            .guess_virtual_output()
            .map(|id| id.to_string())
    }

    pub fn status(&self) -> StatusDto {
        let guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let Some(r) = guard.as_ref() else {
            let mut s = StatusDto::stopped();
            s.last_error = self
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            // ⚠️ 这条提前返回的路径也必须带上掉线原因 ——
            //    设备掉线导致引擎停掉时，界面正是靠它才知道"为什么没声音了"。
            s.device_lost = self
                .device_lost
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            // ⚠️ 提前返回这条路径同样要带上 —— panic 之后引擎往往正好是停的，
            //    而界面恰恰需要在这时告诉用户"出事了"。
            s.last_panic = take_last_panic();
            s.audio_stalled = self
                .audio_stalled
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            return s;
        };
        let st = r.stats.snapshot();
        let sr = 48_000.0;
        StatusDto {
            // running = **音效是否开启**，不等于"引擎是否存在"。
            // 待机时引擎照跑（麦克风在转发），但 running 必须是 false。
            running: r.active.load(Ordering::Relaxed),
            device: r.out_device.clone(),
            input_device: r.in_device.clone(),
            slots: r.slots_meta.clone(),
            playing: st.playing,
            active_slot: st.active_slot,
            playhead_secs: st.playhead as f32 / sr,
            ring_ms: st.ring_fill as f32 / 2.0 / (sr / 1000.0),
            underruns: st.underruns,
            overruns: st.overruns,
            limiter_db: st.limiter_gain_db,
            out_peak_db: st.out_peak_db,
            out_rms_db: st.out_rms_db,
            hotkeys_active: r
                .hotkeys
                .lock()
                .map(|h| h.as_ref().map(|x| x.is_bound()).unwrap_or(false))
                .unwrap_or(false),
            last_error: None,
            uptime_secs: r.started_at.elapsed().as_secs_f32(),
            remote_urls: self
                .remote_urls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            monitor_plays: r.monitor.plays.load(std::sync::atomic::Ordering::Relaxed),
            skipped: r.skipped_notes.clone(),
            passthrough: true, // 走到这里说明引擎还活着
            device_lost: self
                .device_lost
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            last_panic: take_last_panic(),
            audio_stalled: self
                .audio_stalled
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    /// 启动音频。
    ///
    /// `app` 用于把**热键绑定调度回主线程** —— macOS 上 Carbon 热键的管理器
    /// 必须在主线程创建，否则事件永远不会派发（而且不报错，表现为"按键没反应"）。
    /// `--smoke` 等无 GUI 场景传 `None`。
    pub fn start(&self, cfg: StartConfig, app: Option<&tauri::AppHandle>) -> Result<()> {
        // 设备与音效列表都没变（典型情况：从「待机」点「开启音效」）→
        // **不重开音频流**，只把音效重新打开。瞬间生效，用户来回切毫无卡顿。
        if let Some(k) = self.running_cfg_key() {
            if k == cfg_fingerprint(&cfg) {
                return self.activate(app);
            }
            println!("[启动] 设备/音效列表有变化 → 彻底重启音频");
        }
        // ⚠️ 必须用 release 而不是 stop：stop 只进待机（引擎照跑），
        //    紧接着覆盖 `running` 会让**旧引擎永远活着** —— 线程和缓冲都收不回来。
        //    这个泄漏被"停止=彻底关闭"的旧语义掩盖了很久，改待机后才暴露出来。
        self.release();
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;

        let b = CpalBackend::new();

        // 1. 设备
        // ⚠️ **必须校验设备还在不在**，不能直接拿配置里的名字去开流。
        //
        // 原因是实测出来的：设备拔掉后（换机器、拔耳机），名字对应的设备已经不存在，
        // 而 CoreAudio 对不存在设备的查询会**永久阻塞** —— `start()` 卡死不返回，
        // 界面表现就是"点了「启动」什么反应都没有"，日志也停在参数那一行。
        //
        // 现在的行为：名字对不上 → 退回系统默认，并**明确告诉用户换成了什么**。
        let all = b.devices()?;
        let mut subst: Vec<String> = Vec::new();

        let in_id = match cfg.input_device.as_deref().filter(|n| !n.is_empty()) {
            Some(n) => match all
                .iter()
                .find(|d| d.name == n && d.direction == audio_core::device::Direction::Input)
            {
                Some(d) => d.id.clone(),
                None => {
                    let d = b.default_input().context("系统没有默认输入设备")?;
                    println!("[设备] ⚠ 配置里的麦克风「{n}」已不在 → 改用系统默认「{d}」");
                    subst.push(format!("麦克风「{n}」已不在，改用系统默认"));
                    d
                }
            },
            None => b.default_input().context("系统没有默认输入设备")?,
        };

        let out_id = match cfg.output_device.as_deref().filter(|n| !n.is_empty()) {
            Some(n) => match all
                .iter()
                .find(|d| d.name == n && d.direction == audio_core::device::Direction::Output)
            {
                Some(d) => d.id.clone(),
                None => {
                    let d = b.guess_virtual_output().context(format!(
                        "配置里的输出设备「{n}」已不在，而且也没探测到别的虚拟声卡"
                    ))?;
                    println!("[设备] ⚠ 配置里的输出「{n}」已不在 → 改用「{d}」");
                    subst.push(format!("输出设备「{n}」已不在，改用「{d}」"));
                    d
                }
            },
            None => b
                .guess_virtual_output()
                .context("没探测到虚拟声卡。请先在设备下拉框里选一个输出设备（例如 VB-Cable）")?,
        };

        // 把替换这件事告诉界面（用户得知道"为什么声音从我耳机跑到别处去了"）
        if !subst.is_empty() {
            self.set_device_lost(Some(subst.join("；")));
        } else {
            self.set_device_lost(None);
        }

        // 2. 格式协商
        let in_fmt = b.input_format(&in_id, Some(StreamFormat::new(48_000, 1)))?;
        let out_fmt = b.output_format(&out_id, Some(StreamFormat::new(48_000, 2)))?;

        // 3. 解码音效
        let files: Vec<PathBuf> = cfg.files.iter().map(PathBuf::from).collect();
        // 监听输出设备（系统默认输出，通常是耳机/扬声器）
        let monitor_out = if cfg.monitor {
            // 显式指定了就用它，否则用系统默认输出
            let target = cfg
                .monitor_device
                .as_ref()
                .filter(|s| !s.is_empty())
                .map(|n| DeviceId::new(n.clone()))
                .or_else(|| b.default_output());
            match target {
                Some(d) => match b.output_format(&d, Some(StreamFormat::new(48_000, 2))) {
                    Ok(f) => {
                        println!("[监听] 输出设备 = {d}  {f}");
                        Some((d.to_string(), f))
                    }
                    Err(e) => {
                        println!("[监听] ❌ 不可用（输出格式协商失败）: {e:#}");
                        None
                    }
                },
                None => {
                    println!("[监听] ❌ 不可用：找不到输出设备（既没指定也没默认）");
                    None
                }
            }
        } else {
            None
        };

        let loaded = load_slots(
            &files,
            out_fmt.sample_rate,
            cfg.looping,
            monitor_out.is_some(),
            &cfg.names,
        );
        if loaded.slots.is_empty() && !files.is_empty() {
            anyhow::bail!(
                "这 {} 个音效文件全都无法解码。第一个失败原因：{}",
                files.len(),
                loaded
                    .skipped
                    .first()
                    .map(|(_, r)| r.as_str())
                    .unwrap_or("未知")
            );
        }
        let LoadedSounds {
            mut slots,
            monitor_cache,
            skipped,
        } = loaded;
        // 把每个音效自己的音量写进槽位（引擎会在每帧读取）
        for (i, s) in slots.iter_mut().enumerate() {
            s.gain_db = cfg.gains.get(i).copied().unwrap_or(0.0);
        }
        let slots_meta: Vec<SlotDto> = slots
            .iter()
            .enumerate()
            .map(|(i, s)| SlotDto {
                index: i,
                name: s.name.clone(),
                file: files
                    .get(i)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                duration: s.duration_secs(out_fmt.sample_rate),
                peak_db: 0.0,
                hotkey: cfg.hotkeys.get(i).cloned().unwrap_or_default(),
                gain_db: s.gain_db,
            })
            .collect();

        let slot_gains: Vec<f32> = slots_meta.iter().map(|s| s.gain_db).collect();
        let monitor = Arc::new(MonitorState {
            out: monitor_out,
            cache: monitor_cache,
            gains: slot_gains,
            gen: std::sync::atomic::AtomicU64::new(0),
            plays: std::sync::atomic::AtomicU64::new(0),
        });

        // 4. 组装引擎（纯计算，不碰设备）
        let parts = EngineParts::build(in_fmt, out_fmt, slots, cfg.buffer_ms);
        let control = parts.control.clone();
        let stats = parts.stats.clone();
        control.mic_gain_db.set(cfg.mic_gain_db);
        control.sfx_gain_db.set(cfg.sfx_gain_db);

        // ⚠️ cpal 在 macOS 上 drop 流时**不会释放回调闭包捕获的数据**（实测：捕获 2.5MB
        //    的闭包每次开流泄漏 2.5MB，线性增长；不捕获数据的闭包则平稳）。
        //    引擎持有全部音效样本，直接塞进闭包的话每次「停止/启动」就泄漏一整套音效。
        //
        //    对策：引擎装进 Arc<Mutex<Option<_>>>，主线程也持一份；停止时把引擎**取走**，
        //    样本立即释放，被 cpal 泄漏掉的只剩一个空壳（几十字节）。
        let render_slot = Arc::new(Mutex::new(Some(parts.render)));
        let capture_slot = Arc::new(Mutex::new(Some(parts.capture)));
        let render_for_audio = render_slot.clone();
        let capture_for_audio = capture_slot.clone();

        // 5. 专用音频线程：开流后驻留。开流失败通过 channel 报回。
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        let out_name = out_id.to_string();
        let in_name = in_id.to_string();

        let audio_thread = std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || {
                let b = CpalBackend::new();
                // **先输出后输入**：输出先开始消费，输入才不会把缓冲灌满
                let out_stream = match b.open_output(
                    &out_id,
                    Some(out_fmt),
                    Box::new(move |d: &mut [f32]| match render_for_audio.try_lock() {
                        Ok(mut g) => match g.as_mut() {
                            Some(r) => r.process(d),
                            None => d.fill(0.0), // 已停止：输出静音
                        },
                        // 主线程正在换引擎，这一块输出静音（每次启停最多一块）
                        Err(_) => d.fill(0.0),
                    }),
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = tx.send(Err(format!("打开输出设备失败: {e}")));
                        return;
                    }
                };
                let in_stream = match b.open_input(
                    &in_id,
                    Some(in_fmt),
                    Box::new(move |d: &[f32]| {
                        if let Ok(mut g) = capture_for_audio.try_lock() {
                            if let Some(c) = g.as_mut() {
                                c.process(d);
                            }
                        }
                    }),
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = tx.send(Err(format!("打开输入设备失败: {e}")));
                        return;
                    }
                };
                let _ = tx.send(Ok(()));
                while !sd.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                drop(in_stream);
                drop(out_stream);
            })?;

        match rx.recv_timeout(Duration::from_secs(8)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                shutdown.store(true, Ordering::Relaxed);
                let _ = audio_thread.join();
                anyhow::bail!(e);
            }
            Err(_) => {
                shutdown.store(true, Ordering::Relaxed);
                anyhow::bail!("音频设备启动超时（8 秒）—— 设备可能被其他程序独占");
            }
        }

        // 6. 热键轮询线程
        let active = Arc::new(AtomicBool::new(true));
        let hotkeys: Arc<Mutex<Option<Hotkeys>>> = Arc::new(Mutex::new(None));
        let poller: Arc<Mutex<Option<hotkeys::HotkeyPoller>>> = Arc::new(Mutex::new(None));
        let hotkey_thread = if cfg.hotkeys.is_empty() || cfg.hotkeys.len() != slots_meta.len() {
            None
        } else {
            let bound = bind_hotkeys_on_main_thread(app, &cfg.hotkeys);
            match &bound {
                Ok((_, how)) => println!("[热键] 绑定成功（{how}）：{}", cfg.hotkeys.join(", ")),
                Err(e) => println!("[热键] ❌ 绑定失败: {e:#}"),
            }
            match bound.map(|(h, _)| h) {
                Ok(hk) => {
                    // 轮询线程只拿 poller —— 见 HotkeyPoller 的注释
                    *poller.lock().unwrap_or_else(|e| e.into_inner()) = Some(hk.poller());
                    *hotkeys.lock().unwrap_or_else(|e| e.into_inner()) = Some(hk);
                    let ctl = control.clone();
                    let sd = shutdown.clone();
                    // ⚠️ 热键路径必须也触发监听 —— 这是游戏内的主要触发方式，
                    //    早期只有界面点击那条路会监听，导致"按了热键自己听不到"。
                    let mon = monitor.clone();
                    let act = active.clone();
                    let hks = poller.clone();
                    Some(
                        std::thread::Builder::new()
                            .name("hotkeys".into())
                            .spawn(move || {
                                while !sd.load(Ordering::Relaxed) {
                                    // 待机时热键已注销，poll 返回 None；
                                    // 这里再挡一层，避免注销与轮询之间的竞态漏进来一次。
                                    let slot = if act.load(Ordering::Relaxed) {
                                        hks.lock()
                                            .unwrap_or_else(|e| e.into_inner())
                                            .as_ref()
                                            .and_then(|h| h.poll())
                                    } else {
                                        None
                                    };
                                    if let Some(slot) = slot {
                                        if ctl.play(slot) {
                                            mon.fire(slot);
                                        }
                                    }
                                    std::thread::sleep(Duration::from_millis(12));
                                }
                            })?,
                    )
                }
                Err(_) => None, // 已经在上面 warn 过了
            }
        };

        // 手机遥控：作为热键失效时的兜底通路，随引擎一起待命
        match cfg.remote_port {
            Some(port) => match self.start_remote(port, "0.0.0.0".to_string()) {
                Ok(urls) => {
                    if let Some(sh) = self
                        .remote
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                    {
                        sh.set(
                            control.clone(),
                            slots_meta.iter().map(|s| s.name.clone()).collect(),
                        );
                        // 遥控路径同理：三条触发路径都要监听
                        let mon = monitor.clone();
                        sh.set_hook(Arc::new(move |slot| mon.fire(slot)));
                    }
                    println!("[遥控] 已就绪: {}", urls.join("  "));
                    // 防火墙会拦住未签名程序的入站连接，手机就连不上 —— 直接给出放行命令
                    if let Some(h) = remote::firewall_hint(
                        &std::env::current_exe()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                    ) {
                        println!("[遥控] ⚠ {h}");
                    }
                }
                Err(e) => log::warn!("手机遥控启动失败: {e:#}"),
            },
            None => {
                if let Some(sh) = self
                    .remote
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    sh.clear(); // 关掉开关后，手机端会显示"音频未启动"
                }
            }
        }

        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = Some(Running {
            control,
            stats,
            shutdown,
            audio_thread: Some(audio_thread),
            hotkey_thread,
            slots_meta,
            out_device: out_name,
            in_device: in_name,
            started_at: std::time::Instant::now(),
            monitor,
            render_slot,
            capture_slot,
            skipped_notes: skipped.iter().map(|(n, r)| format!("{n}（{r}）")).collect(),
            active,
            hotkeys,
            poller,
            cfg_key: cfg_fingerprint(&cfg),
            cfg,
        });
        Ok(())
    }

    /// 启动手机遥控（幂等）。返回可访问的地址列表。
    ///
    /// 服务**只在第一次真正起监听**，之后跨音频重启一直存活 ——
    /// 否则手机上刷新一下就再也连不上了。
    pub fn start_remote(&self, port: u16, bind: String) -> Result<Vec<String>> {
        let mut g = self.remote.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_some() {
            return Ok(self
                .remote_urls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone());
        }
        let shared = Arc::new(remote::Shared::default());
        let urls = remote::serve(shared.clone(), remote::RemoteConfig { port, bind })?;
        *g = Some(shared);
        *self.remote_urls.lock().unwrap_or_else(|e| e.into_inner()) = urls.clone();
        Ok(urls)
    }

    /// 停止音效 —— **但继续转发麦克风**。
    ///
    /// 为什么不整个停掉：用户大多数时候不用音效，而游戏的麦克风**一直**指着虚拟声卡。
    /// 一旦连转发也断了，虚拟声卡里就是一片死寂（实测精确为 0），**队友会突然听不到他说话**。
    /// 所以「停止」的语义是「关掉音效」，不是「把我变成哑巴」。
    ///
    /// 进入待机时会**注销热键** —— 否则 F1/F2 仍被我们截走，游戏里按这两个键毫无反应。
    pub fn stop(&self) {
        let guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let Some(r) = guard.as_ref() else {
            return;
        };
        // 先把正在播的音效掐掉，再摘热键
        r.control.stop_all();
        r.active.store(false, Ordering::Relaxed);
        if let Ok(mut hk) = r.hotkeys.lock() {
            if let Some(h) = hk.as_mut() {
                if let Err(e) = h.unbind_all() {
                    println!("[待机] 注销热键失败: {e:#}");
                }
            }
        }
        // 轮询线程那边也断掉 —— 双保险，确保待机时按 F1/F2 什么都不做
        *r.poller.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // 遥控那边也断开，避免待机时被人从手机触发
        if let Some(sh) = self
            .remote
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            sh.clear();
        }
        println!("[待机] 音效已关闭；麦克风仍在转发（队友照常听得到你）");
    }

    /// 彻底释放音频设备（退出、或要换设备时用）。
    ///
    /// ⚠️ 调用后虚拟声卡里就没人写了 —— 如果游戏的麦克风还指着它，队友会听不到你。
    pub fn release(&self) {
        if let Some(sh) = self
            .remote
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            sh.clear();
        }
        let mut guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut r) = guard.take() {
            r.shutdown.store(true, Ordering::Relaxed);
            if let Some(h) = r.audio_thread.take() {
                let _ = h.join();
            }
            // 引擎取走 → 音效样本立即释放（cpal 泄漏的闭包只剩空壳）
            if let Ok(mut g) = r.render_slot.lock() {
                *g = None;
            }
            if let Ok(mut g) = r.capture_slot.lock() {
                *g = None;
            }
            if let Some(h) = r.hotkey_thread.take() {
                let _ = h.join();
            }
        }
    }

    /// 重新激活音效（从待机回到开启）。设备没变的话**不重开流**，瞬间生效。
    pub fn activate(&self, app: Option<&tauri::AppHandle>) -> Result<()> {
        let guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let Some(r) = guard.as_ref() else {
            anyhow::bail!("音频还没启动");
        };
        // 重新绑定热键
        let specs: Vec<String> = r.slots_meta.iter().map(|s| s.hotkey.clone()).collect();
        if !specs.is_empty() && specs.iter().all(|k| !k.is_empty()) {
            let mut slot = r.hotkeys.lock().unwrap_or_else(|e| e.into_inner());
            let mut poller_ok = false;
            match slot.as_mut() {
                // 复用已有的管理器重新注册（新建一个会超时，见 Hotkeys::rebind 注释）
                Some(h) => match h.rebind(&specs) {
                    Ok(()) => {
                        println!("[热键] 重新绑定成功：{}", specs.join(", "));
                        poller_ok = true;
                    }
                    Err(e) => println!("[热键] ❌ 重新绑定失败: {e:#}"),
                },
                None => match bind_hotkeys_on_main_thread(app, &specs) {
                    Ok((h, how)) => {
                        println!("[热键] 绑定成功（{how}）：{}", specs.join(", "));
                        *slot = Some(h);
                        poller_ok = true;
                    }
                    Err(e) => println!("[热键] ❌ 绑定失败: {e:#}"),
                },
            }
            // 绑定成功才把轮询句柄放回去 —— 否则按了也没人处理
            if poller_ok {
                let p = slot.as_ref().map(|h| h.poller());
                *r.poller.lock().unwrap_or_else(|e| e.into_inner()) = p;
            }
        }
        r.active.store(true, Ordering::Relaxed);
        println!("[开启] 音效已恢复；热键重新绑定");
        Ok(())
    }

    /// 当前运行的启动参数指纹（用来判断「启动」能不能走快路）。
    pub fn running_cfg_key(&self) -> Option<String> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|r| r.cfg_key.clone())
    }

    /// 热插拔检测：本次运行用的设备还在不在？
    ///
    /// 返回 `(输入设备是否丢失, 输出设备是否丢失, 丢失的设备名)`。
    pub fn check_devices(&self) -> Option<(bool, bool, String)> {
        // 先在锁里把要查的东西取出来，**枚举时不再持锁** ——
        // 否则 start() 末尾要写 running 时会干等。
        let (in_name, out_name, in_id, out_id) = {
            let g = self.running.lock().unwrap_or_else(|e| e.into_inner());
            let r = g.as_ref()?;
            (
                r.in_device.clone(),
                r.out_device.clone(),
                r.cfg.input_device.clone(),
                r.cfg.output_device.clone(),
            )
        };
        // ⚠️ 这里**绝对不能**用 cpal 枚举设备：在另一个线程创建第二个 cpal 后端
        //    会和主线程"开音频流"撞死（实测 start() 直接卡住）。用自己的 CoreAudio 查询。
        #[cfg(target_os = "macos")]
        let exists = {
            let ins = crate::macos_audio::input_devices();
            let outs = crate::macos_audio::output_devices();
            move |name: &str| {
                ins.iter().any(|(_, n)| n == name) || outs.iter().any(|(_, n)| n == name)
            }
        };
        #[cfg(not(target_os = "macos"))]
        let exists = {
            let devs = CpalBackend::new().devices().ok()?;
            move |name: &str| devs.iter().any(|d| d.name == name)
        };

        // 只检查用户**显式指定**的设备 —— 没指定就是用系统默认，
        // 系统默认变了由系统自己负责，不算"掉线"。
        let in_lost = in_id.as_deref().map(|_| !exists(&in_name)).unwrap_or(false);
        let out_lost = out_id
            .as_deref()
            .map(|_| !exists(&out_name))
            .unwrap_or(false);
        if in_lost || out_lost {
            let which = if out_lost { &out_name } else { &in_name };
            Some((in_lost, out_lost, which.clone()))
        } else {
            None
        }
    }

    pub fn set_device_lost(&self, msg: Option<String>) {
        *self.device_lost.lock().unwrap_or_else(|e| e.into_inner()) = msg;
    }

    /// 输入设备掉了 → 自动换到系统默认麦克风重开。
    ///
    /// ⚠️ **输出设备掉了不做自动替换**：系统默认输出通常是扬声器，
    /// 自动换过去会把音效外放出来（还可能在语音时造成回声）——
    /// 那比没声音更糟。这种情况只报警，让用户自己选。
    pub fn recover_input_device(&self, app: Option<&tauri::AppHandle>) -> Result<String> {
        let cfg = {
            let g = self.running.lock().unwrap_or_else(|e| e.into_inner());
            g.as_ref().map(|r| r.cfg.clone())
        };
        let Some(mut cfg) = cfg else {
            anyhow::bail!("音频没在跑，无需恢复");
        };
        let old = cfg.input_device.clone().unwrap_or_default();
        cfg.input_device = None; // 用系统默认麦克风
        let saved = cfg.names.clone();
        let _ = saved;
        // 指纹会变（设备不同了）→ start 会走完整重启
        self.start(cfg, app)?;
        Ok(old)
    }

    /// 渲染回调累计处理了多少帧。**回调停了它就不涨** —— 看门狗靠这个判断死活。
    pub fn rendered_frames(&self) -> u64 {
        use std::sync::atomic::Ordering;
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|r| r.stats.rendered_frames.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// 音频是否还活着（引擎在跑，且回调在推进）。false = 该重启了。
    pub fn audio_alive(&self) -> bool {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub fn set_audio_stalled(&self, msg: Option<String>) {
        *self.audio_stalled.lock().unwrap_or_else(|e| e.into_inner()) = msg;
    }

    /// 按**上次的配置**把音频重启一遍（看门狗自救用）。
    ///
    /// 注意要绕开 `start()` 的快路：设备没变时它会直接 activate，
    /// 而我们现在要的恰恰是**重开流**。
    pub fn restart_audio(&self, app: Option<&tauri::AppHandle>) -> Result<()> {
        let cfg = {
            let g = self.running.lock().unwrap_or_else(|e| e.into_inner());
            g.as_ref().map(|r| r.cfg.clone())
        };
        let Some(cfg) = cfg else {
            anyhow::bail!("没有可恢复的配置");
        };
        self.release();
        backend_cpal::clear_engine_panic();
        self.start(cfg, app)
    }

    /// 切回真实麦克风。
    ///
    /// 返回 `(是否真的切成功, 切到了哪个设备, 当前实际默认设备)`。
    /// **必须回读确认** —— 有些虚拟声卡驱动的 set 会返回成功但根本不生效。
    #[cfg(target_os = "macos")]
    pub fn revert_to_real_mic(&self) -> Result<(bool, String, String)> {
        // 先释放设备：占着不放时某些驱动会拒绝切换
        self.release();

        let original = self.original_default_input();
        let target = original
            .as_ref()
            .and_then(|(_, name)| crate::macos_audio::find_input_by_name(name))
            .or_else(crate::macos_audio::first_real_input)
            .context("找不到真实麦克风设备 —— 请检查系统声音设置")?;

        let (id, name) = target;
        let ok = crate::macos_audio::set_default_input(id);
        let current = crate::macos_audio::default_input()
            .map(|(_, n)| n)
            .unwrap_or_default();
        println!(
            "[回退] 系统默认输入 → 「{name}」：{}（当前实际「{current}」）",
            if ok { "成功" } else { "❌ 没生效" }
        );
        Ok((ok, name, current))
    }

    /// 程序启动时记录的默认输入设备（用于「切回真实麦克风」）。
    #[cfg(target_os = "macos")]
    pub fn original_default_input(&self) -> Option<(u32, String)> {
        self.original_default_input
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 记录当前默认输入设备（幂等，只记第一次）。
    #[cfg(target_os = "macos")]
    pub fn remember_default_input(&self) {
        let mut g = self
            .original_default_input
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            *g = crate::macos_audio::default_input();
            if let Some((id, n)) = g.as_ref() {
                println!("[回退] 已记录原始默认输入设备：{id} 「{n}」");
            }
        }
    }

    pub fn play(&self, slot: usize) -> Result<bool> {
        let guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let Some(r) = guard.as_ref() else {
            anyhow::bail!("音频还没启动");
        };
        let ok = r.control.play(slot);
        if ok {
            // 监听：把同一段音效也放给用户自己听（游戏里唯一能感知到的反馈）
            r.monitor.fire(slot);
        }
        Ok(ok)
    }

    pub fn stop_all(&self) -> Result<()> {
        let guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = guard.as_ref() {
            r.control.stop_all();
        }
        Ok(())
    }

    /// 调整单个音效的音量。**下一帧生效**，不必重播。
    pub fn set_slot_gain(&self, slot: usize, db: f32) -> Result<bool> {
        let guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(r) => Ok(r.control.set_slot_gain(slot, db)),
            None => Ok(false), // 没在跑就静默忽略，UI 仍可先调
        }
    }

    pub fn set_gains(&self, mic_db: f32, sfx_db: f32) {
        let guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = guard.as_ref() {
            r.control.mic_gain_db.set(mic_db);
            r.control.sfx_gain_db.set(sfx_db);
        }
    }

    /// 试听：把音效放给**自己**听（走系统默认输出），不送进虚拟麦克风。
    ///
    /// 很必要的功能 —— 没有它，想确认某个音效是什么就只能播给全队听。
    ///
    /// **解码在这里同步做**：这样文件不存在 / 格式损坏能立刻报给调用方。
    /// （早期版本把解码也丢进线程，结果坏文件只会静默失败，用户点了没反应也没提示。）
    pub fn preview(&self, path: PathBuf, gain_db: f32) -> Result<()> {
        if !path.is_file() {
            anyhow::bail!("文件不存在: {}", path.display());
        }

        // 同步部分：选设备、协商格式、解码 —— 任何一步失败都直接返回给调用方
        let b = CpalBackend::new();
        let dev = b
            .default_output()
            .context("系统没有默认输出设备（试听需要它）")?;
        let dev_name = dev.to_string();
        let fmt = b.output_format(&dev, Some(StreamFormat::new(48_000, 2)))?;
        let mut audio = decode_file(&path, fmt.sample_rate)
            .with_context(|| format!("解码失败: {}", path.display()))?;
        if audio.frames() == 0 {
            anyhow::bail!("这个文件里没有音频数据: {}", path.display());
        }
        audio.normalize_peak(0.708);

        // 异步部分：只负责把已解码好的样本喂给声卡
        let gen = self
            .preview_gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let gen_arc = self.preview_gen.clone();
        std::thread::Builder::new()
            .name("preview".into())
            .spawn(move || {
                if let Err(e) = preview_play(audio, dev_name, fmt, gain_db, gen, gen_arc) {
                    log::warn!("试听播放失败: {e:#}");
                }
            })?;
        Ok(())
    }

    /// 停止当前试听。
    pub fn stop_preview(&self) {
        self.preview_gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn record_error(&self, e: impl Into<String>) {
        *self.last_error.lock().unwrap_or_else(|x| x.into_inner()) = Some(e.into());
    }
}

impl Drop for AudioHost {
    fn drop(&mut self) {
        self.stop();
    }
}

// ─────────────────────────────────────────── 麦克风权限

/// 在**启动时**就把系统麦克风权限问出来。
///
/// 为什么必须提前问：macOS 只有在 `Info.plist` 里有 `NSMicrophoneUsageDescription`
/// 时才会授权（缺了会直接拒绝，日志里是
/// "Refusing authorization request ... without NSMicrophoneUsageDescription key"）。
/// 而权限是**在首次访问麦克风那一刻**才弹窗的 —— 如果那一刻正好是用户点「启动」，
/// 音频流会带着"静音"状态开起来，用户点了「允许」也不会自动恢复，必须重开程序。
/// 所以这里在界面刚起来时就先触发一次。
///
/// 做法：开一个极短的输入流再关掉。没有权限时会弹窗，有权限时瞬间完成。
pub fn request_mic_permission() {
    std::thread::Builder::new()
        .name("mic-permission".into())
        .spawn(|| {
            let dbg = |m: &str| {
                if let Ok(p) = std::env::var("MIXER_PERM_LOG") {
                    use std::io::Write;
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(p)
                    {
                        let _ = writeln!(f, "{m}");
                    }
                }
            };
            dbg("开始申请麦克风权限");
            let b = CpalBackend::new();
            let Some(dev) = b.default_input() else {
                dbg("没有默认输入设备");
                return;
            };
            dbg(&format!("默认输入设备 {dev}"));
            let Ok(fmt) = b.input_format(&dev, Some(StreamFormat::new(48_000, 1))) else {
                dbg("输入格式协商失败");
                return;
            };
            dbg(&format!("输入格式 {fmt}"));
            // 开一下、让系统弹窗，然后立刻关掉
            match b.open_input(&dev, Some(fmt), Box::new(|_: &[f32]| {})) {
                Ok(stream) => {
                    std::thread::sleep(Duration::from_millis(800));
                    drop(stream);
                    dbg("输入流已打开又关闭（系统应已授权或弹窗）");
                }
                Err(e) => dbg(&format!("打开输入流失败: {e:#}")),
            }
        })
        .ok();
}

// ─────────────────────────────────────────── 监听

/// 监听状态：触发的音效同时放给**用户自己**听。
///
/// 抽成独立对象的原因：触发音效有**三条路径** —— 界面点击、全局热键、手机遥控。
/// 监听必须三条都生效，尤其**热键**（游戏内的主要方式）。
/// 早期实现把监听写死在界面点击那条路上，结果热键触发时自己完全听不到 ——
/// 而这个功能的全部意义就是游戏内反馈。
pub struct MonitorState {
    out: Option<(String, StreamFormat)>,
    cache: Vec<Arc<Vec<f32>>>,
    gains: Vec<f32>,
    gen: std::sync::atomic::AtomicU64,
    /// 实际触发次数（自检用：确认监听真的被调用过）
    plays: std::sync::atomic::AtomicU64,
}

impl MonitorState {
    /// 触发某个槽位的监听。没配置监听设备时静默跳过。
    pub fn fire(&self, slot: usize) {
        let Some((dev, fmt)) = self.out.clone() else {
            return;
        };
        let Some(samples) = self.cache.get(slot).cloned() else {
            return;
        };
        let gain = self.gains.get(slot).copied().unwrap_or(0.0);
        self.fire_samples(samples, dev, fmt, gain);
    }

    fn fire_samples(&self, samples: Arc<Vec<f32>>, dev: String, fmt: StreamFormat, gain_db: f32) {
        use std::sync::atomic::Ordering;
        self.plays.fetch_add(1, Ordering::Relaxed);
        let gen = self.gen.fetch_add(1, Ordering::SeqCst) + 1;
        let gen_arc = Arc::new(std::sync::atomic::AtomicU64::new(gen));
        println!(
            "[监听] ▶ 送往 {dev}（{} 样本，{gain_db} dB）",
            samples.len()
        );
        std::thread::Builder::new()
            .name("monitor".into())
            .spawn(move || {
                if let Err(e) = play_samples(samples, dev, fmt, gain_db, gen, gen_arc) {
                    println!("[监听] ❌ 播放失败: {e:#}");
                }
            })
            .ok();
    }
}

// ─────────────────────────────────────────── 热键绑定

/// 在**主线程**上绑定热键并在需要时取回结果。
///
/// macOS 的 Carbon 热键要求管理器在主线程创建，而 Tauri 的命令处理器不保证
/// 跑在主线程，所以这里显式调度过去再等结果回来。
fn bind_hotkeys_on_main_thread(
    app: Option<&tauri::AppHandle>,
    keys: &[String],
) -> Result<(Hotkeys, &'static str)> {
    if hotkeys::is_main_thread() {
        return Ok((Hotkeys::bind_to_slots(keys)?, "调用方本来就在主线程"));
    }
    let Some(app) = app else {
        // 无 GUI 场景（--smoke）：直接绑，让 new_manager 给出明确报错
        return Ok((Hotkeys::bind_to_slots(keys)?, "无 GUI 上下文"));
    };

    let (tx, rx) = mpsc::channel();
    let keys: Vec<String> = keys.to_vec();
    app.run_on_main_thread(move || {
        let _ = tx.send(Hotkeys::bind_to_slots(&keys));
    })
    .context("把热键绑定调度到主线程失败")?;

    let hk = rx
        .recv_timeout(Duration::from_secs(6))
        .map_err(|_| anyhow::anyhow!("在主线程绑定热键超时（6 秒）"))??;
    Ok((hk, "已调度到主线程"))
}

// ─────────────────────────────────────────── 试听

fn lin(db: f32) -> f32 {
    if db <= -60.0 {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

fn preview_play(
    audio: audio_core::decode::DecodedAudio,
    dev_name: String,
    fmt: StreamFormat,
    gain_db: f32,
    my_gen: u64,
    gen: Arc<std::sync::atomic::AtomicU64>,
) -> Result<()> {
    play_samples(Arc::new(audio.samples), dev_name, fmt, gain_db, my_gen, gen)
}

/// 把一段样本送到指定输出设备播放（试听与监听共用）。
fn play_samples(
    samples_owned: Arc<Vec<f32>>,
    dev_name: String,
    fmt: StreamFormat,
    gain_db: f32,
    my_gen: u64,
    gen: Arc<std::sync::atomic::AtomicU64>,
) -> Result<()> {
    use std::sync::atomic::Ordering;

    let b = CpalBackend::new();
    let dev = DeviceId::new(dev_name);
    let total_frames = samples_owned.len() / 2;
    let g = lin(gain_db);
    let samples = samples_owned;
    let mut pos = 0usize;
    let src = samples;

    // 输出回调：从已解码的缓冲里顺序取，取完就静音
    let stream = b.open_output(
        &dev,
        Some(fmt),
        Box::new(move |out: &mut [f32]| {
            for frame in out.as_chunks_mut::<2>().0 {
                if pos + 1 < src.len() {
                    frame[0] = src[pos] * g;
                    frame[1] = src[pos + 1] * g;
                    pos += 2;
                } else {
                    frame[0] = 0.0;
                    frame[1] = 0.0;
                }
            }
        }),
    )?;

    // 播放期间轮询代号：被新的试听顶替就提前收工
    let dur = Duration::from_secs_f32(total_frames as f32 / fmt.sample_rate as f32 + 0.1);
    let t0 = std::time::Instant::now();
    while t0.elapsed() < dur {
        if gen.load(Ordering::SeqCst) != my_gen {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    drop(stream);
    Ok(())
}
