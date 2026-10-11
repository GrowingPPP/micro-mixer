//! 混音引擎 —— 产品的核心。
//!
//! 数据流：
//! ```text
//! 真实麦克风 ──► 下混单声道 ──► 漂移控制重采样 ──┐
//!                                              ├─► 环形缓冲 ──► 混音 ──► 限幅 ──► 虚拟麦克风
//! 音效槽位 ×N（预解码驻留内存）─────────────────┘
//! ```
//!
//! **实时安全**：`process()` 内部零分配、零加锁、零 IO。
//! 所有缓冲都在构造时预分配；主线程通过**无锁命令队列**操控播放。

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rtrb::{Consumer, Producer, RingBuffer};

use crate::decode::DecodedAudio;
use crate::device::StreamFormat;
use crate::limiter::Limiter;
use crate::resample::{DriftController, LinearResampler};

// ─────────────────────────────────────────── 无锁原子量

/// 可在实时线程读写的 f32。
#[derive(Debug)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    #[inline]
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    #[inline]
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
}

// ─────────────────────────────────────────── 控制命令

/// 主线程 → 音频线程的控制命令。
///
/// 走 SPSC 环形缓冲，音频线程零加锁地取出。
#[derive(Debug, Clone, Copy)]
pub enum Command {
    /// 从头播放某个槽位
    Play { slot: u8 },
    /// 淡出并停止某个槽位
    Stop { slot: u8 },
    /// 停止全部
    StopAll,
    /// 调整某个槽位的音量（dB），**运行时立即生效**
    SetGain { slot: u8, db: f32 },
}

// ─────────────────────────────────────────── 音效槽位

/// 一个音效槽位。加载后**预解码驻留内存**，实时线程只读 PCM。
#[derive(Debug, Clone)]
pub struct SfxSlot {
    /// 显示名（通常是文件名）
    pub name: String,
    /// 交错立体声 f32，已重采样到输出采样率
    pub samples: Arc<Vec<f32>>,
    /// 播完后是否循环
    pub looping: bool,
    /// 该槽位独立增益（dB）
    pub gain_db: f32,
}

impl SfxSlot {
    /// 从解码结果构造。
    pub fn from_decoded(name: impl Into<String>, audio: &DecodedAudio, looping: bool) -> Self {
        Self {
            name: name.into(),
            samples: Arc::new(audio.samples.clone()),
            looping,
            gain_db: 0.0,
        }
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    pub fn duration_secs(&self, sample_rate: u32) -> f32 {
        self.frames() as f32 / sample_rate as f32
    }
}

// ─────────────────────────────────────────── 统计

/// 限幅增益的原子存储偏移（dB）。原子量是无符号的，负增益需要偏移。
pub const LIMITER_DB_OFFSET: f32 = 60.0;

/// 电平表的原子存储偏移（dB）—— 电平是负 dB，原子量无符号，所以加偏移。
pub const METER_DB_OFFSET: f32 = 100.0;

#[inline]
fn db_to_atomic(db: f32) -> u32 {
    ((db + METER_DB_OFFSET).clamp(0.0, 200.0) * 100.0) as u32
}

#[inline]
fn atomic_to_db(v: u32) -> f32 {
    v as f32 / 100.0 - METER_DB_OFFSET
}

#[inline]
fn lin_to_db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

#[derive(Debug, Default)]
pub struct EngineStats {
    pub captured_frames: AtomicU64,
    pub rendered_frames: AtomicU64,
    /// 缓冲被抽干（输出侧没数据可读）的次数
    pub underruns: AtomicU64,
    /// 缓冲写满（输入侧没空间可写）的次数
    pub overruns: AtomicU64,
    /// 当前缓冲水位（样本数）
    pub ring_fill: AtomicUsize,
    /// 是否有任一音效在播放
    pub playing: AtomicBool,
    /// 最后触发/正在播放的槽位；-1 表示无
    pub active_slot: AtomicI32,
    /// 当前槽位播放进度（帧）
    pub playhead: AtomicU64,
    /// 限幅器当前增益（dB），带 [`LIMITER_DB_OFFSET`] 偏移
    pub limiter_gain_db100: AtomicU32,
    /// **送往虚拟麦克风的最终电平**（峰值 / RMS，dB×100 + 偏移）
    ///
    /// 这是产品里最重要的一个指标：它直接回答"队友能不能听到我"。
    /// 语音软件的降噪会吃掉太弱的信号，所以必须让用户看得见。
    pub out_peak_db100: AtomicU32,
    pub out_rms_db100: AtomicU32,
}

impl EngineStats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            captured_frames: self.captured_frames.load(Ordering::Relaxed),
            rendered_frames: self.rendered_frames.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            overruns: self.overruns.load(Ordering::Relaxed),
            ring_fill: self.ring_fill.load(Ordering::Relaxed),
            playing: self.playing.load(Ordering::Relaxed),
            active_slot: self.active_slot.load(Ordering::Relaxed),
            playhead: self.playhead.load(Ordering::Relaxed),
            limiter_gain_db: self.limiter_gain_db100.load(Ordering::Relaxed) as f32 / 100.0
                - LIMITER_DB_OFFSET,
            out_peak_db: atomic_to_db(self.out_peak_db100.load(Ordering::Relaxed)),
            out_rms_db: atomic_to_db(self.out_rms_db100.load(Ordering::Relaxed)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StatsSnapshot {
    pub captured_frames: u64,
    pub rendered_frames: u64,
    pub underruns: u64,
    pub overruns: u64,
    pub ring_fill: usize,
    pub playing: bool,
    pub active_slot: i32,
    pub playhead: u64,
    pub limiter_gain_db: f32,
    /// 送往虚拟麦克风的峰值电平（dBFS）
    pub out_peak_db: f32,
    /// 送往虚拟麦克风的短时 RMS（dBFS）—— 用它判断"够不够响"
    pub out_rms_db: f32,
}

// ─────────────────────────────────────────── 控制面

/// 主线程用来操控引擎的句柄。
///
/// 增益是原子量，播放控制走无锁命令队列 —— **不需要锁，也不阻塞音频线程**。
#[derive(Debug)]
pub struct PlaybackControl {
    /// 用 Mutex 包住是因为 rtrb 的 Producer 是单生产者（非 Sync），
    /// 而句柄要在多个控制线程间共享。控制路径不在实时线程上，加锁无妨。
    cmd: Mutex<Producer<Command>>,
    pub mic_gain_db: AtomicF32,
    pub sfx_gain_db: AtomicF32,
    stats: Arc<EngineStats>,
    slot_count: usize,
}

impl PlaybackControl {
    fn send(&self, cmd: Command) -> bool {
        match self.cmd.lock() {
            Ok(mut p) => p.push(cmd).is_ok(),
            Err(_) => false,
        }
    }

    /// 从头播放指定槽位（对应"按下热键"）。
    pub fn play(&self, slot: usize) -> bool {
        if slot >= self.slot_count {
            return false;
        }
        self.send(Command::Play { slot: slot as u8 })
    }

    /// 淡出并停止指定槽位。
    pub fn stop(&self, slot: usize) -> bool {
        if slot >= self.slot_count {
            return false;
        }
        self.send(Command::Stop { slot: slot as u8 })
    }

    /// 停止全部音效（**人声照常透传**）。
    pub fn stop_all(&self) -> bool {
        self.send(Command::StopAll)
    }

    /// 调整某个槽位的音量（dB）。**下一帧就生效**，不必重播。
    pub fn set_slot_gain(&self, slot: usize, db: f32) -> bool {
        if slot >= self.slot_count {
            return false;
        }
        self.send(Command::SetGain {
            slot: slot as u8,
            db,
        })
    }

    pub fn slot_count(&self) -> usize {
        self.slot_count
    }

    pub fn is_playing(&self) -> bool {
        self.stats.playing.load(Ordering::Relaxed)
    }

    pub fn active_slot(&self) -> i32 {
        self.stats.active_slot.load(Ordering::Relaxed)
    }

    pub fn playhead_frames(&self) -> u64 {
        self.stats.playhead.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }
}

// ─────────────────────────────────────────── 音效播放器

/// 淡入淡出时长（毫秒）。起停与重触发都靠它消除咔哒声。
const FADE_MS: f32 = 8.0;

struct FilePlayer {
    samples: Arc<Vec<f32>>,
    /// 读位置（样本索引，交错立体声）
    pos: usize,
    looping: bool,
    gain: AtomicF32,
    active: bool,
    /// 0..1 淡入淡出系数
    fade: f32,
    /// +1 淡入 / -1 淡出 / 0 稳态
    fade_dir: f32,
    /// 淡出结束后是否要重启（用于"播放中再次触发"）
    restart_after_fade: bool,
    /// 每帧的淡变步进
    fade_step: f32,
    /// 结尾淡出长度（帧）
    end_fade_frames: usize,
}

impl FilePlayer {
    fn new(slot: &SfxSlot, sample_rate: u32) -> Self {
        let fade_frames = (sample_rate as f32 * FADE_MS / 1000.0).max(1.0);
        Self {
            samples: slot.samples.clone(),
            pos: 0,
            looping: slot.looping,
            gain: AtomicF32::new(slot.gain_db),
            active: false,
            fade: 0.0,
            fade_dir: 0.0,
            restart_after_fade: false,
            fade_step: 1.0 / fade_frames,
            end_fade_frames: fade_frames as usize,
        }
    }

    /// 立即从头开始（含淡入）。
    fn start(&mut self) {
        self.pos = 0;
        self.active = true;
        self.fade = 0.0;
        self.fade_dir = 1.0;
        self.restart_after_fade = false;
    }

    /// 请求播放。若正在播放，先快速淡出再重启（避免波形突变的咔哒声）。
    fn trigger(&mut self) {
        if self.active && self.fade > 0.001 {
            self.restart_after_fade = true;
            self.fade_dir = -1.0;
        } else {
            self.start();
        }
    }

    /// 请求停止（淡出）。
    fn stop(&mut self) {
        if self.active {
            self.fade_dir = -1.0;
            self.restart_after_fade = false;
        }
    }

    fn is_active(&self) -> bool {
        self.active
    }

    /// **实时安全**：把本槽位的输出**叠加**到 `out`（交错立体声）。
    #[inline]
    fn render_add(&mut self, out: &mut [f32]) {
        if !self.active {
            return;
        }
        let total = self.samples.len();
        if total < 2 {
            self.active = false;
            return;
        }
        let g = db_to_lin(self.gain.get());

        for frame in out.chunks_exact_mut(2) {
            // 淡变推进（每帧一次，而不是每样本）
            if self.fade_dir > 0.0 {
                self.fade = (self.fade + self.fade_step).min(1.0);
                if self.fade >= 1.0 {
                    self.fade_dir = 0.0;
                }
            } else if self.fade_dir < 0.0 {
                self.fade = (self.fade - self.fade_step).max(0.0);
                if self.fade <= 0.0 {
                    self.fade_dir = 0.0;
                    self.active = false;
                    if self.restart_after_fade {
                        self.start();
                    }
                    return; // 后续帧保持不动（此前的值是累加前的）
                }
            }

            // 到达结尾
            if self.pos + 2 > total {
                if self.looping {
                    self.pos = 0;
                } else {
                    self.active = false;
                    return;
                }
            }

            // 结尾淡出：避免文件在波形中段突然结束产生咔哒
            let remaining = (total - self.pos) / 2;
            let end_env = if remaining < self.end_fade_frames {
                remaining as f32 / self.end_fade_frames as f32
            } else {
                1.0
            };

            let a = self.fade * end_env * g;
            frame[0] += self.samples[self.pos] * a;
            frame[1] += self.samples[self.pos + 1] * a;
            self.pos += 2;
        }
    }
}

// ─────────────────────────────────────────── 采集侧

/// 采集处理：下混 → 漂移控制重采样 → 写入环形缓冲。**实时线程。**
pub struct CaptureProcessor {
    in_channels: usize,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    resampler: LinearResampler,
    drift: DriftController,
    ring: Producer<f32>,
    capacity: usize,
    stats: Arc<EngineStats>,
}

impl CaptureProcessor {
    /// **实时安全。** `data` 是与设备格式一致的交错 f32。
    pub fn process(&mut self, data: &[f32]) {
        let ch = self.in_channels;
        let frames = data.len() / ch;
        if frames == 0 {
            return;
        }

        // 1. 下混为单声道 —— 保证单声道兼容性（人声居中，下混不衰减）
        self.mono.clear();
        if ch == 1 {
            self.mono.extend_from_slice(&data[..frames]);
        } else {
            let inv = 1.0 / ch as f32;
            for f in data[..frames * ch].chunks_exact(ch) {
                self.mono.push(f.iter().sum::<f32>() * inv);
            }
        }

        // 2. 时钟漂移控制：按缓冲水位微调重采样比率
        //    （读的是推送前的水位 —— 控制环要看的是"当前积压了多少"）
        let fill_before = self.capacity.saturating_sub(self.ring.slots());
        self.resampler.set_ratio(self.drift.ratio_for(fill_before));

        // 3. 重采样到输出采样率
        let need = (self.mono.len() as f64 * self.resampler.ratio()).ceil() as usize + 8;
        if self.resampled.len() < need {
            self.resampled.resize(need, 0.0);
        }
        let n = self.resampler.process(&self.mono, &mut self.resampled);

        // 4. 单声道 → 立体声，写入环形缓冲
        for &s in &self.resampled[..n] {
            if self.ring.slots() < 2 {
                self.stats.overruns.fetch_add(1, Ordering::Relaxed);
                break;
            }
            let _ = self.ring.push(s);
            let _ = self.ring.push(s);
        }
        // 推送之后记录水位 —— 这才是"缓冲区里现在有多少数据"
        self.stats.ring_fill.store(
            self.capacity.saturating_sub(self.ring.slots()),
            Ordering::Relaxed,
        );
        self.stats
            .captured_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }
}

// ─────────────────────────────────────────── 渲染侧

/// 渲染处理：取命令 → 读环形缓冲 → 混音 → 限幅 → 写输出。**实时线程。**
pub struct RenderProcessor {
    ring: Consumer<f32>,
    cmd: Consumer<Command>,
    out_channels: usize,
    frames_cap: usize,
    mic: Vec<f32>,
    sfx: Vec<f32>,
    players: Vec<FilePlayer>,
    limiter: Limiter,
    limiter_enabled: bool,
    ctl: Arc<PlaybackControl>,
    stats: Arc<EngineStats>,
    last_frame: [f32; 2],
    /// 连续 underrun 计数，用于平滑淡出而不是硬切
    silent_run: usize,
    /// 电平表内部状态（快攻慢放，模拟 VU 观感）
    meter_peak: f32,
    meter_rms: f32,
    sample_rate: u32,
}

impl RenderProcessor {
    /// **实时安全。** 必须把 `out` 全部填满。
    ///
    /// 内部混音**永远按立体声处理**，最后一步才映射到设备的实际声道数。
    /// 这样引擎的正确性与设备协商出的声道数彻底解耦 ——
    /// 之前内部缓冲按设备声道切片、而音效播放器硬编码按 2 声道迭代，
    /// 两者一旦不一致就会出现"文件被以错误速率消费"这类隐蔽故障。
    pub fn process(&mut self, out: &mut [f32]) {
        let dev_ch = self.out_channels.max(1);
        let frames = (out.len() / dev_ch).min(self.frames_cap);
        if frames == 0 {
            out.fill(0.0);
            return;
        }
        let used = frames * 2; // 内部立体声：交错 L/R

        // 0. 处理控制命令（无锁取出）
        while let Ok(cmd) = self.cmd.pop() {
            match cmd {
                Command::Play { slot } => {
                    if let Some(p) = self.players.get_mut(slot as usize) {
                        p.trigger();
                    }
                }
                Command::Stop { slot } => {
                    if let Some(p) = self.players.get_mut(slot as usize) {
                        p.stop();
                    }
                }
                Command::StopAll => {
                    for p in self.players.iter_mut() {
                        p.stop();
                    }
                }
                Command::SetGain { slot, db } => {
                    if let Some(p) = self.players.get_mut(slot as usize) {
                        p.gain.set(db);
                    }
                }
            }
        }

        // 1. 从环形缓冲取麦克风数据（立体声）
        let mic = &mut self.mic[..used];
        let mut underrun = false;
        for f in mic.chunks_exact_mut(2) {
            match (self.ring.pop(), self.ring.pop()) {
                (Ok(l), Ok(r)) => {
                    f[0] = l;
                    f[1] = r;
                    self.last_frame = [l, r];
                    self.silent_run = 0;
                }
                _ => {
                    // 缓冲被抽干：用上一帧顶上并计数，随后逐渐淡出，避免咔哒声
                    underrun = true;
                    self.silent_run += 1;
                    let decay = (1.0 - self.silent_run as f32 / 2048.0).max(0.0);
                    f[0] = self.last_frame[0] * decay;
                    f[1] = self.last_frame[1] * decay;
                }
            }
        }
        if underrun {
            self.stats.underruns.fetch_add(1, Ordering::Relaxed);
        }
        self.stats
            .ring_fill
            .store(self.ring.slots(), Ordering::Relaxed);

        // 2. 渲染所有音效槽位（累加）
        let sfx = &mut self.sfx[..used];
        sfx.fill(0.0);
        let mut any_active = false;
        let mut active_slot = -1i32;
        let mut playhead = 0u64;
        for (i, p) in self.players.iter_mut().enumerate() {
            p.render_add(sfx);
            if p.is_active() {
                any_active = true;
                if active_slot < 0 {
                    active_slot = i as i32;
                    playhead = (p.pos / 2) as u64;
                }
            }
        }
        self.stats.playing.store(any_active, Ordering::Relaxed);
        self.stats
            .active_slot
            .store(active_slot, Ordering::Relaxed);
        self.stats.playhead.store(playhead, Ordering::Relaxed);

        // 3. 混音（增益在实时线程读取，无锁）
        let mg = db_to_lin(self.ctl.mic_gain_db.get());
        let sg = db_to_lin(self.ctl.sfx_gain_db.get());
        for i in 0..used {
            mic[i] = mic[i] * mg + sfx[i] * sg;
        }

        // 4. 限幅 —— 保证永不削波
        if self.limiter_enabled {
            self.limiter.process_in_place(mic);
            self.stats.limiter_gain_db100.store(
                ((self.limiter.current_gain_db() + LIMITER_DB_OFFSET).clamp(0.0, 120.0) * 100.0)
                    as u32,
                Ordering::Relaxed,
            );
        }

        // 4.5 电平表 —— 量的是**限幅之后、真正送出去**的信号
        {
            let mut peak = 0f32;
            let mut sum_sq = 0f32;
            for &s in mic.iter() {
                let a = s.abs();
                if a > peak {
                    peak = a;
                }
                sum_sq += s * s;
            }
            let rms = (sum_sq / used.max(1) as f32).sqrt();
            let block_secs = frames as f32 / self.sample_rate.max(1) as f32;
            let attack = 1.0 - (-block_secs / 0.005).exp(); // 5ms 起
            let release = 1.0 - (-block_secs / 0.35).exp(); // 350ms 落
            let smooth = |cur: f32, new: f32| {
                let k = if new > cur { attack } else { release };
                cur + (new - cur) * k
            };
            self.meter_peak = smooth(self.meter_peak, peak);
            self.meter_rms = smooth(self.meter_rms, rms);
            self.stats
                .out_peak_db100
                .store(db_to_atomic(lin_to_db(self.meter_peak)), Ordering::Relaxed);
            self.stats
                .out_rms_db100
                .store(db_to_atomic(lin_to_db(self.meter_rms)), Ordering::Relaxed);
        }

        // 5. 映射到设备声道数（内部始终是立体声）
        for i in 0..frames {
            let l = mic[i * 2];
            let r = mic[i * 2 + 1];
            let base = i * dev_ch;
            match dev_ch {
                1 => out[base] = (l + r) * 0.5,
                2 => {
                    out[base] = l;
                    out[base + 1] = r;
                }
                n => {
                    out[base] = l;
                    out[base + 1] = r;
                    for c in 2..n {
                        out[base + c] = (l + r) * 0.5;
                    }
                }
            }
        }
        let dev_used = frames * dev_ch;
        if dev_used < out.len() {
            out[dev_used..].fill(0.0);
        }

        self.stats
            .rendered_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }
}

#[inline]
fn db_to_lin(db: f32) -> f32 {
    if db <= -60.0 {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

// ─────────────────────────────────────────── 组装

/// 引擎的两半：采集侧进输入回调，渲染侧进输出回调。
pub struct EngineParts {
    pub capture: CaptureProcessor,
    pub render: RenderProcessor,
    pub control: Arc<PlaybackControl>,
    pub stats: Arc<EngineStats>,
}

impl EngineParts {
    /// 组装引擎。
    ///
    /// * `input_format` —— 真实麦克风的实际格式
    /// * `output_format` —— 虚拟设备的格式（通常 48kHz / 2ch）
    /// * `slots` —— 音效槽位（已预解码并重采样到输出采样率）
    /// * `buffer_ms` —— 环形缓冲目标水位（毫秒），越大越稳、延迟越高
    pub fn build(
        input_format: StreamFormat,
        output_format: StreamFormat,
        slots: Vec<SfxSlot>,
        buffer_ms: u32,
    ) -> Self {
        // 水位按输出侧的样本数计（立体声 → ×2）
        let target_frames = (output_format.sample_rate as u64 * buffer_ms as u64 / 1000) as usize;
        let target_samples = target_frames * 2;
        // 环形缓冲给到目标水位的 4 倍以上，留足抖动余量；下限 16384 样本（约 170ms 立体声）
        let capacity = (target_samples * 4).next_power_of_two().max(16384);

        let (producer, consumer) = RingBuffer::<f32>::new(capacity);
        // 命令队列容量给足即可，控制事件频率极低
        let (cmd_tx, cmd_rx) = RingBuffer::<Command>::new(256);

        let stats = Arc::new(EngineStats {
            active_slot: AtomicI32::new(-1),
            ..Default::default()
        });
        let control = Arc::new(PlaybackControl {
            cmd: Mutex::new(cmd_tx),
            mic_gain_db: AtomicF32::new(0.0),
            sfx_gain_db: AtomicF32::new(-6.0),
            stats: stats.clone(),
            slot_count: slots.len(),
        });

        let base_ratio = output_format.sample_rate as f64 / input_format.sample_rate.max(1) as f64;

        let capture = CaptureProcessor {
            in_channels: input_format.ch(),
            mono: Vec::with_capacity(8192),
            resampled: vec![0.0; 32768],
            resampler: LinearResampler::new(base_ratio),
            drift: DriftController::new(base_ratio, target_samples),
            ring: producer,
            capacity,
            stats: stats.clone(),
        };

        // 输出回调的缓冲通常 ≤ 4096 帧，给到 8192 帧足够
        const FRAMES_CAP: usize = 8192;
        let players = slots
            .iter()
            .map(|s| FilePlayer::new(s, output_format.sample_rate))
            .collect();

        let render = RenderProcessor {
            ring: consumer,
            cmd: cmd_rx,
            out_channels: output_format.ch(),
            frames_cap: FRAMES_CAP,
            mic: vec![0.0; FRAMES_CAP * 2],
            sfx: vec![0.0; FRAMES_CAP * 2],
            players,
            limiter: Limiter::with_sample_rate(output_format.sample_rate),
            limiter_enabled: true,
            ctl: control.clone(),
            stats: stats.clone(),
            last_frame: [0.0, 0.0],
            silent_run: 0,
            meter_peak: 0.0,
            meter_rms: 0.0,
            sample_rate: output_format.sample_rate,
        };

        Self {
            capture,
            render,
            control,
            stats,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn 空槽位() -> Vec<SfxSlot> {
        Vec::new()
    }

    fn 构造槽位(data: Vec<f32>, looping: bool) -> SfxSlot {
        SfxSlot {
            name: "test".into(),
            samples: Arc::new(data),
            looping,
            gain_db: 0.0,
        }
    }

    fn 构造引擎() -> EngineParts {
        EngineParts::build(
            StreamFormat::new(48_000, 1),
            StreamFormat::new(48_000, 2),
            空槽位(),
            20,
        )
    }

    /// 渲染若干个缓冲区并返回观测到的峰值
    fn 渲染取峰值(e: &mut EngineParts, buffers: usize) -> f32 {
        let mut out = vec![0f32; 512];
        let mut peak = 0f32;
        for _ in 0..buffers {
            e.render.process(&mut out);
            peak = peak.max(out.iter().fold(0f32, |m, s| m.max(s.abs())));
        }
        peak
    }

    #[test]
    fn 静默引擎应输出静音而非噪音() {
        let mut e = 构造引擎();
        let mut out = vec![0f32; 512];
        e.render.process(&mut out);
        assert!(out.iter().all(|s| s.abs() < 1e-6), "空缓冲应输出静音");
    }

    #[test]
    fn 透传应保持信号形状() {
        let mut e = 构造引擎();
        let sr = 48_000f32;
        let input: Vec<f32> = (0..4800)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin() * 0.5)
            .collect();
        e.capture.process(&input);

        let mut out = vec![0f32; 2048];
        e.render.process(&mut out);
        let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.05, "透传峰值应约 0.5，实际 {peak}");
        for f in out.chunks_exact(2).take(100) {
            assert!((f[0] - f[1]).abs() < 1e-6, "左右声道应一致（单声道兼容）");
        }
    }

    #[test]
    fn 立体声输入应正确下混() {
        let mut e = EngineParts::build(
            StreamFormat::new(48_000, 2),
            StreamFormat::new(48_000, 2),
            空槽位(),
            20,
        );
        let mut input = vec![0f32; 4800];
        for f in input.chunks_exact_mut(2) {
            f[0] = 0.5;
        }
        e.capture.process(&input);
        let mut out = vec![0f32; 1024];
        e.render.process(&mut out);
        let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.25).abs() < 0.02, "下混峰值应约 0.25，实际 {peak}");
    }

    #[test]
    fn 音效触发与停止() {
        let mut e = EngineParts::build(
            StreamFormat::new(48_000, 1),
            StreamFormat::new(48_000, 2),
            vec![构造槽位(vec![0.8f32; 48000], false)],
            20,
        );
        let ctl = e.control.clone();
        ctl.sfx_gain_db.set(0.0); // 断言原始幅度，绕开默认 -6dB

        let mut out = vec![0f32; 512];
        e.render.process(&mut out);
        assert!(out.iter().all(|s| s.abs() < 1e-6), "未触发时不应有声");

        ctl.play(0);
        // 淡入需要若干帧，渲染几个缓冲区再取峰值
        let peak = 渲染取峰值(&mut e, 6);
        assert!((peak - 0.8).abs() < 0.05, "触发后峰值应≈0.8，实际 {peak}");

        ctl.stop(0);
        let _ = 渲染取峰值(&mut e, 6); // 淡出
        assert!(渲染取峰值(&mut e, 2) < 1e-6, "停止后不应有声");
    }

    #[test]
    fn 淡入淡出应消除起停突变() {
        // 用直流信号放大会暴露任何突变：淡变生效时首尾应平滑
        let mut e = EngineParts::build(
            StreamFormat::new(48_000, 1),
            StreamFormat::new(48_000, 2),
            vec![构造槽位(vec![0.9f32; 96000], true)],
            20,
        );
        let ctl = e.control.clone();
        ctl.sfx_gain_db.set(0.0); // 断言原始幅度，绕开默认 -6dB
        let mut out = vec![0f32; 512];

        ctl.play(0);
        e.render.process(&mut out);
        // 第一个样本必须接近 0（淡入起点），而不是直接跳到 0.9
        let first = out[0].abs();
        assert!(first < 0.1, "淡入起点应接近 0，实际 {first}");

        // 淡入完成后达到满幅
        let peak = 渲染取峰值(&mut e, 5);
        assert!(peak > 0.85, "淡入完成后应达满幅，实际 {peak}");

        // 停止时同样应从满幅平滑降到 0
        ctl.stop(0);
        e.render.process(&mut out);
        let after_stop_first = out[0].abs();
        assert!(
            after_stop_first > 0.5,
            "停止后的第一帧仍应接近满幅（正在淡出），实际 {after_stop_first}"
        );
        let _ = 渲染取峰值(&mut e, 6);
        assert!(渲染取峰值(&mut e, 2) < 1e-6, "淡出结束后应完全静音");
    }

    #[test]
    fn 多槽位应独立工作且可叠加() {
        let mut e = EngineParts::build(
            StreamFormat::new(48_000, 1),
            StreamFormat::new(48_000, 2),
            vec![
                构造槽位(vec![0.3f32; 48000], false),
                构造槽位(vec![0.4f32; 48000], false),
                构造槽位(vec![0.2f32; 48000], false),
            ],
            20,
        );
        let ctl = e.control.clone();
        ctl.sfx_gain_db.set(0.0); // 断言原始幅度，绕开默认 -6dB

        ctl.play(0);
        assert!((渲染取峰值(&mut e, 6) - 0.3).abs() < 0.05, "槽位 0 应独立可播");

        ctl.stop(0);
        let _ = 渲染取峰值(&mut e, 6);

        ctl.play(1);
        assert!((渲染取峰值(&mut e, 6) - 0.4).abs() < 0.05, "槽位 1 应独立可播");

        // 叠加：槽位 1 + 2 应约为 0.6
        ctl.play(2);
        let p = 渲染取峰值(&mut e, 6);
        assert!((p - 0.6).abs() < 0.08, "两路叠加应约 0.6，实际 {p}");

        // 全部停止
        ctl.stop_all();
        let _ = 渲染取峰值(&mut e, 8);
        assert!(渲染取峰值(&mut e, 2) < 1e-6, "stop_all 后应完全静音");
    }

    #[test]
    fn 槽位增益应可运行时调整() {
        let mut e = EngineParts::build(
            StreamFormat::new(48_000, 1),
            StreamFormat::new(48_000, 2),
            vec![构造槽位(vec![0.8f32; 96000], true)],
            20,
        );
        let ctl = e.control.clone();
        ctl.sfx_gain_db.set(0.0);
        ctl.play(0);
        let full = 渲染取峰值(&mut e, 6);

        // 调低 12 dB
        assert!(ctl.set_slot_gain(0, -12.0), "设置应被接受");
        let quiet = 渲染取峰值(&mut e, 6);
        let delta = 20.0 * (quiet / full).log10();
        assert!(
            (delta + 12.0).abs() < 1.5,
            "应衰减约 12dB，实测 {delta:.1}dB（{full:.3} → {quiet:.3}）"
        );

        // 调回去
        ctl.set_slot_gain(0, 0.0);
        let back = 渲染取峰值(&mut e, 6);
        assert!((back - full).abs() < 0.05, "调回 0dB 应恢复原幅度");

        // 越界不 panic
        assert!(!ctl.set_slot_gain(99, -6.0), "越界应返回 false");
    }

    #[test]
    fn 静音槽位不应发声() {
        let mut e = EngineParts::build(
            StreamFormat::new(48_000, 1),
            StreamFormat::new(48_000, 2),
            vec![构造槽位(vec![0.8f32; 96000], true)],
            20,
        );
        let ctl = e.control.clone();
        ctl.sfx_gain_db.set(0.0);
        ctl.set_slot_gain(0, -60.0); // -60dB 视为静音
        ctl.play(0);
        let p = 渲染取峰值(&mut e, 8);
        assert!(p < 1e-3, "增益 -60dB 应基本静音，实际峰值 {p}");
    }

    #[test]
    fn 电平表应反映真实输出电平() {
        // 直流 0.5（= -6 dBFS）经过透传，电平表应读到 -6 dBFS 附近
        let mut e = EngineParts::build(
            StreamFormat::new(48_000, 1),
            StreamFormat::new(48_000, 2),
            vec![],
            20,
        );
        e.control.mic_gain_db.set(0.0);
        // 喂 2 秒信号，保证渲染期间供给不断（否则后期欠载会让电平掉下来）
        let input = vec![0.5f32; 96_000];
        e.capture.process(&input);
        // 渲染 20 个缓冲区（约 0.2 秒）—— 足够让 5ms 起振的平滑器收敛
        let mut out = vec![0f32; 1024];
        for _ in 0..20 {
            e.render.process(&mut out);
        }
        let st = e.control.stats();
        assert!(
            (st.out_peak_db - (-6.02)).abs() < 1.5,
            "峰值应约 -6dBFS，实测 {:.2}",
            st.out_peak_db
        );
        assert!(
            (st.out_rms_db - (-6.02)).abs() < 1.5,
            "直流信号的 RMS 应也约 -6dBFS，实测 {:.2}",
            st.out_rms_db
        );
    }

    #[test]
    fn 静音时电平表应显示很低() {
        let mut e = 构造引擎();
        let mut out = vec![0f32; 1024];
        for _ in 0..40 {
            e.render.process(&mut out);
        }
        let st = e.control.stats();
        assert!(st.out_peak_db < -80.0, "静音时峰值应 < -80dBFS，实际 {}", st.out_peak_db);
    }

    #[test]
    fn 电平表量的是限幅之后的信号() {
        // 灌入远超 0dBFS 的信号：限幅把输出压到 ceiling 内，
        // 电平表读到的必须是**限幅后**的值，而不是原始过载值。
        let mut e = 构造引擎();
        let input: Vec<f32> = (0..96_000).map(|i| (i as f32 * 0.3).sin() * 5.0).collect();
        e.capture.process(&input);
        let mut out = vec![0f32; 1024];
        for _ in 0..20 {
            e.render.process(&mut out);
        }
        let st = e.control.stats();
        assert!(
            st.out_peak_db <= -0.5,
            "限幅上限是 -1dBFS，电平表不应读到更高：{:.2}",
            st.out_peak_db
        );
        assert!(st.out_peak_db > -6.0, "限幅后应接近 -1dBFS，实际 {:.2}", st.out_peak_db);
    }

    #[test]
    fn 越界槽位不应崩溃() {
        let mut e = EngineParts::build(
            StreamFormat::new(48_000, 1),
            StreamFormat::new(48_000, 2),
            vec![构造槽位(vec![0.5f32; 1000], false)],
            20,
        );
        let ctl = e.control.clone();
        assert!(!ctl.play(99), "越界播放应返回 false");
        assert!(!ctl.stop(99), "越界停止应返回 false");
        assert_eq!(ctl.slot_count(), 1);
        let _ = 渲染取峰值(&mut e, 2); // 不应 panic
    }

    #[test]
    fn 播放中重触发应先淡出再重启() {
        let mut e = EngineParts::build(
            StreamFormat::new(48_000, 1),
            StreamFormat::new(48_000, 2),
            vec![构造槽位(vec![0.7f32; 96000], true)],
            20,
        );
        let ctl = e.control.clone();
        ctl.sfx_gain_db.set(0.0); // 断言原始幅度，绕开默认 -6dB
        ctl.play(0);
        let _ = 渲染取峰值(&mut e, 6);
        let before = ctl.playhead_frames();

        ctl.play(0); // 重触发
        let _ = 渲染取峰值(&mut e, 8);
        let after = ctl.playhead_frames();
        assert!(after < before + 200, "重触发后播放位置应回到开头附近，before={before} after={after}");
        // 重触发后仍在正常发声
        assert!(渲染取峰值(&mut e, 6) > 0.5, "重触发后应继续发声");
    }

    #[test]
    fn 非循环音效播完应自动停止() {
        let mut e = EngineParts::build(
            StreamFormat::new(48_000, 1),
            StreamFormat::new(48_000, 2),
            vec![构造槽位(vec![0.6f32; 4800], false)], // 0.05 秒
            20,
        );
        let ctl = e.control.clone();
        ctl.play(0);
        // 渲染 0.5 秒足够播完
        let mut out = vec![0f32; 2048];
        for _ in 0..12 {
            e.render.process(&mut out);
        }
        assert!(!ctl.is_playing(), "非循环音效播完应自动停止");
        assert!(渲染取峰值(&mut e, 2) < 1e-6, "播完后应静音");
    }

    #[test]
    fn 过载信号也不削波() {
        let mut e = 构造引擎();
        let input: Vec<f32> = (0..9600).map(|i| (i as f32 * 0.1).sin() * 8.0).collect();
        e.capture.process(&input);
        let mut out = vec![0f32; 8192];
        e.render.process(&mut out);
        let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak <= 0.92, "限幅后峰值应 ≤ -1dBFS，实际 {peak}");
    }

    #[test]
    fn 音效叠加过载时限幅器应介入() {
        let mut e = EngineParts::build(
            StreamFormat::new(48_000, 1),
            StreamFormat::new(48_000, 2),
            vec![
                构造槽位(vec![0.9f32; 96000], true),
                构造槽位(vec![0.9f32; 96000], true),
            ],
            20,
        );
        let ctl = e.control.clone();
        ctl.sfx_gain_db.set(0.0);
        ctl.play(0);
        ctl.play(1);
        let _ = 渲染取峰值(&mut e, 8);
        let s = ctl.stats();
        assert!(s.limiter_gain_db < -0.5, "两路 0.9 叠加应触发限幅，实际增益 {}dB", s.limiter_gain_db);
        let peak = 渲染取峰值(&mut e, 4);
        assert!(peak <= 0.92, "限幅后峰值应受限，实际 {peak}");
    }

    #[test]
    fn 默认音效增益应为负六分贝() {
        let mut e = EngineParts::build(
            StreamFormat::new(48_000, 1),
            StreamFormat::new(48_000, 2),
            vec![构造槽位(vec![1.0f32; 48000], false)],
            20,
        );
        e.control.play(0);
        let peak = 渲染取峰值(&mut e, 6);
        // -6dB ≈ 0.501
        assert!((peak - 0.501).abs() < 0.03, "默认增益应约 -6dB，实际峰值 {peak}");
    }

    /// 播放音高必须与源文件一致 —— 用零交叉法独立测量。
    /// 这条测试能抓住"文件被以错误速率消费"这类问题。
    fn 测量频率(samples: &[f32], sr: u32) -> f32 {
        let zc = samples
            .windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count();
        zc as f32 / 2.0 / (samples.len() as f32 / sr as f32)
    }

    #[test]
    fn 播放音高必须与源一致() {
        let sr = 48_000u32;
        for freq in [220.0f32, 440.0, 880.0] {
            let frames = sr as usize; // 1 秒
            let mut samples = Vec::with_capacity(frames * 2);
            for i in 0..frames {
                let v = (2.0 * std::f32::consts::PI * freq * i as f32 / sr as f32).sin() * 0.5;
                samples.push(v);
                samples.push(v);
            }
            let mut e = EngineParts::build(
                StreamFormat::new(sr, 1),
                StreamFormat::new(sr, 2),
                vec![构造槽位(samples, true)],
                20,
            );
            e.control.sfx_gain_db.set(0.0);
            e.control.play(0);
            let _ = 渲染取峰值(&mut e, 4); // 跳过淡入

            let mut out = vec![0f32; 4096];
            let mut mono: Vec<f32> = Vec::new();
            for _ in 0..12 {
                e.render.process(&mut out);
                mono.extend(out.chunks_exact(2).map(|f| f[0]));
            }
            let measured = 测量频率(&mono, sr);
            assert!(
                (measured - freq).abs() < freq * 0.05,
                "源 {freq}Hz 播放后实测 {measured:.1}Hz —— 消费速率不对"
            );
        }
    }

    /// 设备协商出非立体声时，引擎必须仍然正确。
    ///
    /// 这条测试是为一个真实故障加的：渲染器原先按**设备声道数**切内部缓冲，
    /// 而音效播放器硬编码按 2 声道迭代；一旦设备协商成单声道，
    /// 两者不一致就会让文件被以错误速率消费（曾观察到 440Hz 播成 872Hz）。
    /// 现在内部永远是立体声，最后才映射到设备声道。
    #[test]
    fn 单声道输出设备也应保持音高正确() {
        let sr = 48_000u32;
        let freq = 440.0f32;
        let frames = sr as usize;
        let mut samples = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let v = (2.0 * std::f32::consts::PI * freq * i as f32 / sr as f32).sin() * 0.5;
            samples.push(v);
            samples.push(v);
        }
        for dev_ch in [1u16, 2, 4] {
            let mut e = EngineParts::build(
                StreamFormat::new(sr, 1),
                StreamFormat::new(sr, dev_ch),
                vec![构造槽位(samples.clone(), true)],
                20,
            );
            e.control.sfx_gain_db.set(0.0);
            e.control.mic_gain_db.set(-60.0);
            e.control.play(0);
            let _ = 渲染取峰值(&mut e, 4);

            // 按设备帧数喂缓冲，模拟真实回调
            let mut out = vec![0f32; 1024 * dev_ch as usize];
            let mut collected: Vec<f32> = Vec::new();
            for _ in 0..24 {
                e.render.process(&mut out);
                // 单声道设备直接取；多声道取第一声道
                collected.extend(out.chunks_exact(dev_ch as usize).map(|f| f[0]));
            }
            let measured = 测量频率(&collected, sr);
            assert!(
                (measured - freq).abs() < freq * 0.05,
                "{dev_ch} 声道设备上音高应为 {freq}Hz，实测 {measured:.1}Hz"
            );
        }
    }

    #[test]
    fn 不同采样率应自动转换() {
        let mut e = EngineParts::build(
            StreamFormat::new(44_100, 1),
            StreamFormat::new(48_000, 2),
            空槽位(),
            20,
        );
        let input = vec![0.3f32; 4410]; // 0.1 秒 @44.1k
        e.capture.process(&input);

        let fill = e.stats.ring_fill.load(Ordering::Relaxed);
        let frames = fill / 2;
        assert!(
            (4600..=5000).contains(&frames),
            "0.1s @44.1k 应转成约 4800 帧 @48k，实际 {frames} 帧"
        );
        assert_eq!(
            e.stats.overruns.load(Ordering::Relaxed),
            0,
            "不应发生缓冲溢出"
        );
    }
}
