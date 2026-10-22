//! 音频宿主 —— GUI 与音频引擎之间的一层。
//!
//! ## 为什么要单独一层
//!
//! cpal 的 `Stream` 在 macOS 上**不是 `Send`**（内部含平台标记），
//! 所以流必须留在创建它的线程上。这里用一个**专用音频线程**持有流，
//! GUI 线程只通过 `PlaybackControl`（无锁原子量 + SPSC 命令队列）操控它。
//!
//! 好处：GUI 卡顿、重绘、甚至 WebView 崩溃都不会影响音频实时线程。

use audio_session::{Library, MonitorOptions, Playback, Session, SessionOptions, SoundSpec};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use audio_core::decode::decode_file;
use audio_core::device::{AudioBackend, DeviceId, StreamFormat};
use audio_core::engine::{AtomicF32, EngineStats};
use audio_core::limiter::Limiter;
use backend_cpal::CpalBackend;
use hotkeys::{HotkeyGroup, HotkeyGroups, HotkeyManager, Hotkeys};

#[cfg(feature = "voice")]
use crate::voice::{VoiceHotkeyAction, VoiceHotkeyOutcome, VoiceService, DEFAULT_VOICE_BLOCK_MS};
use crate::voice::{VoiceParamsDto, VoicePresetDto, VoiceStartConfig, VoiceStatusDto};
#[cfg(feature = "voice")]
use audio_core::voice::{
    PreviewError, VoiceControl, VoicePhase, VoicePreviewClip, VoicePreviewControl,
};

// ─────────────────────────────────────────── DTO

#[derive(Debug, Clone, Serialize)]
pub struct DeviceDto {
    pub id: String,
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
    pub source_index: usize,
    pub id: String,
    pub name: String,
    pub file: String,
    pub duration: f32,
    pub peak_db: f32,
    pub hotkey: String,
    /// 该音效自己的音量（dB）
    pub gain_db: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StartConfig {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub files: Vec<String>,
    #[serde(default)]
    pub ids: Vec<String>,
    #[serde(default)]
    pub tags: Vec<Vec<String>>,
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
    /// 没有它，用户在游戏里按键后什么都听不到，只能盲猜对方有没有听到。
    #[serde(default)]
    pub monitor: bool,
    /// 监听输出设备；None = 系统默认输出
    #[serde(default)]
    pub monitor_device: Option<String>,
    /// 手机遥控端口；None 表示不开启
    #[serde(default)]
    pub remote_port: Option<u16>,
    /// 会话初始变声值；不参与启动指纹，缺省时沿用宿主当前配置。
    #[serde(default, skip_serializing)]
    #[cfg_attr(not(feature = "voice"), allow(dead_code))]
    pub voice: Option<VoiceStartConfig>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusDto {
    pub running: bool,
    pub looping: bool,
    pub device: String,
    pub input_device: String,
    pub slots: Vec<SlotDto>,
    pub playing: bool,
    pub active_slot: i32,
    pub playhead_secs: f32,
    pub preview_sound_id: Option<String>,
    pub preview_playing: bool,
    pub preview_playhead_secs: f32,
    pub preview_duration_secs: f32,
    pub ring_ms: f32,
    pub underruns: u64,
    pub overruns: u64,
    pub limiter_db: f32,
    /// 送往虚拟麦克风的峰值电平（dBFS）
    pub out_peak_db: f32,
    /// 送往虚拟麦克风的短时 RMS（dBFS）
    pub out_rms_db: f32,
    pub hotkeys_active: bool,
    /// 人声效果热键的一次性提示；界面按代次只展示一次。
    pub voice_notice: Option<String>,
    pub voice_notice_generation: u64,
    pub last_error: Option<String>,
    pub uptime_secs: f32,
    pub remote_urls: Vec<String>,
    /// 本次运行中监听实际触发的次数
    pub monitor_plays: u64,
    /// 本机监听故障独立于主输出，持续到该会话结束。
    pub monitor_error: Option<String>,
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
    /// `running=false, passthrough=true` = **待机**：对方仍听得到你说话，但音效不响应。
    pub passthrough: bool,
    /// 变声状态；未编译变声后端时为 `None`。
    pub voice: Option<VoiceStatusDto>,
}

impl StatusDto {
    pub fn stopped() -> Self {
        Self {
            running: false,
            looping: false,
            device: String::new(),
            input_device: String::new(),
            slots: Vec::new(),
            playing: false,
            active_slot: -1,
            playhead_secs: 0.0,
            preview_sound_id: None,
            preview_playing: false,
            preview_playhead_secs: 0.0,
            preview_duration_secs: 0.0,
            ring_ms: 0.0,
            underruns: 0,
            overruns: 0,
            limiter_db: 0.0,
            out_peak_db: -100.0,
            out_rms_db: -100.0,
            hotkeys_active: false,
            voice_notice: None,
            voice_notice_generation: 0,
            last_error: None,
            uptime_secs: 0.0,
            remote_urls: Vec::new(),
            monitor_plays: 0,
            monitor_error: None,
            skipped: Vec::new(),
            passthrough: false,
            device_lost: None,
            last_panic: None,
            audio_stalled: None,
            voice: None,
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

// ─────────────────────────────────────────── 配置写入

/// 完整配置保存与变声持久化共用这一把进程内门，防止两者各自读到旧文件后
/// 用整份快照互相覆盖。持门期间只做配置读写，不等待音频或界面线程。
///
/// 持门写入一律先用 `Config::load_for_write_from` 严格读取：无效 TOML、读取失败或
/// 含被拒绝的变声条目时放弃写入并把错误交给界面，磁盘上的文件保持原样。
static CONFIG_WRITE_GATE: Mutex<()> = Mutex::new(());

pub(crate) fn with_config_write<T, E>(
    action: impl FnOnce() -> std::result::Result<T, E>,
) -> std::result::Result<T, E> {
    let _guard = CONFIG_WRITE_GATE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    action()
}

// ─────────────────────────────────────────── 宿主

struct Running {
    generation: u64,
    session: Session,
    playback: Arc<Playback>,
    stats: Arc<EngineStats>,
    slots_meta: Vec<SlotDto>,
    out_device: String,
    in_device: String,
    output_id: DeviceId,
    input_id: DeviceId,
    monitor_error: Mutex<Option<String>>,
    started_at: std::time::Instant,
    skipped_notes: Vec<String>,
    cfg_key: String,
    cfg: StartConfig,
}

/// All requested settings participate; changed settings must never activate stale state.
pub fn cfg_fingerprint(cfg: &StartConfig) -> String {
    serde_json::to_string(cfg).expect("StartConfig is serializable")
}

fn missing_device_ids(input: &DeviceId, output: &DeviceId, available: &[DeviceId]) -> (bool, bool) {
    (!available.contains(input), !available.contains(output))
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

/// panic 日志文件路径。
///
/// - **Windows**：`%LOCALAPPDATA%\麦克风混音助手\logs\panic.log`（Windows 惯例）
/// - **macOS / Linux**：`~/Library/Logs/麦克风混音助手/panic.log`（macOS 惯例）
///
/// 拆成"平台 + 环境变量注入"的纯函数，是为了**让 Windows 分支能在 macOS 上被测到** ——
/// 交叉编译只证明能编译，证不了算得对。
pub fn panic_log_path() -> Option<std::path::PathBuf> {
    panic_log_path_on(app_config::Platform::current(), &|k| std::env::var_os(k))
}

fn panic_log_path_on(
    p: app_config::Platform,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    use std::ffi::OsString;
    let get = |k: &str| -> Option<std::path::PathBuf> {
        env(k).filter(|s: &OsString| !s.is_empty()).map(Into::into)
    };
    match p {
        app_config::Platform::Windows => Some(
            get("LOCALAPPDATA")?
                .join("麦克风混音助手")
                .join("logs")
                .join("panic.log"),
        ),
        app_config::Platform::Unix => Some(
            get("HOME")?
                .join("Library/Logs/麦克风混音助手")
                .join("panic.log"),
        ),
    }
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

/// 校准暂停的租约时长。界面的完整校准约 8 秒；租约只是界面失联时的兜底，
/// 正常结束由 `voice_calibration_end` 解除。
#[cfg(feature = "voice")]
const CALIBRATION_VOICE_LEASE: Duration = Duration::from_secs(30);

/// 人声热键提示按代次展示一次，避免 250 ms 轮询反复弹出。
#[derive(Default)]
pub(crate) struct VoiceNoticeState {
    pub(crate) generation: u64,
    pub(crate) message: Option<String>,
}

fn set_voice_notice(state: &Mutex<VoiceNoticeState>, message: impl Into<String>) {
    let mut notice = state.lock().unwrap_or_else(|e| e.into_inner());
    notice.generation = notice.generation.wrapping_add(1);
    notice.message = Some(message.into());
}

fn clear_voice_notice(state: &Mutex<VoiceNoticeState>) {
    let mut notice = state.lock().unwrap_or_else(|e| e.into_inner());
    if notice.message.take().is_some() {
        notice.generation = notice.generation.wrapping_add(1);
    }
}

type SlotAction = Arc<dyn Fn(usize) -> bool + Send + Sync>;

#[derive(Default)]
pub struct AudioHost {
    quitting: AtomicBool,
    shutdown_done: AtomicBool,
    /// Serializes the full lifecycle, including device opens and self-test suspension.
    lifecycle: Mutex<()>,
    app: Mutex<Option<tauri::AppHandle>>,
    running: Mutex<Option<Running>>,
    /// 会话之外也保存音量，试听与启动中的用户调整共用这一份状态。
    gains: Mutex<HostGains>,
    next_generation: AtomicU64,
    last_error: Mutex<Option<String>>,
    /// 人声热键的可见提示；独立于启动错误，界面按代次只展示一次。
    voice_notice: Arc<Mutex<VoiceNoticeState>>,
    /// 进程内唯一的平台热键管理器；音效与变声热键都用它绑定。
    hotkey_manager: Arc<Mutex<Option<Arc<HotkeyManager>>>>,
    /// 槽位随会话、变声随进程；由同一个轮询线程排水。
    hotkeys: Arc<Mutex<HotkeyGroups>>,
    /// 当前会话的槽位动作；待机或释放后清空，防止旧会话继续响应。
    slot_action: Arc<Mutex<Option<SlotAction>>>,
    hotkey_shutdown: Arc<AtomicBool>,
    hotkey_thread: Mutex<Option<JoinHandle<()>>>,
    /// 试听代号：每次试听自增，旧的那次看到代号变了就自行停止。
    /// 用它避免连点多个音效时声音叠在一起。
    preview_gen: Arc<AtomicU64>,
    preview_thread: Arc<Mutex<Option<JoinHandle<()>>>>,
    preview_control: Arc<Mutex<Option<PreviewControl>>>,
    /// Reserved before dispatching blocking commands. A later user action also
    /// retires older work that has not reached the host or finished decoding.
    audio_requests: Arc<AudioRequestState>,
    /// 手机遥控的共享状态（跨音频重启存活）
    remote: Mutex<Option<Arc<remote::Shared>>>,
    remote_server: Mutex<Option<remote::Server>>,
    remote_urls: Mutex<Vec<String>>,
    /// 设备掉线提示（热插拔检测到设备不见了时写这里，前端据此报警）
    device_lost: Mutex<Option<String>>,
    /// 音频卡死提示（渲染回调不再被调用，或回调里 panic 过）
    audio_stalled: Mutex<Option<String>>,
    /// 程序启动时的系统默认输入设备 —— 「切回真实麦克风」要还原成它。
    ///
    /// 只在**第一次启动音频之前**记录：之后我们再改默认设备也不会污染这个基准。
    original_default_input: Mutex<Option<(u32, String)>>,
    /// 变声状态与持久化；命令和状态轮询都从这里取快照。
    #[cfg(feature = "voice")]
    voice: VoiceService,
    /// 当前会话的控制句柄；待机保留，释放时丢弃。
    #[cfg(feature = "voice")]
    voice_control: Mutex<Option<VoiceControl>>,
    /// 试听准备/录制/回放的所有者线程；取消时由控制线程收尾。
    #[cfg(feature = "voice")]
    voice_preview_thread: Mutex<Option<JoinHandle<()>>>,
    /// 回归测试注入试听回放实现，避免测试依赖真实输出设备。
    #[cfg(all(test, feature = "voice"))]
    voice_preview_player: Mutex<Option<VoicePreviewPlayer>>,
}

#[cfg(all(test, feature = "voice"))]
type VoicePreviewPlayer =
    Arc<dyn Fn(VoicePreviewClip, u64, Arc<AtomicU64>) -> Result<()> + Send + Sync>;

/// Only request publication and session registration use this short gate. It is
/// never retained while decoding, opening / closing streams or waiting on audio.
#[derive(Default)]
struct AudioRequestState {
    gate: Mutex<()>,
    current: AtomicU64,
    generation: AtomicU64,
    gain_revision: AtomicU64,
}

impl AudioRequestState {
    fn reserve_with_gains(&self, gains: &Mutex<HostGains>) -> u64 {
        let _publication = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        let gains = gains.lock().unwrap_or_else(|e| e.into_inner());
        self.gain_revision.store(gains.revision, Ordering::Release);
        self.current.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn reserve(&self) -> u64 {
        let _publication = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        self.current.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn reserve_for_session(&self, playback: &Playback, generation: u64) -> Option<u64> {
        let _publication = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if self.generation.load(Ordering::Acquire) != generation || !playback.active() {
            return None;
        }
        Some(self.current.fetch_add(1, Ordering::AcqRel) + 1)
    }

    /// 只校验会话代次，不要求播放活跃；待机停止仍需取消本机试听。
    fn reserve_for_generation(&self, generation: u64) -> Option<u64> {
        let _publication = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if self.generation.load(Ordering::Acquire) != generation {
            return None;
        }
        Some(self.current.fetch_add(1, Ordering::AcqRel) + 1)
    }

    fn register_session(&self, generation: u64) {
        let _publication = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        self.generation.store(generation, Ordering::Release);
    }
}

struct HostGains {
    revision: u64,
    mic_db: f32,
    sfx_db: f32,
    sfx_linear: Arc<AtomicF32>,
}

impl Default for HostGains {
    fn default() -> Self {
        Self {
            revision: 0,
            mic_db: 0.0,
            sfx_db: -6.0,
            sfx_linear: Arc::new(AtomicF32::new(10.0f32.powf(-6.0 / 20.0))),
        }
    }
}

struct PreviewControl {
    token: u64,
    sound_id: String,
    audio: Arc<PreviewAudioState>,
    session_generation: u64,
}

struct PreviewAudioState {
    gain: AtomicF32,
    total_gain: Arc<AtomicF32>,
    drained_frames: AtomicU64,
    rendered_frames: AtomicU64,
    total_frames: AtomicU64,
    sample_rate: AtomicU32,
    finished: AtomicBool,
    previous_playback: Option<Arc<Playback>>,
    #[cfg(test)]
    owner_observed_source_end: Mutex<Option<mpsc::Sender<()>>>,
}

fn sound_gain(db: f32) -> Result<f32> {
    anyhow::ensure!(db.is_finite(), "音量必须是有限数值");
    anyhow::ensure!(db <= 36.0, "音量不能超过 +36 dB");
    let gain = if db <= -60.0 {
        0.0
    } else {
        10f32.powf(db / 20.0)
    };
    anyhow::ensure!(gain.is_finite(), "音量超过可支持的范围");
    Ok(gain)
}

impl AudioHost {
    pub fn reserve_audio_request(&self) -> u64 {
        self.audio_requests.reserve_with_gains(&self.gains)
    }

    pub fn audio_request_is_current(&self, ticket: u64) -> bool {
        self.audio_requests.current.load(Ordering::Acquire) == ticket && !self.is_quitting()
    }

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
                id: d.id.to_string(),
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
        let (preview_sound_id, preview_playing, preview_playhead_secs, preview_duration_secs) =
            self.preview_status();
        let (voice_notice_generation, voice_notice) = {
            let notice = self.voice_notice.lock().unwrap_or_else(|e| e.into_inner());
            (notice.generation, notice.message.clone())
        };
        // 变声状态来自宿主的控制侧快照，不读取音频线程内部状态。
        #[cfg(feature = "voice")]
        let voice = Some(self.voice.status());
        #[cfg(not(feature = "voice"))]
        let voice: Option<VoiceStatusDto> = None;
        let guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let Some(r) = guard.as_ref() else {
            let mut s = StatusDto::stopped();
            s.preview_sound_id = preview_sound_id;
            s.preview_playing = preview_playing;
            s.preview_playhead_secs = preview_playhead_secs;
            s.preview_duration_secs = preview_duration_secs;
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
            s.voice_notice = voice_notice;
            s.voice_notice_generation = voice_notice_generation;
            s.voice = voice;
            return s;
        };
        let st = r.stats.snapshot();
        let sr = r.session.output_format.sample_rate as f32;
        let mut monitor_error = r.monitor_error.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(error) = r.session.monitor_health.take_error() {
            *monitor_error = Some(error);
        } else if monitor_error.is_none() && r.session.monitor_health.panicked() {
            *monitor_error = Some("监听音频回调发生异常，请重新开启监听".into());
        }
        StatusDto {
            // running = **音效是否开启**，不等于"引擎是否存在"。
            // 待机时引擎照跑（麦克风在转发），但 running 必须是 false。
            running: r.playback.active(),
            looping: r.cfg.looping,
            device: r.out_device.clone(),
            input_device: r.in_device.clone(),
            slots: r.slots_meta.clone(),
            playing: st.playing,
            active_slot: st.active_slot,
            playhead_secs: st.playhead as f32 / sr,
            preview_sound_id,
            preview_playing,
            preview_playhead_secs,
            preview_duration_secs,
            ring_ms: st.ring_fill as f32 / 2.0 / (sr / 1000.0),
            underruns: st.underruns,
            overruns: st.overruns,
            limiter_db: st.limiter_gain_db,
            out_peak_db: st.out_peak_db,
            out_rms_db: st.out_rms_db,
            hotkeys_active: self
                .hotkeys
                .lock()
                .map(|groups| groups.slots_bound())
                .unwrap_or(false),
            voice_notice,
            voice_notice_generation,
            last_error: self
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            uptime_secs: r.started_at.elapsed().as_secs_f32(),
            remote_urls: self
                .remote_urls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            monitor_plays: r.playback.monitor_plays.load(Ordering::Relaxed),
            monitor_error: monitor_error.clone(),
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
            voice,
        }
    }

    fn preview_status(&self) -> (Option<String>, bool, f32, f32) {
        let guard = self
            .preview_control
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(control) = guard.as_ref().filter(|c| {
            c.token == self.preview_gen.load(Ordering::Acquire)
                && !c.audio.finished.load(Ordering::Acquire)
        }) else {
            return (None, false, 0.0, 0.0);
        };
        let total = control.audio.total_frames.load(Ordering::Acquire);
        let rendered = control
            .audio
            .rendered_frames
            .load(Ordering::Acquire)
            .min(total);
        let rate = control.audio.sample_rate.load(Ordering::Acquire).max(1) as f32;
        (
            Some(control.sound_id.clone()),
            rendered > 0 && rendered < total,
            rendered as f32 / rate,
            total as f32 / rate,
        )
    }

    /// 启动音频。
    ///
    /// `app` 用于把**热键绑定调度回主线程** —— macOS 上 Carbon 热键的管理器
    /// 必须在主线程创建，否则事件永远不会派发（而且不报错，表现为"按键没反应"）。
    /// `--smoke` 等无 GUI 场景传 `None`。
    pub fn start(&self, cfg: StartConfig, app: Option<&tauri::AppHandle>) -> Result<()> {
        self.start_requested(cfg, app, self.reserve_audio_request())
    }

    pub fn start_requested(
        &self,
        cfg: StartConfig,
        app: Option<&tauri::AppHandle>,
        ticket: u64,
    ) -> Result<()> {
        let _operation = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if !self.audio_request_is_current(ticket) {
            return Ok(());
        }
        if let Some(app) = app {
            *self.app.lock().unwrap_or_else(|e| e.into_inner()) = Some(app.clone());
        }
        let result = self.start_inner_requested(cfg, app, Some(ticket));
        if !self.audio_request_is_current(ticket) {
            // Publication has already checked the ticket atomically. A committed
            // session belongs to the host; a newer phone / hotkey action may now
            // be playing it. Never release that owner on this older return path.
            return Ok(());
        }
        result
    }

    fn start_inner(&self, cfg: StartConfig, app: Option<&tauri::AppHandle>) -> Result<()> {
        self.start_inner_requested(cfg, app, None)
    }

    fn start_inner_requested(
        &self,
        mut cfg: StartConfig,
        app: Option<&tauri::AppHandle>,
        ticket: Option<u64>,
    ) -> Result<()> {
        anyhow::ensure!(!self.quitting.load(Ordering::Acquire), "程序正在退出");
        if !self.prepare_start_gains(&mut cfg, ticket)? {
            return Ok(());
        }
        // StartConfig 只提供本次会话的初始值；之后的重建都使用宿主最新状态。
        #[cfg(feature = "voice")]
        let voice_block_ms = match cfg.voice.take() {
            Some(start) => self.voice.apply_start_config(&start)?,
            None => DEFAULT_VOICE_BLOCK_MS,
        };
        // 设备与音效列表都没变（典型情况：从「待机」点「开启音效」）→
        // **不重开音频流**，只把音效重新打开。瞬间生效，用户来回切毫无卡顿。
        if let Some(k) = self.running_cfg_key() {
            if k == cfg_fingerprint(&cfg) {
                return self.activate_inner(app);
            }
            println!("[启动] 设备/音效列表有变化 → 彻底重启音频");
        }
        // ⚠️ 必须用 release 而不是 stop：stop 只进待机（引擎照跑），
        //    紧接着覆盖 `running` 会让**旧引擎永远活着** —— 线程和缓冲都收不回来。
        //    这个泄漏被"停止=彻底关闭"的旧语义掩盖了很久，改待机后才暴露出来。
        self.release_inner();
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // 上一次“音频未启动”之类的提示在新一轮启动开始时失效；
        // 本轮启动后段记录的冲突/绑定提示不能被发布动作清掉。
        self.dismiss_voice_notice();

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
            Some(n) => {
                match audio_session::resolve_device(&all, n, audio_core::device::Direction::Input)?
                {
                    Some(id) => id,
                    None => {
                        let d = b.default_input().context("系统没有默认输入设备")?;
                        println!("[设备] ⚠ 配置里的麦克风「{n}」已不在 → 改用系统默认「{d}」");
                        subst.push(format!("麦克风「{n}」已不在，改用系统默认"));
                        d
                    }
                }
            }
            None => b.default_input().context("系统没有默认输入设备")?,
        };

        let out_id = match cfg.output_device.as_deref().filter(|n| !n.is_empty()) {
            Some(n) => {
                match audio_session::resolve_device(&all, n, audio_core::device::Direction::Output)?
                {
                    Some(id) => id,
                    None => {
                        let d = b.guess_virtual_output().context(format!(
                            "配置里的输出设备「{n}」已不在，而且也没探测到别的虚拟声卡"
                        ))?;
                        println!("[设备] ⚠ 配置里的输出「{n}」已不在 → 改用「{d}」");
                        subst.push(format!("输出设备「{n}」已不在，改用「{d}」"));
                        d
                    }
                }
            }
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
            let target = match cfg.monitor_device.as_deref().filter(|s| !s.is_empty()) {
                Some(selector) => audio_session::resolve_device(
                    &all,
                    selector,
                    audio_core::device::Direction::Output,
                )?,
                None => b.default_output(),
            };
            match target {
                Some(d) => match b.output_format(&d, Some(StreamFormat::new(48_000, 2))) {
                    Ok(f) => {
                        println!("[监听] 输出设备 = {d}  {f}");
                        if d == out_id {
                            log::warn!("监听设备与主输出相同，已停用监听以免重复打开端点");
                            None
                        } else {
                            Some((d.to_string(), f))
                        }
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

        let specs: Vec<_> = files
            .iter()
            .enumerate()
            .map(|(i, file)| SoundSpec {
                id: cfg
                    .ids
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| file.display().to_string()),
                file: file.clone(),
                name: cfg.names.get(i).cloned().unwrap_or_default(),
                gain_db: cfg.gains.get(i).copied().unwrap_or(0.0),
                hotkey: cfg.hotkeys.get(i).cloned().unwrap_or_default(),
                tags: cfg.tags.get(i).cloned().unwrap_or_default(),
            })
            .collect();
        let library = Library::load(&specs, out_fmt.sample_rate, cfg.looping, true);
        if library.slots.is_empty() && !files.is_empty() {
            anyhow::bail!(
                "这 {} 个音效文件全都无法解码。第一个失败原因：{}",
                files.len(),
                library
                    .skipped
                    .first()
                    .map(|(_, r)| r.as_str())
                    .unwrap_or("未知")
            );
        }
        let slots_meta: Vec<_> = library
            .meta
            .iter()
            .enumerate()
            .map(|(index, m)| SlotDto {
                index,
                source_index: m.source_index,
                id: m.spec.id.clone(),
                name: m.spec.name.clone(),
                file: m.spec.file.display().to_string(),
                duration: m.duration_secs,
                peak_db: m.peak_db,
                hotkey: m.spec.hotkey.clone(),
                gain_db: m.spec.gain_db,
            })
            .collect();
        let monitor = monitor_out.map(|(device, format)| MonitorOptions {
            device: DeviceId::new(device),
            format,
            slots: library.monitor_slots(out_fmt.sample_rate, format.sample_rate),
        });
        let skipped_notes = library
            .skipped
            .iter()
            .map(|(n, r)| format!("{n}（{r}）"))
            .collect();
        let out_name = all
            .iter()
            .find(|d| d.id == out_id && d.direction == audio_core::device::Direction::Output)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| out_id.to_string());
        let in_name = all
            .iter()
            .find(|d| d.id == in_id && d.direction == audio_core::device::Direction::Input)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| in_id.to_string());
        #[cfg(feature = "voice")]
        let voice_options = Some(self.voice.prepare_options(voice_block_ms)?);
        #[cfg(not(feature = "voice"))]
        let voice_options: Option<audio_session::VoiceOptions> = None;
        let session = Session::open(SessionOptions {
            input: in_id.clone(),
            output: out_id.clone(),
            input_format: in_fmt,
            output_format: out_fmt,
            buffer_ms: cfg.buffer_ms,
            slots: library.slots,
            mic_gain_db: cfg.mic_gain_db,
            sfx_gain_db: cfg.sfx_gain_db,
            monitor,
            voice: voice_options,
        })?;
        #[cfg(feature = "voice")]
        let voice_control = session.voice_control();
        #[cfg(feature = "voice")]
        let voice_preview_control = session.voice_preview_control();
        let playback = session.playback.clone();
        let control = playback.control.clone();
        let stats = session.stats.clone();
        let slot_specs: Vec<_> = slots_meta.iter().map(|s| s.hotkey.clone()).collect();
        #[cfg(feature = "voice")]
        let voice_specs = self.voice.hotkey_specs();
        #[cfg(not(feature = "voice"))]
        let voice_specs: Vec<String> = Vec::new();
        let has_slot_keys = slot_specs.iter().any(|spec| !spec.trim().is_empty());
        let has_voice_keys = voice_specs.iter().any(|spec| !spec.trim().is_empty());
        let voice_bound = self
            .hotkeys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .voice_bound();
        let disjoint_ok = if has_slot_keys || has_voice_keys {
            match hotkeys::validate_disjoint(&[&slot_specs, &voice_specs]) {
                Ok(()) => true,
                Err(error) => {
                    // 冲突不能阻止音频启动：变声热键常驻时让槽位退让，否则槽位优先。
                    self.record_voice_notice(format!("热键冲突，未绑定：{error:#}"));
                    false
                }
            }
        } else {
            true
        };
        let bind_slots = has_slot_keys && (disjoint_ok || !voice_bound);
        let slot_hotkeys = if bind_slots {
            match self.bind_hotkeys_on_main_thread(app, &slot_specs) {
                Ok((hotkeys, how)) => {
                    println!("[热键] 绑定成功（{how}）");
                    Some(hotkeys)
                }
                Err(error) => {
                    println!("[热键] ❌ 绑定失败: {error:#}");
                    None
                }
            }
        } else {
            None
        };
        self.install_slot_hotkeys(slot_hotkeys)?;
        #[cfg(feature = "voice")]
        if has_voice_keys && !voice_bound && disjoint_ok {
            // 启动时通常已常驻绑定；这里只覆盖首次失败后的重试，不重复注册。
            if let Err(error) = self.ensure_voice_hotkeys(app) {
                self.record_voice_notice(format!("人声效果热键绑定失败：{error:#}"));
            }
        }
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
        *self.slot_action.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(self.play_handler(playback.clone(), generation));
        self.ensure_hotkey_thread();

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
                        // 遥控路径同理：三条触发路径都要监听
                        sh.set_routed(
                            control,
                            slots_meta.iter().map(|s| s.name.clone()).collect(),
                            self.play_handler(playback.clone(), generation),
                            self.remote_stop_handler(playback.clone(), generation),
                        );
                        #[cfg(feature = "voice")]
                        sh.set_voice_route(self.voice.remote_route());
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
                *self.remote_server.lock().unwrap_or_else(|e| e.into_inner()) = None;
                self.remote_urls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clear();
                if let Some(sh) = self
                    .remote
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    #[cfg(feature = "voice")]
                    self.voice.retire_remote_route();
                    sh.clear(); // 关掉开关后，手机端会显示"音频未启动"
                }
            }
        }

        let running = Running {
            generation,
            session,
            playback,
            stats,
            slots_meta,
            out_device: out_name,
            in_device: in_name,
            output_id: out_id,
            input_id: in_id,
            monitor_error: Mutex::new(None),
            started_at: std::time::Instant::now(),
            skipped_notes,
            cfg_key: cfg_fingerprint(&cfg),
            cfg,
        };
        let unpublished = self.publish_running(running, ticket);
        #[cfg(feature = "voice")]
        if unpublished.is_none() {
            // 只把已发布的会话句柄交给命令层；被拒绝的会话随后直接关闭。
            self.attach_voice_control(voice_control);
            self.attach_voice_preview(voice_preview_control);
        }
        if let Some(unpublished) = unpublished {
            self.discard_unpublished_session(unpublished);
        }
        Ok(())
    }

    /// 被取代的启动从未发布，但它的槽位热键已装入宿主级集合，必须一并归还。
    ///
    /// 调用方持有生命周期锁，更新会话不可能在此时完成发布，因此不会误清
    /// 已提交会话的热键。
    fn discard_unpublished_session(&self, running: Running) {
        if let Some(shared) = self
            .remote
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            #[cfg(feature = "voice")]
            self.voice.retire_remote_route();
            shared.clear();
        }
        self.end_session_hotkeys();
        self.close_running(running);
    }

    /// Commit ownership and its routable generation together. No reservation can
    /// pass between the last ticket check and handing the new session to the host.
    fn publish_running(&self, mut running: Running, ticket: Option<u64>) -> Option<Running> {
        let _publication = self
            .audio_requests
            .gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if ticket.is_some_and(|t| !self.audio_request_is_current(t)) {
            return Some(running);
        }
        // 开流期间的音量调整不能被启动参数的旧副本覆盖。
        let gains = self.gains.lock().unwrap_or_else(|e| e.into_inner());
        running.playback.set_gains(gains.mic_db, gains.sfx_db);
        running.cfg.mic_gain_db = gains.mic_db;
        running.cfg.sfx_gain_db = gains.sfx_db;
        running.cfg_key = cfg_fingerprint(&running.cfg);
        let generation = running.generation;
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = Some(running);
        self.audio_requests
            .generation
            .store(generation, Ordering::Release);
        None
    }

    /// 启动手机遥控（幂等）。返回可访问的地址列表。
    ///
    /// 服务**只在第一次真正起监听**，之后跨音频重启一直存活 ——
    /// 否则手机上刷新一下就再也连不上了。
    pub fn start_remote(&self, port: u16, bind: String) -> Result<Vec<String>> {
        let mut g = self.remote.lock().unwrap_or_else(|e| e.into_inner());
        let mut server = self.remote_server.lock().unwrap_or_else(|e| e.into_inner());
        if server.as_ref().is_some_and(|s| s.port == port) {
            return Ok(self
                .remote_urls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone());
        }
        if let Some(shared) = g.as_ref() {
            #[cfg(feature = "voice")]
            self.voice.retire_remote_route();
            shared.clear();
        }
        *server = None;
        let shared = g
            .clone()
            .unwrap_or_else(|| Arc::new(remote::Shared::default()));
        let listener = remote::Server::start(shared.clone(), remote::RemoteConfig { port, bind })?;
        let urls = listener.urls.clone();
        *server = Some(listener);
        *g = Some(shared);
        *self.remote_urls.lock().unwrap_or_else(|e| e.into_inner()) = urls.clone();
        Ok(urls)
    }

    /// 停止音效 —— **但继续转发麦克风**。
    ///
    /// 为什么不整个停掉：用户大多数时候不用音效，而游戏的麦克风**一直**指着虚拟声卡。
    /// 一旦连转发也断了，虚拟声卡里就是一片死寂（实测精确为 0），**对方会突然听不到他说话**。
    /// 所以「停止」的语义是「关掉音效」，不是「把我变成哑巴」。
    ///
    /// 进入待机时只注销**槽位热键** —— 否则 F1/F2 仍被我们截走，游戏里按这两个键毫无反应。
    /// 人声效果热键继续保留，待机时仍可切换预设。
    pub fn stop(&self) {
        self.stop_requested(self.reserve_audio_request());
    }

    pub fn stop_requested(&self, ticket: u64) {
        let _operation = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        self.stop_inner_requested(Some(ticket));
    }

    fn stop_inner(&self) {
        self.stop_inner_requested(None);
    }

    fn stop_inner_requested(&self, ticket: Option<u64>) {
        if ticket.is_some_and(|t| !self.audio_request_is_current(t)) {
            return;
        }
        self.stop_preview_inner();
        let groups = {
            let _admission = self
                .preview_control
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if ticket.is_some_and(|t| !self.audio_request_is_current(t)) {
                return;
            }
            let guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
            let Some(r) = guard.as_ref() else {
                return;
            };
            r.playback.standby();
            self.hotkeys.clone()
        };
        *self.slot_action.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // 待机只归还音效槽位按键；变声热键继续可用。
        if let Err(error) = self.with_hotkey_groups(groups, |groups| groups.standby()) {
            self.record_error(format!("注销热键失败: {error:#}"));
        }
        if let Some(sh) = self
            .remote
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            // 待机只关闭播放入口；停止处理器与变声路由保留，手机仍能停止
            // 正在录制的人声试听、启用变声与切换预设，且不会重新启动音效。
            sh.clear_playback();
            #[cfg(feature = "voice")]
            sh.set_voice_route(self.voice.remote_route());
        }
    }

    pub fn release(&self) {
        self.release_requested(self.reserve_audio_request());
    }

    pub fn release_requested(&self, ticket: u64) {
        let _operation = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        self.release_inner_requested(Some(ticket));
    }

    fn release_inner(&self) {
        self.release_inner_requested(None);
    }

    fn release_inner_requested(&self, ticket: Option<u64>) {
        if ticket.is_some_and(|t| !self.audio_request_is_current(t)) {
            return;
        }
        #[cfg(feature = "voice")]
        self.cancel_voice_preview_inner();
        self.stop_preview_inner();
        let admission = self
            .preview_control
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if ticket.is_some_and(|t| !self.audio_request_is_current(t)) {
            return;
        }
        self.set_device_lost(None);
        self.set_audio_stalled(None);
        if let Some(sh) = self
            .remote
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            #[cfg(feature = "voice")]
            self.voice.retire_remote_route();
            sh.clear();
        }
        let running = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.audio_requests.register_session(0);
        #[cfg(feature = "voice")]
        if running.is_some() {
            // 会话释放后旧句柄不能再影响新会话，下一次启动重新构建。
            self.attach_voice_control(None);
            self.attach_voice_preview(None);
        }
        if let Some(r) = running {
            r.playback.standby();
            // A hotkey worker may be waiting for preview admission. Its captured
            // playback is inactive before we release the lock.
            drop(admission);
            self.end_session_hotkeys();
            self.close_running(r);
        } else {
            drop(admission);
            self.end_session_hotkeys();
        }
    }

    /// 会话释放只注销槽位热键；人声效果热键保留到进程退出。
    fn end_session_hotkeys(&self) {
        *self.slot_action.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Err(error) =
            self.with_hotkey_groups(self.hotkeys.clone(), |groups| groups.end_session())
        {
            self.record_error(format!("释放槽位热键失败: {error:#}"));
        }
    }

    fn close_running(&self, running: Running) {
        running.playback.standby();
        drop(running); // Session closes all streams on their owner thread.
    }

    fn with_hotkey_groups<T: Send + 'static>(
        &self,
        handle: Arc<Mutex<HotkeyGroups>>,
        action: impl FnOnce(&mut HotkeyGroups) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        if !handle.lock().unwrap_or_else(|e| e.into_inner()).has_sets() {
            return action(&mut handle.lock().unwrap_or_else(|e| e.into_inner()));
        }
        if !cfg!(target_os = "macos") || hotkeys::is_main_thread() {
            return action(&mut handle.lock().unwrap_or_else(|e| e.into_inner()));
        }
        let app = self.app.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(app) = app else {
            // 无 GUI 上下文（测试或 --smoke）：没有可调度的主线程，直接执行。
            return action(&mut handle.lock().unwrap_or_else(|e| e.into_inner()));
        };
        let (tx, rx) = mpsc::sync_channel(1);
        app.run_on_main_thread(move || {
            let result = action(&mut handle.lock().unwrap_or_else(|e| e.into_inner()));
            let _ = tx.send(result);
        })?;
        rx.recv_timeout(Duration::from_secs(6))
            .context("主线程热键操作超时")?
    }

    pub fn activate(&self, app: Option<&tauri::AppHandle>) -> Result<()> {
        self.reserve_audio_request();
        let _operation = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        self.activate_inner(app)
    }

    fn activate_inner(&self, app: Option<&tauri::AppHandle>) -> Result<()> {
        let (handle, playback, specs, names, remote_port, generation) = {
            let guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
            let r = guard.as_ref().context("音频还没启动")?;
            (
                self.hotkeys.clone(),
                r.playback.clone(),
                r.slots_meta
                    .iter()
                    .map(|s| s.hotkey.clone())
                    .collect::<Vec<_>>(),
                r.slots_meta
                    .iter()
                    .map(|s| s.name.clone())
                    .collect::<Vec<_>>(),
                r.cfg.remote_port,
                r.generation,
            )
        };
        if specs.iter().any(|s| !s.trim().is_empty()) {
            let has_slot_set = handle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .slot_hotkeys()
                .is_some();
            let result = if has_slot_set {
                self.with_hotkey_groups(handle, move |groups| groups.rebind_slots(&specs))
            } else {
                self.bind_hotkeys_on_main_thread(app, &specs)
                    .map(|(hotkeys, _)| {
                        handle
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .set_slot_hotkeys(Some(hotkeys));
                    })
            };
            if let Err(error) = result {
                self.record_error(format!("恢复热键失败: {error:#}"));
            }
        }
        *self.slot_action.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(self.play_handler(playback.clone(), generation));
        self.ensure_hotkey_thread();
        // 音频恢复后不再保留“音频未启动”等旧提示。
        self.dismiss_voice_notice();
        if remote_port.is_some() {
            if let Some(sh) = self
                .remote
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                sh.set_routed(
                    playback.control.clone(),
                    names,
                    self.play_handler(playback.clone(), generation),
                    self.remote_stop_handler(playback.clone(), generation),
                );
                #[cfg(feature = "voice")]
                sh.set_voice_route(self.voice.remote_route());
            }
        }
        playback.activate();
        Ok(())
    }

    /// Temporarily surrender devices for self-test, then restore the exact activity state.
    pub fn with_suspended<T>(
        &self,
        app: Option<&tauri::AppHandle>,
        test: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let _operation = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        // 自检期间不使用人声；只叠加暂停层，不改配置。暂停层存在时会话重建保留临时旁路。
        #[cfg(feature = "voice")]
        self.voice.suspend();
        let saved = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|r| (r.cfg.clone(), r.playback.active()));
        self.release_inner();
        let result = test();
        let restore_error = if let Some((cfg, active)) = saved {
            match self.start_inner(cfg, app) {
                Ok(()) => {
                    if !active {
                        self.stop_inner();
                    }
                    None
                }
                Err(e) => {
                    self.record_error(format!("自检后恢复音频失败: {e:#}"));
                    Some(anyhow::anyhow!("自检已结束，但恢复音频失败: {e:#}"))
                }
            }
        } else {
            None
        };
        #[cfg(feature = "voice")]
        self.voice.restore();
        if let Some(error) = restore_error {
            return Err(error);
        }
        result
    }

    /// 当前运行的启动参数指纹（用来判断「启动」能不能走快路）。
    pub fn running_cfg_key(&self) -> Option<String> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|r| r.cfg_key.clone())
    }

    /// Snapshot checks carry a generation so a delayed fault cannot replace a newer session.
    pub fn check_devices(&self) -> Option<(u64, bool, bool, String)> {
        let (generation, input_id, output_id, in_name, out_name, health) = {
            let g = self.running.lock().unwrap_or_else(|e| e.into_inner());
            let r = g.as_ref()?;
            (
                r.generation,
                r.input_id.clone(),
                r.output_id.clone(),
                r.in_device.clone(),
                r.out_device.clone(),
                r.session.health.clone(),
            )
        };
        if let Some(err) = health.take_error() {
            let is_in = err.starts_with("输入");
            let which = if is_in { in_name } else { out_name };
            println!("[热插拔] 音频流报错（{err}）→ 判定「{which}」掉线");
            return Some((generation, is_in, !is_in, which));
        }
        // Native stable IDs only: never initialize another AudioUnit/AudioClient
        // while the owner is opening or rendering a stream.
        let available = CpalBackend::available_device_ids().ok()?;
        let (in_lost, out_lost) = missing_device_ids(&input_id, &output_id, &available);
        if in_lost || out_lost {
            let which = if out_lost { out_name } else { in_name };
            Some((generation, in_lost, out_lost, which))
        } else {
            None
        }
    }

    pub fn set_device_lost(&self, msg: Option<String>) {
        *self.device_lost.lock().unwrap_or_else(|e| e.into_inner()) = msg;
    }

    pub fn set_device_lost_for(&self, generation: u64, msg: Option<String>) -> bool {
        let running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if !running.as_ref().is_some_and(|r| r.generation == generation) {
            return false;
        }
        self.set_device_lost(msg);
        true
    }

    /// Reopen the failed input using the system default, even when the saved selector was None.
    pub fn recover_input_device(
        &self,
        expected_generation: u64,
        app: Option<&tauri::AppHandle>,
    ) -> Result<Option<(u64, String)>> {
        let _operation = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let saved = {
            let g = self.running.lock().unwrap_or_else(|e| e.into_inner());
            g.as_ref()
                .filter(|r| r.generation == expected_generation)
                .map(|r| (r.cfg.clone(), r.playback.active(), r.in_device.clone()))
        };
        let Some((mut cfg, was_active, old)) = saved else {
            return Ok(None);
        };
        if self.is_quitting() {
            return Ok(None);
        }
        cfg.input_device = None;
        self.release_inner();
        if let Err(error) = self.start_inner(cfg, app) {
            self.record_error(format!("麦克风自动恢复失败: {error:#}"));
            return Err(error);
        }
        if !was_active {
            self.stop_inner();
        }
        Ok(Some((self.audio_progress().0, old)))
    }

    pub fn audio_progress(&self) -> (u64, u64, u64, bool) {
        let guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_ref()
            .map(|r| {
                (
                    r.generation,
                    r.stats.rendered_frames.load(Ordering::Relaxed),
                    r.stats.captured_frames.load(Ordering::Relaxed),
                    r.session.health.panicked(),
                )
            })
            .unwrap_or((0, 0, 0, false))
    }

    /// Smoke fault injection targets the actual session, never a process-global flag.
    pub fn session_health(&self) -> Option<Arc<audio_core::device::StreamHealth>> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|r| r.session.health.clone())
    }

    pub fn begin_shutdown(&self) -> bool {
        {
            let first = !self.quitting.swap(true, Ordering::AcqRel);
            self.reserve_audio_request();
            self.preview_gen.fetch_add(1, Ordering::AcqRel);
            first
        }
    }
    pub fn is_quitting(&self) -> bool {
        self.quitting.load(Ordering::Acquire)
    }
    pub fn finish_shutdown(&self) {
        self.shutdown_done.store(true, Ordering::Release);
    }
    pub fn shutdown_done(&self) -> bool {
        self.shutdown_done.load(Ordering::Acquire)
    }

    pub fn set_audio_stalled(&self, msg: Option<String>) {
        *self.audio_stalled.lock().unwrap_or_else(|e| e.into_inner()) = msg;
    }

    pub fn set_audio_stalled_for(&self, generation: u64, msg: Option<String>) -> bool {
        let running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if !running.as_ref().is_some_and(|r| r.generation == generation) {
            return false;
        }
        self.set_audio_stalled(msg);
        true
    }

    /// Restart only the session that produced the watchdog snapshot.
    pub fn restart_audio(
        &self,
        expected_generation: u64,
        app: Option<&tauri::AppHandle>,
    ) -> Result<Option<u64>> {
        let _operation = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let saved = {
            let g = self.running.lock().unwrap_or_else(|e| e.into_inner());
            g.as_ref()
                .filter(|r| r.generation == expected_generation)
                .map(|r| (r.cfg.clone(), r.playback.active()))
        };
        let Some((cfg, was_active)) = saved else {
            return Ok(None);
        };
        if self.is_quitting() {
            return Ok(None);
        }
        self.release_inner();
        if let Err(error) = self.start_inner(cfg, app) {
            self.record_error(format!("音频自动恢复失败: {error:#}"));
            return Err(error);
        }
        if !was_active {
            self.stop_inner();
        }
        Ok(Some(self.audio_progress().0))
    }

    /// 切回真实麦克风。
    ///
    /// 返回 `(是否真的切成功, 切到了哪个设备, 当前实际默认设备)`。
    /// **必须回读确认** —— 有些虚拟声卡驱动的 set 会返回成功但根本不生效。
    #[cfg(target_os = "macos")]
    pub fn revert_to_real_mic(&self) -> Result<(bool, String, String)> {
        let _operation = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        let original = self.original_default_input();
        let target = original
            .as_ref()
            .filter(|(_, name)| !audio_core::device::is_virtual_name(name))
            .and_then(|(_, name)| crate::macos_audio::find_input_by_name(name))
            .or_else(crate::macos_audio::first_real_input)
            .context("找不到真实麦克风设备 —— 请检查系统声音设置")?;

        let (id, name) = target;
        self.release_inner();
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
        {
            let running = self.running.lock().unwrap_or_else(|e| e.into_inner());
            let r = running.as_ref().context("音频还没启动")?;
            if !r.playback.active() || slot >= r.playback.control.slot_count() {
                return Ok(false);
            }
        }
        self.play_requested(slot, self.reserve_audio_request())
    }

    pub fn play_requested(&self, slot: usize, ticket: u64) -> Result<bool> {
        if !self.audio_request_is_current(ticket) {
            return Ok(false);
        }
        let guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let Some(r) = guard.as_ref() else {
            anyhow::bail!("音频还没启动");
        };
        let playback = r.playback.clone();
        let generation = r.generation;
        drop(guard);
        Ok(Self::play_routed(
            &playback,
            &self.preview_gen,
            &self.preview_control,
            &self.preview_thread,
            (&self.audio_requests, Some(ticket)),
            generation,
            slot,
        ))
    }

    fn play_handler(
        &self,
        playback: Arc<Playback>,
        generation: u64,
    ) -> Arc<dyn Fn(usize) -> bool + Send + Sync> {
        let preview_generation = self.preview_gen.clone();
        let controls = self.preview_control.clone();
        let preview_thread = self.preview_thread.clone();
        let requests = self.audio_requests.clone();
        Arc::new(move |slot| {
            if slot >= playback.control.slot_count() {
                return false;
            }
            let Some(ticket) = requests.reserve_for_session(&playback, generation) else {
                return false;
            };
            Self::play_routed(
                &playback,
                &preview_generation,
                &controls,
                &preview_thread,
                (&requests, Some(ticket)),
                generation,
                slot,
            )
        })
    }

    fn play_routed(
        playback: &Playback,
        preview_generation: &AtomicU64,
        controls: &Mutex<Option<PreviewControl>>,
        preview_thread: &Mutex<Option<JoinHandle<()>>>,
        request: (&AudioRequestState, Option<u64>),
        generation: u64,
        slot: usize,
    ) -> bool {
        let (requests, requested_ticket) = request;
        let mut control = controls.lock().unwrap_or_else(|e| e.into_inner());
        if !playback.active() || slot >= playback.control.slot_count() {
            return false;
        }
        if control
            .as_ref()
            .is_some_and(|c| c.session_generation != generation)
        {
            // An old handler cannot interrupt a preview belonging to a new session.
            return false;
        }
        // Captured phone / hotkey callbacks first validate their session. A
        // retired callback must not invalidate a newer session's user request.
        let ticket = requested_ticket.unwrap_or_else(|| requests.reserve());
        if requests.current.load(Ordering::Acquire) != ticket {
            return false;
        }
        playback.play_with_checked_barrier(slot, || {
            if control.is_some() {
                preview_generation.fetch_add(1, Ordering::AcqRel);
                *control = None;
            }
            // The owner closes the old output stream before the new command
            // becomes visible to either renderer. Its completion takes no locks.
            let previous = preview_thread
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(previous) = previous {
                let _ = previous.join();
            }
            // Stream closure may block. A later stop / replacement reserved
            // while we joined must prevent either output from receiving Play.
            requests.current.load(Ordering::Acquire) == ticket
        })
    }

    pub fn stop_all(&self) -> Result<()> {
        self.stop_all_requested(self.reserve_audio_request())
    }

    pub fn stop_all_requested(&self, ticket: u64) -> Result<()> {
        let _operation = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if !self.audio_request_is_current(ticket) {
            return Ok(());
        }
        self.stop_preview_inner();
        #[cfg(feature = "voice")]
        self.cancel_voice_preview_inner();
        let _admission = self
            .preview_control
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !self.audio_request_is_current(ticket) {
            return Ok(());
        }
        let guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = guard.as_ref() {
            r.playback.stop_all();
        }
        Ok(())
    }

    fn remote_stop_handler(
        &self,
        playback: Arc<Playback>,
        generation: u64,
    ) -> Arc<dyn Fn() + Send + Sync> {
        let preview_generation = self.preview_gen.clone();
        let controls = self.preview_control.clone();
        let requests = self.audio_requests.clone();
        #[cfg(feature = "voice")]
        let voice = self.voice.clone();
        Arc::new(move || {
            // 手机停止也是全局停止：待机时 playback 已不活跃，但人声试听仍可用。
            // 只取消仍属于当前会话的代次，且不在监听回调里 join 所有者线程。
            #[cfg(feature = "voice")]
            if requests.generation.load(Ordering::Acquire) == generation {
                voice.preview_cancel();
            }
            // A handler may already have been cloned when its session is released.
            // Only affect a preview associated with that exact session. Avoid
            // joining under a remote listener callback (listener shutdown could
            // otherwise wait on it). Serialize stop with preview/play submission.
            // 待机时 playback 不活跃，但音效试听仍可能在本机播放，因此只按
            // 会话代次校验，不能提前返回；播放本身仍只在活跃时停止。
            let Some(ticket) = requests.reserve_for_generation(generation) else {
                return;
            };
            let mut control = controls.lock().unwrap_or_else(|e| e.into_inner());
            if requests.current.load(Ordering::Acquire) != ticket {
                return;
            }
            if control
                .as_ref()
                .is_some_and(|c| c.session_generation != generation)
            {
                return;
            }
            if control.is_some() {
                preview_generation.fetch_add(1, Ordering::AcqRel);
                *control = None;
            }
            if playback.active() {
                playback.stop_all();
            }
        })
    }

    /// 调整单个音效的音量。**下一帧生效**，不必重播。
    pub fn set_slot_gain(&self, slot: usize, db: f32) -> Result<bool> {
        sound_gain(db)?;
        let sound_id = {
            let guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .as_ref()
                .and_then(|r| r.slots_meta.get(slot))
                .map(|m| m.id.clone())
        };
        match sound_id {
            Some(id) => self.set_sound_gain(&id, db),
            None => Ok(false),
        }
    }

    /// Apply a card's gain to the same stable sound in primary, monitor and preview.
    /// Preview control exists during decode too, and updates never restart its cursor.
    pub fn set_sound_gain(&self, sound_id: &str, db: f32) -> Result<bool> {
        let gain = sound_gain(db)?;
        anyhow::ensure!(!sound_id.trim().is_empty(), "素材标识不能为空");
        let mut updated = false;
        let mut guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = guard.as_mut() {
            if let Some(slot) = r.slots_meta.iter().position(|m| m.id == sound_id) {
                if r.playback.set_slot_gain(slot, db) {
                    let meta = &mut r.slots_meta[slot];
                    meta.gain_db = db;
                    r.cfg.gains.resize(r.cfg.files.len(), 0.0);
                    r.cfg.gains[meta.source_index] = db;
                    r.cfg_key = cfg_fingerprint(&r.cfg);
                    updated = true;
                }
            }
        }
        drop(guard);
        let control = self
            .preview_control
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(control) = control.as_ref().filter(|c| {
            c.sound_id == sound_id
                && c.token == self.preview_gen.load(Ordering::Acquire)
                && !c.audio.finished.load(Ordering::Acquire)
        }) {
            control.audio.gain.set(gain);
            updated = true;
        }
        Ok(updated)
    }

    /// 启动票据之后更新的音量优先于后台命令携带的旧配置。
    fn prepare_start_gains(&self, cfg: &mut StartConfig, ticket: Option<u64>) -> Result<bool> {
        sound_gain(cfg.mic_gain_db)?;
        let sfx = sound_gain(cfg.sfx_gain_db)?;
        for db in &cfg.gains {
            sound_gain(*db)?;
        }
        let _publication = self
            .audio_requests
            .gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if ticket.is_some_and(|t| !self.audio_request_is_current(t)) {
            return Ok(false);
        }
        let mut gains = self.gains.lock().unwrap_or_else(|e| e.into_inner());
        let unchanged_since_request = ticket.is_some()
            && gains.revision == self.audio_requests.gain_revision.load(Ordering::Acquire);
        if unchanged_since_request {
            gains.revision = gains.revision.wrapping_add(1);
            gains.mic_db = cfg.mic_gain_db;
            gains.sfx_db = cfg.sfx_gain_db;
            gains.sfx_linear.set(sfx);
        } else {
            cfg.mic_gain_db = gains.mic_db;
            cfg.sfx_gain_db = gains.sfx_db;
        }
        Ok(true)
    }

    pub fn set_gains(&self, mic_db: f32, sfx_db: f32) {
        if sound_gain(mic_db).is_err() {
            return;
        }
        let Ok(sfx) = sound_gain(sfx_db) else {
            return;
        };
        let mut gains = self.gains.lock().unwrap_or_else(|e| e.into_inner());
        gains.revision = gains.revision.wrapping_add(1);
        gains.mic_db = mic_db;
        gains.sfx_db = sfx_db;
        gains.sfx_linear.set(sfx);
        let mut guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = guard.as_mut() {
            r.playback.set_gains(mic_db, sfx_db);
            r.cfg.mic_gain_db = mic_db;
            r.cfg.sfx_gain_db = sfx_db;
            r.cfg_key = cfg_fingerprint(&r.cfg);
        }
    }

    /// Change repeat mode for the current pass and future triggers without
    /// rebuilding streams or restarting the sound. Preview stays single-pass.
    pub fn set_looping(&self, looping: bool) -> bool {
        let mut guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let Some(r) = guard.as_mut() else {
            return false;
        };
        r.playback.set_looping(looping);
        r.cfg.looping = looping;
        r.cfg_key = cfg_fingerprint(&r.cfg);
        true
    }

    /// Decode on the blocking command worker; cancellation also covers the preparation phase.
    pub fn preview(&self, path: PathBuf, gain_db: f32) -> Result<()> {
        self.preview_sound(path.display().to_string(), path, gain_db)
    }

    pub fn preview_requested(&self, path: PathBuf, gain_db: f32, ticket: u64) -> Result<()> {
        self.preview_sound_requested(path.display().to_string(), path, gain_db, ticket)
    }

    pub fn preview_sound(&self, sound_id: String, path: PathBuf, gain_db: f32) -> Result<()> {
        self.preview_sound_requested(sound_id, path, gain_db, self.reserve_audio_request())
    }

    pub fn preview_sound_requested(
        &self,
        sound_id: String,
        path: PathBuf,
        gain_db: f32,
        ticket: u64,
    ) -> Result<()> {
        self.launch_preview_requested(
            sound_id,
            gain_db,
            ticket,
            move || {
                anyhow::ensure!(path.is_file(), "文件不存在: {}", path.display());
                let b = CpalBackend::new();
                let dev = b
                    .default_output()
                    .context("系统没有默认输出设备（试听需要它）")?;
                let fmt = b.output_format(&dev, Some(StreamFormat::new(48_000, 2)))?;
                let mut audio = decode_file(&path, fmt.sample_rate)
                    .with_context(|| format!("解码失败: {}", path.display()))?;
                anyhow::ensure!(
                    audio.frames() > 0,
                    "这个文件里没有音频数据: {}",
                    path.display()
                );
                audio.normalize_peak(0.708);
                Ok((Arc::new(audio.samples), dev, fmt))
            },
            move |(samples, device, format), token, generation, gain| {
                play_samples(
                    CpalBackend::new(),
                    samples,
                    device,
                    format,
                    gain,
                    token,
                    generation,
                )
            },
        )
    }

    #[cfg(test)]
    fn launch_preview<T: Send + 'static>(
        &self,
        sound_id: String,
        gain_db: f32,
        prepare: impl FnOnce() -> Result<T>,
        play: impl FnOnce(T, u64, Arc<AtomicU64>, Arc<PreviewAudioState>) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        self.launch_preview_requested(
            sound_id,
            gain_db,
            self.reserve_audio_request(),
            prepare,
            play,
        )
    }

    fn launch_preview_requested<T: Send + 'static>(
        &self,
        sound_id: String,
        gain_db: f32,
        ticket: u64,
        prepare: impl FnOnce() -> Result<T>,
        play: impl FnOnce(T, u64, Arc<AtomicU64>, Arc<PreviewAudioState>) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        if !self.audio_request_is_current(ticket) {
            return Ok(());
        }
        let gain = sound_gain(gain_db)?;
        anyhow::ensure!(!sound_id.trim().is_empty(), "素材标识不能为空");
        anyhow::ensure!(!self.is_quitting(), "程序正在退出");
        // Allocate before any device lookup/decode. A stop during prepare must
        // invalidate this request rather than let it assign a fresh token later.
        let (token, session_generation, audio) = {
            let _operation = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
            if !self.audio_request_is_current(ticket) {
                return Ok(());
            }
            // 取消人声试听与分配音效试听 token 必须在同一临界区，
            // 否则新的人声试听可能夹在两次加锁之间启动，两个本机试听同时输出。
            #[cfg(feature = "voice")]
            self.cancel_voice_preview_inner();
            let (session_generation, playback) = self
                .running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map_or((0, None), |r| (r.generation, Some(r.playback.clone())));
            let audio = Arc::new(PreviewAudioState {
                gain: AtomicF32::new(gain),
                total_gain: self
                    .gains
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .sfx_linear
                    .clone(),
                drained_frames: AtomicU64::new(0),
                rendered_frames: AtomicU64::new(0),
                total_frames: AtomicU64::new(0),
                sample_rate: AtomicU32::new(0),
                finished: AtomicBool::new(false),
                previous_playback: playback,
                #[cfg(test)]
                owner_observed_source_end: Mutex::new(None),
            });
            let mut control = self
                .preview_control
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !self.audio_request_is_current(ticket) {
                return Ok(());
            }
            anyhow::ensure!(!self.is_quitting(), "程序正在退出");
            if let Some(playback) = &audio.previous_playback {
                playback.stop_all();
            }
            let token = self.preview_gen.fetch_add(1, Ordering::AcqRel) + 1;
            *control = Some(PreviewControl {
                token,
                sound_id,
                audio: audio.clone(),
                session_generation,
            });
            (token, session_generation, audio)
        };
        let payload = match prepare() {
            Ok(payload) => payload,
            Err(error) => {
                self.clear_preview_control(token);
                if self.is_quitting() || self.preview_gen.load(Ordering::Acquire) != token {
                    return Ok(());
                }
                return Err(error);
            }
        };
        let _operation = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        // After admission, cancellation belongs to the accepted audio token.
        // A rejected later Play (invalid slot / full queue) must preserve this
        // valid preview even while it is still decoding.
        if self.is_quitting() || self.preview_gen.load(Ordering::Acquire) != token {
            self.clear_preview_control(token);
            return Ok(());
        }
        let current_session = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(0, |r| r.generation);
        if current_session != session_generation {
            self.clear_preview_control(token);
            return Ok(());
        }
        let mut control = self
            .preview_control
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.is_quitting() || self.preview_gen.load(Ordering::Acquire) != token {
            if control.as_ref().is_some_and(|c| c.token == token) {
                *control = None;
            }
            return Ok(());
        }
        self.join_preview();
        if self.is_quitting() || self.preview_gen.load(Ordering::Acquire) != token {
            if control.as_ref().is_some_and(|c| c.token == token) {
                *control = None;
            }
            return Ok(());
        }
        let generation = self.preview_gen.clone();
        let completion = audio.clone();
        let thread = match std::thread::Builder::new()
            .name("preview".into())
            .spawn(move || {
                if generation.load(Ordering::Acquire) == token {
                    if let Err(error) = play(payload, token, generation, audio) {
                        log::warn!("试听播放失败: {error:#}");
                    }
                }
                completion.finished.store(true, Ordering::Release);
            }) {
            Ok(thread) => thread,
            Err(error) => {
                *control = None;
                return Err(error.into());
            }
        };
        *self
            .preview_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(thread);
        Ok(())
    }

    /// Cancel and close the previous stream before another lifecycle operation starts.
    pub fn stop_preview(&self) {
        self.stop_preview_requested(self.reserve_audio_request());
    }

    pub fn stop_preview_requested(&self, ticket: u64) {
        let _operation = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if !self.audio_request_is_current(ticket) {
            return;
        }
        self.stop_preview_inner();
    }

    fn stop_preview_inner(&self) {
        let mut control = self
            .preview_control
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.preview_gen.fetch_add(1, Ordering::AcqRel);
        *control = None;
        self.join_preview();
    }

    fn clear_preview_control(&self, token: u64) {
        let mut control = self
            .preview_control
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if control.as_ref().is_some_and(|c| c.token == token) {
            *control = None;
        }
    }

    fn join_preview(&self) {
        // Every caller holds preview_control until closure completes, including
        // lifecycle callers. Taking the handle alone does not prove it is closed.
        let thread = self
            .preview_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }

    // ─────────────────────────────────────── 变声命令

    /// 麦克风校准开始：人声路径回到原声，直到 [`voice_calibration_end`] 或租约到期。
    ///
    /// 校准读取送往虚拟麦克风的电平，变声启用时那是变声与路径限幅之后的信号，
    /// 得到的增益会偏离真实人声。该层只读写变声状态，不取生命周期锁、不重建会话，
    /// 因此可在界面线程上同步调用。
    ///
    /// [`voice_calibration_end`]: Self::voice_calibration_end
    pub fn voice_calibration_begin(&self) {
        #[cfg(feature = "voice")]
        self.voice.begin_calibration(CALIBRATION_VOICE_LEASE);
    }

    /// 麦克风校准结束：解除暂停；没有在校准时无操作。
    pub fn voice_calibration_end(&self) {
        #[cfg(feature = "voice")]
        self.voice.end_calibration();
    }

    pub fn voice_status(&self) -> VoiceStatusDto {
        #[cfg(feature = "voice")]
        {
            // 命令取消后工作线程会自行退出；轮询顺手回收已结束的所有者。
            self.reap_voice_preview_thread();
            self.voice.status()
        }
        #[cfg(not(feature = "voice"))]
        {
            VoiceStatusDto::disabled()
        }
    }

    #[cfg(feature = "voice")]
    fn reap_voice_preview_thread(&self) {
        let mut slot = self
            .voice_preview_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(thread) = slot.take() {
                let _ = thread.join();
            }
        }
    }

    pub fn voice_presets(&self) -> Vec<VoicePresetDto> {
        #[cfg(feature = "voice")]
        {
            self.voice.presets()
        }
        #[cfg(not(feature = "voice"))]
        {
            Vec::new()
        }
    }

    pub fn voice_set_enabled(&self, enabled: bool) -> Result<VoiceStatusDto> {
        #[cfg(feature = "voice")]
        {
            self.voice.set_enabled(enabled)
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = enabled;
            anyhow::bail!("当前构建未启用变声")
        }
    }

    pub fn voice_set_bypass(&self, bypass: bool) -> Result<VoiceStatusDto> {
        #[cfg(feature = "voice")]
        {
            self.voice.set_bypass(bypass)
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = bypass;
            anyhow::bail!("当前构建未启用变声")
        }
    }

    pub fn voice_set_params(&self, params: VoiceParamsDto) -> Result<VoiceStatusDto> {
        #[cfg(feature = "voice")]
        {
            self.voice.set_params(params)
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = params;
            anyhow::bail!("当前构建未启用变声")
        }
    }

    pub fn voice_select_preset(&self, id: &str) -> Result<VoiceStatusDto> {
        #[cfg(feature = "voice")]
        {
            self.voice.select_preset(id)
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = id;
            anyhow::bail!("当前构建未启用变声")
        }
    }

    pub fn voice_save_as_user_preset(&self, name: &str) -> Result<VoiceStatusDto> {
        #[cfg(feature = "voice")]
        {
            self.voice.save_as_user_preset(name)
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = name;
            anyhow::bail!("当前构建未启用变声")
        }
    }

    pub fn voice_save_preset(&self) -> Result<VoiceStatusDto> {
        #[cfg(feature = "voice")]
        {
            self.voice.save_preset()
        }
        #[cfg(not(feature = "voice"))]
        {
            anyhow::bail!("当前构建未启用变声")
        }
    }

    pub fn voice_rename_preset(&self, id: &str, name: &str) -> Result<VoiceStatusDto> {
        #[cfg(feature = "voice")]
        {
            self.voice.rename_preset(id, name)
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = (id, name);
            anyhow::bail!("当前构建未启用变声")
        }
    }

    pub fn voice_delete_preset(&self, id: &str) -> Result<VoiceStatusDto> {
        #[cfg(feature = "voice")]
        {
            self.voice.delete_preset(id)
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = id;
            anyhow::bail!("当前构建未启用变声")
        }
    }

    pub fn voice_restore_draft(&self) -> Result<VoiceStatusDto> {
        #[cfg(feature = "voice")]
        {
            self.voice.restore_draft()
        }
        #[cfg(not(feature = "voice"))]
        {
            anyhow::bail!("当前构建未启用变声")
        }
    }

    // ─────────────────────────────────────── 三秒人声试听

    /// 开始三秒人声试听；与音效试听共用本机输出仲裁。
    ///
    /// 未启动或刚释放时以 `Unavailable` 报告原因，界面直接显示具体文案。
    pub fn voice_preview_start(&self) -> Result<VoiceStatusDto> {
        #[cfg(feature = "voice")]
        {
            self.voice_preview_start_inner()
        }
        #[cfg(not(feature = "voice"))]
        {
            anyhow::bail!("当前构建未启用变声")
        }
    }

    /// 取消人声试听；第二次点击、预设切换与全局停止走同一清理路径。
    pub fn voice_preview_stop(&self) -> Result<VoiceStatusDto> {
        #[cfg(feature = "voice")]
        {
            let _operation = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
            self.cancel_voice_preview_inner();
            Ok(self.voice_status())
        }
        #[cfg(not(feature = "voice"))]
        {
            anyhow::bail!("当前构建未启用变声")
        }
    }

    #[cfg(feature = "voice")]
    fn voice_preview_start_inner(&self) -> Result<VoiceStatusDto> {
        let _operation = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_quitting() {
            self.voice.preview_unavailable_now("程序正在退出");
            return Ok(self.voice_status());
        }
        let Some(preview) = self.voice.preview_control() else {
            self.voice.preview_unavailable_now("音频未启动");
            return Ok(self.voice_status());
        };
        let phase_control = self
            .voice_control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(phase_control) = phase_control else {
            self.voice.preview_unavailable_now("音频未启动");
            return Ok(self.voice_status());
        };

        // 本机输出互斥：音效试听及其输出线程必须先结束。
        self.stop_preview_inner();
        self.voice.preview_cancel();
        let generation = self.voice.preview_begin();
        let service = self.voice.clone();
        let epoch = service.preview_epoch();

        let mut slot = self
            .voice_preview_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(previous) = slot.take() {
            let _ = previous.join();
        }
        #[cfg(test)]
        let player = self
            .voice_preview_player
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let job = VoicePreviewJob {
            service,
            preview,
            phase_control,
            generation,
            epoch,
            #[cfg(test)]
            player,
        };
        match std::thread::Builder::new()
            .name("voice-preview".into())
            .spawn(move || run_voice_preview(job))
        {
            Ok(thread) => *slot = Some(thread),
            Err(error) => {
                self.voice
                    .preview_unavailable(generation, format!("启动人声试听线程失败：{error}"));
            }
        }
        drop(slot);
        Ok(self.voice_status())
    }

    /// 取消录制或回放并等待所有者线程退出；记录区由试听句柄原子复位。
    #[cfg(feature = "voice")]
    fn cancel_voice_preview_inner(&self) {
        self.voice.preview_cancel();
        let thread = self
            .voice_preview_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }

    /// 程序启动后绑定配置里的人声效果热键，并启动唯一的轮询线程。
    ///
    /// 绑定失败不阻止程序启动；提示通过独立的人声提示字段交给界面。
    pub fn initialize_hotkeys(&self, app: Option<&tauri::AppHandle>) -> Result<()> {
        if let Some(app) = app {
            *self.app.lock().unwrap_or_else(|e| e.into_inner()) = Some(app.clone());
        }
        self.ensure_hotkey_thread();
        #[cfg(feature = "voice")]
        {
            if let Err(error) = self.ensure_voice_hotkeys(app) {
                self.record_voice_notice(format!("人声效果热键绑定失败：{error:#}"));
                return Err(error);
            }
        }
        Ok(())
    }

    /// 确保变声热键已绑定；已绑定时不重复注册。
    #[cfg(feature = "voice")]
    fn ensure_voice_hotkeys(&self, app: Option<&tauri::AppHandle>) -> Result<()> {
        let specs = self.voice.hotkey_specs();
        if !specs.iter().any(|spec| !spec.trim().is_empty()) {
            return Ok(());
        }
        if self
            .hotkeys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .voice_hotkeys()
            .is_some()
        {
            return Ok(());
        }
        // 启动时槽位热键尚未绑定，但配置里的键位仍会与常驻的人声热键冲突。
        let slot_specs = self.slot_hotkey_specs();
        hotkeys::validate_disjoint(&[&slot_specs, &specs]).context("人声效果热键与音效热键冲突")?;
        let (bound, how) = self.bind_hotkeys_on_main_thread(app, &specs)?;
        println!("[人声热键] 绑定成功（{how}）");
        self.install_voice_hotkeys(bound)?;
        self.ensure_hotkey_thread();
        Ok(())
    }

    /// 启动唯一的轮询线程；槽位与变声事件由同一个消费者排水。
    fn ensure_hotkey_thread(&self) {
        let mut handle = self.hotkey_thread.lock().unwrap_or_else(|e| e.into_inner());
        if handle.is_some() {
            return;
        }
        let shutdown = self.hotkey_shutdown.clone();
        let groups = self.hotkeys.clone();
        let slot_action = self.slot_action.clone();
        let voice_action = self.voice_hotkey_handler();
        match std::thread::Builder::new()
            .name("hotkeys".into())
            .spawn(move || {
                while !shutdown.load(Ordering::Acquire) {
                    let actions = groups
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .poll_actions();
                    for (group, index) in actions {
                        match group {
                            HotkeyGroup::Slots => {
                                let action = slot_action
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .clone();
                                if let Some(action) = action {
                                    action(index);
                                }
                            }
                            HotkeyGroup::Voice => voice_action(index),
                        }
                    }
                    std::thread::sleep(Duration::from_millis(12));
                }
            }) {
            Ok(thread) => *handle = Some(thread),
            Err(error) => {
                drop(handle);
                self.record_voice_notice(format!("启动热键轮询线程失败：{error}"));
            }
        }
    }

    /// 进程退出前注销全部热键并 join 轮询线程；可重复调用。
    pub fn shutdown_hotkeys(&self) {
        self.hotkey_shutdown.store(true, Ordering::Release);
        if let Some(thread) = self
            .hotkey_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = thread.join();
        }
        *self.slot_action.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let groups = self.hotkeys.clone();
        if let Err(error) = self.with_hotkey_groups(groups.clone(), |groups| groups.shutdown()) {
            // 应用退出时主线程调度可能已不可用；直接注销比留下系统热键更安全。
            log::warn!("主线程注销热键失败，改为直接注销: {error:#}");
            if let Err(error) = groups.lock().unwrap_or_else(|e| e.into_inner()).shutdown() {
                self.record_error(format!("释放热键失败: {error:#}"));
            }
        }
    }

    fn install_slot_hotkeys(&self, hotkeys: Option<Hotkeys>) -> Result<()> {
        self.with_hotkey_groups(self.hotkeys.clone(), move |groups| {
            groups.set_slot_hotkeys(hotkeys);
            Ok(())
        })
    }

    #[cfg(feature = "voice")]
    fn install_voice_hotkeys(&self, hotkeys: Hotkeys) -> Result<()> {
        self.with_hotkey_groups(self.hotkeys.clone(), move |groups| {
            groups.set_voice_hotkeys(Some(hotkeys));
            Ok(())
        })
    }

    fn record_voice_notice(&self, message: impl Into<String>) {
        set_voice_notice(&self.voice_notice, message);
    }

    fn dismiss_voice_notice(&self) {
        clear_voice_notice(&self.voice_notice);
    }

    /// 保存三个人声效果热键并立即重绑；与音频会话无关。
    ///
    /// 先校验跨组冲突再落盘；重绑失败保留旧绑定并把错误返回界面。
    pub fn voice_set_hotkeys(
        &self,
        bypass: &str,
        next: &str,
        prev: &str,
    ) -> Result<VoiceStatusDto> {
        #[cfg(feature = "voice")]
        {
            let _operation = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
            let voice_specs = vec![
                bypass.trim().to_owned(),
                next.trim().to_owned(),
                prev.trim().to_owned(),
            ];
            let slot_specs = self.slot_hotkey_specs();
            hotkeys::validate_disjoint(&[&slot_specs, &voice_specs])?;
            // 先保存用户选择；系统级冲突导致重绑失败时，旧绑定仍然有效。
            self.voice
                .save_hotkeys(&voice_specs[0], &voice_specs[1], &voice_specs[2])?;
            let has_voice_set = self
                .hotkeys
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .voice_hotkeys()
                .is_some();
            if !voice_specs.iter().any(|spec| !spec.trim().is_empty()) {
                if has_voice_set {
                    self.with_hotkey_groups(self.hotkeys.clone(), |groups| groups.clear_voice())
                        .context("注销人声效果热键失败")?;
                }
                self.dismiss_voice_notice();
                return Ok(self.voice.status());
            }
            if has_voice_set {
                self.with_hotkey_groups(self.hotkeys.clone(), move |groups| {
                    groups.rebind_voice(&voice_specs)
                })
                .context("人声效果热键重绑失败，原热键仍然有效")?;
            } else {
                let app = self.app.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let (bound, _) = self.bind_hotkeys_on_main_thread(app.as_ref(), &voice_specs)?;
                self.install_voice_hotkeys(bound)?;
            }
            self.ensure_hotkey_thread();
            self.dismiss_voice_notice();
            Ok(self.voice.status())
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = (bypass, next, prev);
            anyhow::bail!("当前构建未启用变声")
        }
    }

    /// 当前生效的音效槽位热键；无会话时读取配置里的持久列表。
    #[cfg(feature = "voice")]
    fn slot_hotkey_specs(&self) -> Vec<String> {
        let running_specs = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|running| {
                running
                    .slots_meta
                    .iter()
                    .map(|slot| slot.hotkey.clone())
                    .collect::<Vec<_>>()
            });
        if let Some(specs) = running_specs {
            return specs;
        }
        // 只读用途：配置异常时仍按能读出的槽位热键校验，真正的写入由 `save_hotkeys` 严格拒绝。
        app_config::Config::load_from(self.voice.config_path())
            .sound_entries()
            .into_iter()
            .map(|entry| entry.hotkey)
            .collect()
    }

    /// 在需要时把绑定动作调度到主线程，并复用宿主唯一的管理器。
    fn bind_hotkeys_on_main_thread(
        &self,
        app: Option<&tauri::AppHandle>,
        keys: &[String],
    ) -> Result<(Hotkeys, &'static str)> {
        bind_hotkeys_with_manager(app, &self.hotkey_manager, keys)
    }

    /// 未编译变声后端时没有人声效果热键；保留空处理器让轮询线程结构一致。
    #[cfg(not(feature = "voice"))]
    fn voice_hotkey_handler(&self) -> Arc<dyn Fn(usize) + Send + Sync> {
        Arc::new(|_| {})
    }

    /// 热键线程的事件入口：只调用宿主控制路径，不接触音频回调。
    ///
    /// 事件每次读取当前会话代次；0 表示无会话。变声热键跨会话常驻，
    /// 因此不需要捕获旧代次。
    #[cfg(feature = "voice")]
    fn voice_hotkey_handler(&self) -> Arc<dyn Fn(usize) + Send + Sync> {
        let voice = self.voice.clone();
        let requests = self.audio_requests.clone();
        let notice_state = self.voice_notice.clone();
        Arc::new(move |index| {
            let action = match index {
                0 => VoiceHotkeyAction::ToggleBypass,
                1 => VoiceHotkeyAction::NextPreset,
                2 => VoiceHotkeyAction::PrevPreset,
                _ => return,
            };
            let generation = requests.generation.load(Ordering::Acquire);
            match route_voice_hotkey(&voice, generation, action) {
                Ok(outcome) => {
                    if let Some(message) = outcome.notice {
                        set_voice_notice(&notice_state, message);
                    } else if outcome.acted {
                        // 动作已生效，之前“音频未启动/变声未开启”的提示不再成立。
                        clear_voice_notice(&notice_state);
                    }
                }
                Err(error) => {
                    set_voice_notice(&notice_state, format!("人声效果热键执行失败：{error:#}"));
                }
            }
        })
    }

    #[cfg(feature = "voice")]
    fn attach_voice_control(&self, control: Option<VoiceControl>) {
        let mut slot = self.voice_control.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::mem::replace(&mut *slot, control.clone());
        drop(previous);
        drop(slot);

        match control {
            Some(control) => self.voice.attach(control),
            None => self.voice.detach(),
        }
    }

    /// 发布或释放本会话的试听记录句柄；旧试听立即失效。
    #[cfg(feature = "voice")]
    fn attach_voice_preview(&self, control: Option<VoicePreviewControl>) {
        self.voice.attach_preview(control);
    }

    pub fn record_error(&self, e: impl Into<String>) {
        *self.last_error.lock().unwrap_or_else(|x| x.into_inner()) = Some(e.into());
    }
}

/// 试听线程所需的全部句柄；音频回调不接触这些状态。
#[cfg(feature = "voice")]
struct VoicePreviewJob {
    service: VoiceService,
    preview: VoicePreviewControl,
    phase_control: VoiceControl,
    generation: u64,
    epoch: Arc<AtomicU64>,
    #[cfg(test)]
    player: Option<VoicePreviewPlayer>,
}

/// 在控制线程完成准备、录制与回放；任何阶段都按代次丢弃旧结果。
#[cfg(feature = "voice")]
fn run_voice_preview(job: VoicePreviewJob) {
    let VoicePreviewJob {
        service,
        preview,
        phase_control,
        generation,
        epoch,
        #[cfg(test)]
        player,
    } = job;

    // 预热与淡出阶段不能写记录区；阶段稳定后自动开始，超时明确失败。
    let prepare_deadline = std::time::Instant::now() + Duration::from_secs(1);
    let start = loop {
        if epoch.load(Ordering::Acquire) != generation {
            return;
        }
        match preview.start() {
            Ok(start) => break start,
            Err(PreviewError::Busy) => {
                if std::time::Instant::now() >= prepare_deadline {
                    service.preview_unavailable(generation, "变声阶段未稳定，试听已取消");
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(PreviewError::AlreadyActive) => {
                preview.stop();
                if std::time::Instant::now() >= prepare_deadline {
                    service.preview_unavailable(generation, "变声阶段未稳定，试听已取消");
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(PreviewError::Unavailable) => {
                service.preview_unavailable(generation, "试听记录区不可用");
                return;
            }
        }
    };
    if epoch.load(Ordering::Acquire) != generation {
        // 取消与启动竞争时立即归还记录区，避免留下无人取走的录制。
        preview.stop();
        return;
    }
    service.preview_mark_recording(generation);

    let expected_phase = match start.source {
        audio_core::voice::VoicePreviewSource::Capture => VoicePhase::Off,
        audio_core::voice::VoicePreviewSource::Render => VoicePhase::Wet,
    };
    // 记录区固定三秒；允许半秒的设备回调与轮询余量。
    let record_deadline = std::time::Instant::now() + Duration::from_millis(3_500);
    let clip = loop {
        if epoch.load(Ordering::Acquire) != generation {
            return;
        }
        if preview.is_finished() {
            match preview.take() {
                Some(clip) => break clip,
                None => {
                    service.preview_unavailable(generation, "没有录到人声，试听已取消");
                    return;
                }
            }
        }
        if phase_control.phase() != expected_phase {
            preview.stop();
            service.preview_unavailable(generation, "变声阶段已变化，试听已取消");
            return;
        }
        if std::time::Instant::now() >= record_deadline {
            preview.stop();
            service.preview_unavailable(generation, "没有录到人声，试听已取消");
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if clip.samples.is_empty() {
        service.preview_unavailable(generation, "没有录到人声，试听已取消");
        return;
    }
    if epoch.load(Ordering::Acquire) != generation {
        return;
    }

    service.preview_mark_playing(generation);
    #[cfg(test)]
    let result = match player {
        Some(player) => player(clip, generation, epoch),
        None => play_voice_clip(clip, generation, epoch),
    };
    #[cfg(not(test))]
    let result = play_voice_clip(clip, generation, epoch);
    match result {
        Ok(()) => service.preview_finish(generation),
        Err(error) => {
            service.preview_unavailable(generation, format!("试听输出失败：{error:#}"));
        }
    }
}

/// 试听回放只走本机默认输出；采样率不同时复用现有试听重采样规则。
#[cfg(feature = "voice")]
fn play_voice_clip(clip: VoicePreviewClip, generation: u64, epoch: Arc<AtomicU64>) -> Result<()> {
    anyhow::ensure!(!clip.samples.is_empty(), "没有录到人声，试听已取消");
    let backend = CpalBackend::new();
    let device = backend
        .default_output()
        .context("系统没有默认输出设备（试听需要它）")?;
    let format = backend.output_format(&device, Some(StreamFormat::new(48_000, 2)))?;
    let stereo: Vec<f32> = clip
        .samples
        .iter()
        .flat_map(|sample| [*sample, *sample])
        .collect();
    let samples = if clip.sample_rate == format.sample_rate {
        stereo
    } else {
        audio_session::resample_stereo(&stereo, clip.sample_rate, format.sample_rate)
    };
    anyhow::ensure!(!samples.is_empty(), "没有录到人声，试听已取消");
    let audio = Arc::new(PreviewAudioState {
        gain: AtomicF32::new(1.0),
        total_gain: Arc::new(AtomicF32::new(1.0)),
        drained_frames: AtomicU64::new(0),
        rendered_frames: AtomicU64::new(0),
        total_frames: AtomicU64::new(0),
        sample_rate: AtomicU32::new(0),
        finished: AtomicBool::new(false),
        previous_playback: None,
        #[cfg(test)]
        owner_observed_source_end: Mutex::new(None),
    });
    play_samples_with_limit(
        backend,
        Arc::new(samples),
        device,
        format,
        audio,
        generation,
        epoch,
        preview_limit_for(clip.post_limiter),
    )
}

/// 把一次热键事件路由到当前的变声状态。
///
/// 代次为 0 表示没有音频会话：预设热键仍更新待用参数，旁路热键只提示，
/// 不会建立旁路或启动音频。
#[cfg(feature = "voice")]
pub(crate) fn route_voice_hotkey(
    voice: &VoiceService,
    session_generation: u64,
    action: VoiceHotkeyAction,
) -> Result<VoiceHotkeyOutcome> {
    voice.handle_hotkey(action, session_generation != 0)
}

impl Drop for AudioHost {
    fn drop(&mut self) {
        self.release_inner();
        self.shutdown_hotkeys();
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

// ─────────────────────────────────────────── 热键绑定

/// 取得宿主唯一的平台管理器；没有时在**主线程**创建并缓存。
///
/// macOS 的 Carbon 热键要求管理器在主线程创建，而 Tauri 的命令处理器不保证
/// 跑在主线程，所以创建动作必须调度过去。创建之后的绑定可以复用同一句柄。
fn acquire_hotkey_manager(slot: &Mutex<Option<Arc<HotkeyManager>>>) -> Result<Arc<HotkeyManager>> {
    if let Some(manager) = slot.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return Ok(manager);
    }
    let created = Arc::new(Hotkeys::new_manager()?);
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    Ok(guard.get_or_insert(created).clone())
}

/// 在**主线程**上创建/复用管理器并绑定热键，必要时取回结果。
fn bind_hotkeys_with_manager(
    app: Option<&tauri::AppHandle>,
    manager_slot: &Arc<Mutex<Option<Arc<HotkeyManager>>>>,
    keys: &[String],
) -> Result<(Hotkeys, &'static str)> {
    if hotkeys::is_main_thread() {
        let manager = acquire_hotkey_manager(manager_slot)?;
        return Ok((Hotkeys::bind_with(manager, keys)?, "调用方本来就在主线程"));
    }
    let Some(app) = app else {
        // 无 GUI 场景（--smoke）：直接绑，让 new_manager 给出明确报错
        let manager = acquire_hotkey_manager(manager_slot)?;
        return Ok((Hotkeys::bind_with(manager, keys)?, "无 GUI 上下文"));
    };

    let (tx, rx) = mpsc::channel();
    let keys: Vec<String> = keys.to_vec();
    let manager_slot = manager_slot.clone();
    app.run_on_main_thread(move || {
        let result = acquire_hotkey_manager(&manager_slot)
            .and_then(|manager| Hotkeys::bind_with(manager, &keys));
        let _ = tx.send(result);
    })
    .context("把热键绑定调度到主线程失败")?;

    let hotkeys = rx
        .recv_timeout(Duration::from_secs(6))
        .map_err(|_| anyhow::anyhow!("在主线程绑定热键超时（6 秒）"))??;
    Ok((hotkeys, "已调度到主线程"))
}

// ─────────────────────────────────────────── 试听

/// 试听回放的限幅模式。
#[derive(Clone, Copy)]
enum PreviewLimit {
    /// 片段已通过正式人声路径限幅，不再重复限幅。
    #[cfg_attr(not(feature = "voice"), allow(dead_code))]
    None,
    Ceiling(f32),
}

#[cfg(feature = "voice")]
fn preview_limit_for(post_limiter: bool) -> PreviewLimit {
    if post_limiter {
        PreviewLimit::None
    } else {
        PreviewLimit::Ceiling(-3.0)
    }
}

struct PreviewRender {
    cursor: audio_session::SampleCursor,
    limiter: Option<Limiter>,
}

struct PreviewCursor(Arc<Mutex<Option<PreviewRender>>>);
impl Drop for PreviewCursor {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Streams stay on the owner. The detached callback shell cannot retain PCM
/// after close, including failed opens, mismatched formats and unwinding.
fn play_samples(
    backend: impl AudioBackend,
    samples_owned: Arc<Vec<f32>>,
    device: DeviceId,
    fmt: StreamFormat,
    audio: Arc<PreviewAudioState>,
    my_gen: u64,
    gen: Arc<AtomicU64>,
) -> Result<()> {
    play_samples_with_limit(
        backend,
        samples_owned,
        device,
        fmt,
        audio,
        my_gen,
        gen,
        PreviewLimit::Ceiling(-1.0),
    )
}

// 回放参数与现有 play_samples 一致，仅额外携带限幅模式；拆结构体会让调用点更长。
#[allow(clippy::too_many_arguments)]
fn play_samples_with_limit(
    backend: impl AudioBackend,
    samples_owned: Arc<Vec<f32>>,
    device: DeviceId,
    fmt: StreamFormat,
    audio: Arc<PreviewAudioState>,
    my_gen: u64,
    gen: Arc<AtomicU64>,
    limit: PreviewLimit,
) -> Result<()> {
    if gen.load(Ordering::Acquire) != my_gen {
        return Ok(());
    }
    anyhow::ensure!(fmt.sample_rate > 0 && fmt.channels > 0, "无效试听格式");
    let total_frames = samples_owned.len() / 2;
    audio.sample_rate.store(fmt.sample_rate, Ordering::Release);
    audio
        .total_frames
        .store(total_frames as u64, Ordering::Release);
    let limiter = match limit {
        PreviewLimit::None => None,
        PreviewLimit::Ceiling(ceiling_db) => Some(Limiter::new_with_channels(
            fmt.sample_rate,
            2,
            ceiling_db,
            2.0,
            80.0,
        )),
    };
    let latency_frames = limiter
        .as_ref()
        .map_or(0, |limiter| limiter.latency_frames()) as u64;
    let cursor = PreviewCursor(Arc::new(Mutex::new(Some(PreviewRender {
        cursor: audio_session::SampleCursor::new(samples_owned, 0.0),
        limiter,
    }))));
    let callback_cursor = cursor.0.clone();
    let callback_gen = gen.clone();
    let callback_audio = audio.clone();
    let stream = backend.open_output(
        &device,
        Some(fmt),
        Box::new(move |out: &mut [f32]| {
            out.fill(0.0);
            if callback_gen.load(Ordering::Acquire) != my_gen {
                return;
            }
            // Wait for both renderers to retire the preceding sound, including
            // their limiter tails. Silence while waiting does not consume PCM.
            if callback_audio
                .previous_playback
                .as_ref()
                .is_some_and(|p| !p.stop_all_finished())
            {
                return;
            }
            if let Ok(mut guard) = callback_cursor.try_lock() {
                if let Some(render) = guard.as_mut() {
                    let gain = callback_audio.gain.get() * callback_audio.total_gain.get();
                    let remaining = total_frames as u64
                        - callback_audio.rendered_frames.load(Ordering::Relaxed);
                    let rendered = remaining.min((out.len() / fmt.ch()) as u64);
                    // 与正式输出一样：先在立体声域限幅，再映射设备声道。
                    // 固定帧缓冲不依赖设备回调大小，不在回调中分配。
                    for frame in out.chunks_exact_mut(fmt.ch()) {
                        let mut stereo = [0.0; 2];
                        render.cursor.render(&mut stereo, 2);
                        stereo[0] *= gain;
                        stereo[1] *= gain;
                        if let Some(limiter) = render.limiter.as_mut() {
                            limiter.process_in_place(&mut stereo);
                        }
                        let mono = (stereo[0] + stereo[1]) * 0.5;
                        frame.fill(mono);
                        if frame.len() > 1 {
                            frame[..2].copy_from_slice(&stereo);
                        }
                    }
                    if callback_gen.load(Ordering::Acquire) != my_gen {
                        out.fill(0.0);
                        return;
                    }
                    callback_audio
                        .rendered_frames
                        .fetch_add(rendered, Ordering::Release);
                    callback_audio
                        .drained_frames
                        .fetch_add((out.len() / fmt.ch()) as u64 - rendered, Ordering::Release);
                }
            }
        }),
    )?;
    anyhow::ensure!(stream.format() == fmt, "试听设备实际格式与协商格式不一致");
    let dur = Duration::from_secs_f32(total_frames as f32 / fmt.sample_rate as f32 + 0.1);
    let start = std::time::Instant::now();
    let mut first_pcm_at = None;
    while gen.load(Ordering::Acquire) == my_gen {
        let rendered = audio.rendered_frames.load(Ordering::Acquire);
        if rendered >= total_frames as u64
            && audio.drained_frames.load(Ordering::Acquire) >= latency_frames
        {
            break;
        }
        #[cfg(test)]
        if rendered >= total_frames as u64 {
            if let Some(observed) = audio.owner_observed_source_end.lock().unwrap().as_ref() {
                let _ = observed.send(());
            }
        }
        if rendered > 0 {
            let first = first_pcm_at.get_or_insert_with(std::time::Instant::now);
            if first.elapsed() >= dur {
                break;
            }
        } else if start.elapsed() >= Duration::from_secs(5) {
            anyhow::bail!("试听等待音频设备响应超时（5 秒）");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(stream);
    drop(cursor);
    Ok(())
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;

#[cfg(test)]
mod panic_path_tests {
    // 测试名用中文描述 + 保留环境变量的大写写法（APPDATA / XDG / HOME / LOCALAPPDATA…）。
    // 改成 snake_case 要把缩写降成小写，可读性反而更差 —— 这里 lint 是错的。
    #![allow(non_snake_case)]
    use super::*;
    use std::ffi::OsString;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn windows_下日志走_LOCALAPPDATA() {
        let env = env_of(&[("LOCALAPPDATA", "/LOCALAPPDATA"), ("HOME", "/HOME")]);
        let p = panic_log_path_on(app_config::Platform::Windows, &env).unwrap();
        assert_eq!(
            p,
            std::path::PathBuf::from("/LOCALAPPDATA/麦克风混音助手/logs/panic.log")
        );
    }

    #[test]
    fn windows_上没有_LOCALAPPDATA_就是_None_不能退回_HOME() {
        let env = env_of(&[("HOME", "/HOME")]);
        assert!(
            panic_log_path_on(app_config::Platform::Windows, &env).is_none(),
            "Windows 上不该去读 HOME"
        );
    }

    #[test]
    fn unix_下日志走_HOME_的_Library_Logs() {
        let env = env_of(&[("HOME", "/HOME")]);
        let p = panic_log_path_on(app_config::Platform::Unix, &env).unwrap();
        assert_eq!(
            p,
            std::path::PathBuf::from("/HOME/Library/Logs/麦克风混音助手/panic.log")
        );
    }

    #[test]
    fn 空字符串不算有值() {
        let env = env_of(&[("LOCALAPPDATA", "")]);
        assert!(panic_log_path_on(app_config::Platform::Windows, &env).is_none());
    }
}
