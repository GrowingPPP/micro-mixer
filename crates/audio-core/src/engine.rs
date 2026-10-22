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
use crate::voice::{VoiceChain, VoicePhase, VoiceRuntime, VoiceSetup};

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
    Play { slot: usize },
    /// 淡出并停止某个槽位
    Stop { slot: usize },
    /// 停止全部
    StopAll,
    /// 调整某个槽位的音量（dB），**运行时立即生效**
    SetGain { slot: usize, db: f32 },
}

#[derive(Debug, Clone, Copy)]
struct QueuedCommand {
    epoch: u64,
    command: Command,
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
    /// 这是产品里最重要的一个指标：它直接回答"对方能不能听到我"。
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
    cmd: Mutex<Producer<QueuedCommand>>,
    /// 高优先级停止独立于有界队列，排满播放命令也不能阻止停止。
    stop_epoch: AtomicU64,
    /// 输出回调完成停止淡出和限幅器尾音后确认的代次。
    stopped_epoch: AtomicU64,
    /// 0 保留各槽位初值；1 关闭循环；2 开启循环。持久参数独立于命令队列。
    looping_override: AtomicU32,
    pub mic_gain_db: AtomicF32,
    pub sfx_gain_db: AtomicF32,
    stats: Arc<EngineStats>,
    slot_count: usize,
}

impl PlaybackControl {
    fn send(&self, cmd: Command) -> bool {
        match self.cmd.lock() {
            Ok(mut p) => p
                .push(QueuedCommand {
                    epoch: self.stop_epoch.load(Ordering::Acquire),
                    command: cmd,
                })
                .is_ok(),
            Err(_) => false,
        }
    }

    /// 从头播放指定槽位（对应"按下热键"）。
    pub fn play(&self, slot: usize) -> bool {
        self.play_with_barrier(slot, || {})
    }

    /// 接纳播放前执行控制线程屏障，例如关闭独立试听。
    /// 槽位无效或队列满时不会执行屏障；成功接纳时正好执行一次。
    /// 屏障持有生产者锁，不得重新调用该控制器的命令方法。
    pub fn play_with_barrier(&self, slot: usize, before_submit: impl FnOnce()) -> bool {
        self.play_with_checked_barrier(slot, || {
            before_submit();
            true
        })
    }

    /// 屏障结束时仍可拒绝过期请求；返回 false 不提交任何播放命令。
    /// 有效槽位和容量先检查。屏障持有生产者锁，不得再次调用命令方法。
    pub fn play_with_checked_barrier(
        &self,
        slot: usize,
        before_submit: impl FnOnce() -> bool,
    ) -> bool {
        if slot >= self.slot_count {
            return false;
        }
        let Ok(mut queue) = self.cmd.lock() else {
            return false;
        };
        if queue.is_full() {
            return false;
        }
        if !before_submit() {
            return false;
        }
        queue
            .push(QueuedCommand {
                epoch: self.stop_epoch.load(Ordering::Acquire),
                command: Command::Play { slot },
            })
            .is_ok()
    }

    /// 同步向主输出和监听提交单音效选择。任何一路背压都拒绝整个请求。
    ///
    /// 两个生产者只在控制线程加锁；固定地址顺序避免反向调用时产生锁循环。
    pub fn play_linked(&self, slot: usize, monitor: &Self) -> bool {
        self.play_linked_with_barrier(slot, monitor, || {})
    }

    /// 两路都能接纳请求后、提交任何一路之前执行一次控制线程屏障。
    /// 屏障持有两路生产者锁，不得重新调用这两个控制器的命令方法。
    pub fn play_linked_with_barrier(
        &self,
        slot: usize,
        monitor: &Self,
        before_submit: impl FnOnce(),
    ) -> bool {
        self.play_linked_with_checked_barrier(slot, monitor, || {
            before_submit();
            true
        })
    }

    /// 两路容量确认后执行可取消屏障；返回 false 时两路都不提交。
    /// 屏障持有两个生产者锁，不得重新调用这两个控制器的命令方法。
    pub fn play_linked_with_checked_barrier(
        &self,
        slot: usize,
        monitor: &Self,
        before_submit: impl FnOnce() -> bool,
    ) -> bool {
        if slot >= self.slot_count || slot >= monitor.slot_count {
            return false;
        }
        if std::ptr::eq(self, monitor) {
            return self.play_with_checked_barrier(slot, before_submit);
        }
        let (first, second) =
            if std::ptr::from_ref(self).addr() < std::ptr::from_ref(monitor).addr() {
                (self, monitor)
            } else {
                (monitor, self)
            };
        let (Ok(mut first_queue), Ok(mut second_queue)) = (first.cmd.lock(), second.cmd.lock())
        else {
            return false;
        };
        if first_queue.is_full() || second_queue.is_full() {
            return false;
        }
        if !before_submit() {
            return false;
        }
        // 持有两路生产者锁后，消费者只能增加空位，所以下面两次 push 都能成功。
        let first_ok = first_queue
            .push(QueuedCommand {
                epoch: first.stop_epoch.load(Ordering::Acquire),
                command: Command::Play { slot },
            })
            .is_ok();
        let second_ok = second_queue
            .push(QueuedCommand {
                epoch: second.stop_epoch.load(Ordering::Acquire),
                command: Command::Play { slot },
            })
            .is_ok();
        first_ok && second_ok
    }

    /// 淡出并停止指定槽位。
    pub fn stop(&self, slot: usize) -> bool {
        if slot >= self.slot_count {
            return false;
        }
        self.send(Command::Stop { slot })
    }

    /// 停止全部音效（**人声照常透传**）。
    pub fn stop_all(&self) -> bool {
        // 与 send 串行，停止之前/之后的播放命令都有明确的代次。
        let _guard = self.cmd.lock().unwrap_or_else(|e| e.into_inner());
        self.stop_epoch.fetch_add(1, Ordering::Release);
        true
    }

    /// 调整某个槽位的音量（dB）。**下一帧就生效**，不必重播。
    pub fn set_slot_gain(&self, slot: usize, db: f32) -> bool {
        if slot >= self.slot_count {
            return false;
        }
        self.send(Command::SetGain { slot, db })
    }

    /// 运行时更新全部音效的循环模式，不重启当前音效，也不受命令队列背压影响。
    pub fn set_looping(&self, looping: bool) {
        self.looping_override
            .store(if looping { 2 } else { 1 }, Ordering::Release);
    }

    pub fn slot_count(&self) -> usize {
        self.slot_count
    }

    pub fn is_playing(&self) -> bool {
        self.stats.playing.load(Ordering::Relaxed)
    }

    /// 最近一次 stop_all 已经由输出回调完成，含淡出和固定限幅延迟。
    /// 在提交 stop_all 后查询，避免尚未消费的播放命令造成假静音。
    pub fn stop_all_finished(&self) -> bool {
        self.stopped_epoch.load(Ordering::Acquire) == self.stop_epoch.load(Ordering::Acquire)
            && !self.is_playing()
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
    /// 每帧的淡变步进
    fade_step: f32,
    /// 结尾淡出长度（帧）
    end_fade_frames: usize,
    /// 循环尾部与头部重叠，避免每圈淡出到零又硬跳到头部。
    loop_crossfade_frames: usize,
}

impl FilePlayer {
    fn new(slot: &SfxSlot, sample_rate: u32) -> Self {
        let fade_frames = (sample_rate as f32 * FADE_MS / 1000.0).max(1.0);
        // 预计算潜在循环边界，初始单次播放也能运行时开启平滑循环。
        // 原本连续的循环保持原时长；只有首尾不连续时才重叠尾部和头部。
        let needs_loop_crossfade = slot.frames() >= 2
            && (0..2).any(|c| {
                let last = slot.frames() * 2 - 2 + c;
                let head_slope = slot.samples[2 + c] - slot.samples[c];
                let tail_slope = slot.samples[last] - slot.samples[last - 2];
                let join_slope = slot.samples[c] - slot.samples[last];
                (join_slope - (head_slope + tail_slope) * 0.5).abs() > 0.001
            });
        Self {
            samples: slot.samples.clone(),
            pos: 0,
            looping: slot.looping,
            gain: AtomicF32::new(slot.gain_db),
            active: false,
            fade: 0.0,
            fade_dir: 0.0,
            fade_step: 1.0 / fade_frames,
            end_fade_frames: fade_frames as usize,
            loop_crossfade_frames: if needs_loop_crossfade {
                (fade_frames as usize).min(slot.frames() / 4)
            } else {
                0
            },
        }
    }

    /// 立即从头开始（含淡入）。
    fn start(&mut self) {
        self.pos = 0;
        self.active = true;
        self.fade = 0.0;
        self.fade_dir = 1.0;
    }

    /// 请求停止（淡出）。
    fn stop(&mut self) {
        if self.active {
            self.fade_dir = -1.0;
        }
    }

    fn is_active(&self) -> bool {
        self.active
    }

    /// **实时安全**：把本槽位的输出写入 `out`（交错立体声）。
    /// 返回占用的帧数，停止后的余下帧可以接着播放下一个音效。
    #[inline]
    fn render_add(&mut self, out: &mut [f32]) -> usize {
        if !self.active {
            return 0;
        }
        let total = self.samples.len();
        if total < 2 {
            self.active = false;
            return 0;
        }
        let g = db_to_lin(self.gain.get());

        for (frame_index, frame) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
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
                    return frame_index + 1;
                }
            }

            // 到达结尾
            if self.pos + 2 > total {
                if self.looping {
                    // 头部已在尾部交叉淡变中播放过，继续从其后读取。
                    self.pos = self.loop_crossfade_frames * 2;
                } else {
                    self.active = false;
                    return frame_index;
                }
            }

            // 结尾淡出：避免文件在波形中段突然结束产生咔哒
            let remaining = (total - self.pos) / 2;
            let end_env = if !self.looping && remaining < self.end_fade_frames {
                remaining as f32 / self.end_fade_frames as f32
            } else {
                1.0
            };

            let a = self.fade * end_env * g;
            if self.looping
                && self.loop_crossfade_frames > 0
                && remaining <= self.loop_crossfade_frames
            {
                let head_frame = self.loop_crossfade_frames - remaining;
                let blend = (head_frame + 1) as f32 / self.loop_crossfade_frames as f32;
                for (c, out_sample) in frame.iter_mut().enumerate() {
                    let tail = self.samples[self.pos + c];
                    let head = self.samples[head_frame * 2 + c];
                    *out_sample += (tail + (head - tail) * blend) * a;
                }
            } else {
                frame[0] += self.samples[self.pos] * a;
                frame[1] += self.samples[self.pos + 1] * a;
            }
            self.pos += 2;
        }
        out.len() / 2
    }
}

// ─────────────────────────────────────────── 采集侧

/// 干湿交叉淡化时长（毫秒）。
const VOICE_CROSSFADE_MS: f32 = 25.0;

/// 变声算法块时长上限（毫秒），防止异常配置造成超大预分配。
const MAX_VOICE_BLOCK_MS: f32 = 40.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoiceTeeState {
    Idle,
    Warming,
    FadingIn,
    SteadyWet,
    FadingOut,
}

/// 把变声链标记为可随引擎移动到音频线程。
///
/// [`VoiceChain`] 的既有接口接收未标注 `Send` 的 trait object；进入引擎的
/// 链都来自 [`VoiceSetup`]，其后端类型要求 `Send`。链在构建线程创建，随后
/// 随采集处理移动到唯一的音频线程并独占使用，不存在跨线程共享。
struct SendVoiceChain(VoiceChain);

// SAFETY: 构造入口 VoiceSetup 已要求 `Box<dyn VoiceBackend + Send>`，
// VoiceChain 中擦除的只是 auto trait 标记；链不会被跨线程共享。
unsafe impl Send for SendVoiceChain {}

/// 采集侧 tee：干声持续写环，湿声在独立链中预热，切换时交叉淡化。
///
/// 预热只把湿声排进链内 FIFO 后丢弃，写环始终保持干声，因此不会抽干环形缓冲。
/// 湿声累计产量达到算法延迟后才切换，初始延迟线中的静音不会写进环。
/// 所有状态由 [`VoiceRuntime`] 的阶段与代次驱动；旧代次的完成结果不会生效。
struct VoiceTee {
    runtime: Arc<VoiceRuntime>,
    chain: SendVoiceChain,
    state: VoiceTeeState,
    generation: u64,
    warmup_generation: u64,
    warmup_frames: usize,
    fade_generation: u64,
    fade_pos: usize,
    crossfade_frames: usize,
}

impl VoiceTee {
    fn new(setup: VoiceSetup, block_frames: usize, sample_rate: u32) -> Self {
        let VoiceSetup {
            params,
            runtime,
            backend,
            ..
        } = setup;
        let mut chain = SendVoiceChain(VoiceChain::new(backend, block_frames));
        chain.0.set_params(params);

        let (phase, generation) = runtime.snapshot();
        let state = match phase {
            VoicePhase::Off => VoiceTeeState::Idle,
            VoicePhase::Warmup => VoiceTeeState::Warming,
            VoicePhase::Wet => VoiceTeeState::SteadyWet,
            VoicePhase::FadingOut => VoiceTeeState::FadingOut,
        };
        let crossfade_frames =
            ((sample_rate as f32 * VOICE_CROSSFADE_MS / 1000.0).round() as usize).max(1);
        let fade_pos = if state == VoiceTeeState::SteadyWet {
            crossfade_frames
        } else {
            0
        };

        Self {
            runtime,
            chain,
            state,
            generation,
            warmup_generation: generation,
            warmup_frames: 0,
            fade_generation: generation,
            fade_pos,
            crossfade_frames,
        }
    }

    /// 消费一块干声，把当前应写环的样本写入 `out`，返回帧数。
    ///
    /// `out` 由采集侧预分配，长度至少为输入帧数加一个算法块。
    fn process(&mut self, dry: &[f32], out: &mut [f32]) -> usize {
        let frames = dry.len();
        let (phase, generation) = self.runtime.snapshot();
        self.sync(phase, generation);

        match phase {
            VoicePhase::Off => {
                out[..frames].copy_from_slice(dry);
                frames
            }
            VoicePhase::Warmup => {
                let produced = self.chain.0.process(dry, out);
                self.warmup_frames = self.warmup_frames.saturating_add(produced);
                // 预热期写环的始终是干声，湿声只用于填充链内 FIFO。
                out[..frames].copy_from_slice(dry);
                if self.warmup_frames >= self.chain.0.latency_frames()
                    && self.runtime.finish_warmup(self.warmup_generation)
                {
                    self.state = VoiceTeeState::FadingIn;
                    self.fade_pos = 0;
                }
                frames
            }
            VoicePhase::Wet => {
                // 每次最多排出本回调输入帧数，块量化积压留在链内 FIFO，
                // 避免小回调时向环形缓冲突发整块数据。
                let wet_frames = self.chain.0.process(dry, &mut out[..frames]);
                if self.state == VoiceTeeState::FadingIn {
                    self.mix_fade(out, dry, wet_frames, frames, true);
                    if self.fade_pos >= self.crossfade_frames {
                        self.state = VoiceTeeState::SteadyWet;
                    }
                    frames
                } else {
                    self.state = VoiceTeeState::SteadyWet;
                    wet_frames
                }
            }
            VoicePhase::FadingOut => {
                // 淡出同样按输入帧数排出，缺湿声时用干声补齐，
                // 保证写环速率与设备回调一致。
                let wet_frames = self.chain.0.process(dry, &mut out[..frames]);
                self.mix_fade(out, dry, wet_frames, frames, false);
                if self.fade_pos >= self.crossfade_frames
                    && self.runtime.finish_fade_out(self.fade_generation)
                {
                    self.chain.0.reset();
                    self.state = VoiceTeeState::Idle;
                    self.warmup_frames = 0;
                    self.fade_pos = 0;
                }
                frames
            }
        }
    }

    /// 按运行时代次同步本地状态；同代次内的阶段推进由本状态机完成。
    fn sync(&mut self, phase: VoicePhase, generation: u64) {
        if generation == self.generation {
            return;
        }
        self.generation = generation;
        match phase {
            VoicePhase::Off => {
                self.chain.0.reset();
                self.state = VoiceTeeState::Idle;
                self.warmup_frames = 0;
                self.fade_pos = 0;
            }
            VoicePhase::Warmup => {
                self.chain.0.reset();
                self.state = VoiceTeeState::Warming;
                self.warmup_generation = generation;
                self.warmup_frames = 0;
                self.fade_pos = 0;
            }
            VoicePhase::Wet => {
                self.state = VoiceTeeState::SteadyWet;
                self.fade_pos = self.crossfade_frames;
            }
            VoicePhase::FadingOut => {
                if self.state == VoiceTeeState::FadingIn {
                    // 反向切换时沿用当前干湿比例，避免淡入中途关闭产生突变。
                    self.fade_pos = self.crossfade_frames.saturating_sub(self.fade_pos);
                } else if self.state != VoiceTeeState::FadingOut {
                    self.fade_pos = 0;
                }
                self.state = VoiceTeeState::FadingOut;
                self.fade_generation = generation;
            }
        }
    }

    fn mix_fade(
        &mut self,
        out: &mut [f32],
        dry: &[f32],
        wet_frames: usize,
        frames: usize,
        fade_in: bool,
    ) {
        let fade = self.crossfade_frames.max(1) as f32;
        for (index, slot) in out.iter_mut().enumerate().take(frames) {
            let progress = self.fade_pos.min(self.crossfade_frames) as f32 / fade;
            let dry_weight = if fade_in { 1.0 - progress } else { progress };
            let has_wet = index < wet_frames;
            let wet = if has_wet { *slot } else { 0.0 };
            *slot = match (dry.get(index).copied(), has_wet) {
                (Some(d), true) => wet + (d - wet) * dry_weight,
                (Some(d), false) => d,
                (None, true) => wet,
                (None, false) => 0.0,
            };
            if self.fade_pos < self.crossfade_frames {
                self.fade_pos += 1;
            }
        }
    }
}

/// 采集处理：下混 → 漂移控制重采样 → 可选变声 tee → 写入环形缓冲。**实时线程。**
pub struct CaptureProcessor {
    in_channels: usize,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    resampler: LinearResampler,
    drift: DriftController,
    ring: Producer<f32>,
    capacity: usize,
    stats: Arc<EngineStats>,
    voice: Option<VoiceTee>,
    /// 变声 tee 的干/湿输出暂存；无变声时为空。
    voice_out: Vec<f32>,
}

impl CaptureProcessor {
    /// **实时安全。** `data` 是与设备格式一致的交错 f32。
    pub fn process(&mut self, data: &[f32]) {
        let ch = self.in_channels;
        let frames = data.len() / ch;
        if frames == 0 {
            return;
        }

        // 1. 时钟漂移控制：按缓冲水位微调重采样比率
        //    （读的是推送前的水位 —— 控制环要看的是"当前积压了多少"）
        let fill_before = self.capacity.saturating_sub(self.ring.slots());
        self.resampler.set_ratio(self.drift.ratio_for(fill_before));

        // 2. 固定缓冲分块下混与重采样。任意设备回调大小都不会触发 Vec 增容。
        let inv = 1.0 / ch as f32;
        let mut overrun = false;
        for chunk in data[..frames * ch].chunks(self.mono.len() * ch) {
            let chunk_frames = chunk.len() / ch;
            for (mono, frame) in self.mono[..chunk_frames]
                .iter_mut()
                .zip(chunk.chunks_exact(ch))
            {
                *mono = frame.iter().sum::<f32>() * inv;
            }
            let n = self
                .resampler
                .process(&self.mono[..chunk_frames], &mut self.resampled);
            // 3. 变声关闭时直接写干声；启用后由 tee 选择干声或湿声。
            let output: &[f32] = if let Some(voice) = self.voice.as_mut() {
                let written = voice.process(&self.resampled[..n], &mut self.voice_out);
                &self.voice_out[..written]
            } else {
                &self.resampled[..n]
            };
            // 4. 单声道 → 立体声。缓冲满时丢弃本块余下输出，但仍消耗全部输入。
            for &s in output {
                if self.ring.slots() < 2 {
                    overrun = true;
                    break;
                }
                let _ = self.ring.push(s);
                let _ = self.ring.push(s);
            }
        }
        if overrun {
            self.stats.overruns.fetch_add(1, Ordering::Relaxed);
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
/// 自测钩子：`MIXER_PANIC_AFTER=<秒>` 让渲染回调在指定秒数后**故意 panic 一次**。
///
/// 为什么要有它：panic 兜底 + 看门狗这套东西，不真的诱发一次就没法说它有效 ——
/// 而"看起来做了"正是本项目反复踩的坑。正常运行时只是两次原子读，可忽略。
/// 用一个哨兵值表示"没开"。
static PANIC_AFTER_SECS: AtomicU64 = AtomicU64::new(u64::MAX);
static PANIC_DONE: AtomicBool = AtomicBool::new(false);

/// 按环境变量启用 panic 自测钩子（启动时调一次即可）。
pub fn arm_panic_hook_from_env() {
    if let Ok(v) = std::env::var("MIXER_PANIC_AFTER") {
        if let Ok(secs) = v.parse::<u64>() {
            PANIC_AFTER_SECS.store(secs, Ordering::Relaxed);
            println!("[自测] 已武装 panic 钩子：渲染回调将在 {secs} 秒后故意 panic 一次");
        }
    }
}

/// 程序化武装自测钩子（供冒烟测试用，不依赖环境变量）。
pub fn arm_panic_hook_for_test(after_secs: u64) {
    PANIC_AFTER_SECS.store(after_secs, Ordering::Relaxed);
    PANIC_DONE.store(false, Ordering::Relaxed);
}

/// 自测钩子是否已经触发过。
pub fn panic_hook_fired() -> bool {
    PANIC_DONE.load(Ordering::Relaxed)
}

/// 变声湿声路径的限幅上限（dBFS）：人声与音效在相加前各自独立限幅。
const PATH_LIMITER_CEILING_DB: f32 = -3.0;

pub struct RenderProcessor {
    ring: Consumer<f32>,
    cmd: Consumer<QueuedCommand>,
    stop_epoch: u64,
    out_channels: usize,
    frames_cap: usize,
    mic: Vec<f32>,
    sfx: Vec<f32>,
    players: Vec<FilePlayer>,
    /// 只有该播放器可以写入 PCM；下一次触发覆盖 pending，而不会叠加。
    active_player: Option<usize>,
    pending_player: Option<usize>,
    applied_looping_override: u32,
    /// 没有音效写入的连续帧数，用于确认限幅器内旧音效已经输出完毕。
    sfx_idle_frames: usize,
    /// 湿声路径的人声限幅；仅存在变声会话时预分配，回调内不创建。
    voice_limiter: Option<Limiter>,
    /// 湿声路径的音效限幅；仅存在变声会话时预分配，回调内不创建。
    sfx_limiter: Option<Limiter>,
    /// 混音后的末端安全限幅；关闭变声时也是唯一一级。
    safety_limiter: Limiter,
    ctl: Arc<PlaybackControl>,
    stats: Arc<EngineStats>,
    last_frame: [f32; 2],
    /// 连续 underrun 计数，用于平滑淡出而不是硬切
    silent_run: usize,
    /// 电平表内部状态（快攻慢放，模拟 VU 观感）
    meter_peak: f32,
    meter_rms: f32,
    sample_rate: u32,
    /// 渲染侧在块边界读取变声阶段，供路径限幅选择处理链路。
    voice_runtime: Option<Arc<VoiceRuntime>>,
    voice_phase: VoicePhase,
}

impl RenderProcessor {
    /// 最近一次渲染块开始时的变声阶段；无变声时恒为 [`VoicePhase::Off`]。
    pub fn voice_phase(&self) -> VoicePhase {
        self.voice_phase
    }

    /// **实时安全。** 必须把 `out` 全部填满。
    ///
    /// 内部混音**永远按立体声处理**，最后一步才映射到设备的实际声道数。
    /// 这样引擎的正确性与设备协商出的声道数彻底解耦 ——
    /// 之前内部缓冲按设备声道切片、而音效播放器硬编码按 2 声道迭代，
    /// 两者一旦不一致就会出现"文件被以错误速率消费"这类隐蔽故障。
    pub fn process(&mut self, out: &mut [f32]) {
        // ── 自测钩子（正常路径上只是两次原子读）──
        if !PANIC_DONE.load(Ordering::Relaxed) {
            let after = PANIC_AFTER_SECS.load(Ordering::Relaxed);
            if after != u64::MAX {
                let secs = self.stats.rendered_frames.load(Ordering::Relaxed) as f64
                    / self.sample_rate.max(1) as f64;
                if secs > after as f64 {
                    PANIC_DONE.store(true, Ordering::Relaxed);
                    panic!("自测：故意在音频渲染回调里 panic（MIXER_PANIC_AFTER={after}）");
                }
            }
        }

        let dev_ch = self.out_channels.max(1);
        let frames = out.len() / dev_ch;
        if frames == 0 {
            out.fill(0.0);
            return;
        }
        // 0. 处理控制命令（无锁取出）
        self.apply_stop_epoch();
        while let Ok(queued) = self.cmd.pop() {
            self.apply_stop_epoch();
            // 增益是持久参数，停止不能取消参数更新；旧播放/停止请求一律失效。
            if queued.epoch != self.stop_epoch && !matches!(queued.command, Command::SetGain { .. })
            {
                continue;
            }
            match queued.command {
                Command::Play { slot } => {
                    if slot < self.players.len() {
                        self.pending_player = Some(slot);
                        if let Some(active) = self.active_player {
                            self.players[active].stop();
                        }
                    }
                }
                Command::Stop { slot } => {
                    if self.pending_player == Some(slot) {
                        self.pending_player = None;
                    }
                    if self.active_player == Some(slot) {
                        self.players[slot].stop();
                    }
                }
                Command::StopAll => {
                    self.stop_players();
                }
                Command::SetGain { slot, db } => {
                    if let Some(p) = self.players.get_mut(slot) {
                        p.gain.set(db);
                    }
                }
            }
        }

        // 设备可能提交超过预分配容量的回调；逐块处理全部帧，容量只决定内部块大小。
        for chunk in out[..frames * dev_ch].chunks_mut(self.frames_cap * dev_ch) {
            self.process_block(chunk);
        }
        out[frames * dev_ch..].fill(0.0);
    }

    fn apply_stop_epoch(&mut self) {
        let epoch = self.ctl.stop_epoch.load(Ordering::Acquire);
        if epoch != self.stop_epoch {
            self.stop_players();
            self.stop_epoch = epoch;
        }
    }

    fn stop_players(&mut self) {
        self.pending_player = None;
        if let Some(active) = self.active_player {
            self.players[active].stop();
        }
    }

    fn render_sfx(&mut self, used: usize) {
        let looping_override = self.ctl.looping_override.load(Ordering::Acquire);
        if looping_override != self.applied_looping_override {
            for player in &mut self.players {
                player.looping = looping_override == 2;
            }
            self.applied_looping_override = looping_override;
        }
        self.sfx[..used].fill(0.0);
        let mut offset = 0;
        while offset < used {
            if self.active_player.is_none() {
                if let Some(next) = self.pending_player.take() {
                    self.players[next].start();
                    self.active_player = Some(next);
                } else {
                    self.sfx_idle_frames = self.sfx_idle_frames.saturating_add((used - offset) / 2);
                    break;
                }
            }
            let active = self.active_player.unwrap();
            let player = &mut self.players[active];
            self.sfx_idle_frames = 0;
            offset += player.render_add(&mut self.sfx[offset..used]) * 2;
            if player.is_active() {
                break;
            }
            self.active_player = None;
        }
        // 切换淡出期间 UI 立即显示最后接受的选择，进度从新音效的开头计算。
        let selected = self.pending_player.or(self.active_player);
        self.stats
            .playing
            .store(selected.is_some(), Ordering::Relaxed);
        self.stats
            .active_slot
            .store(selected.map_or(-1, |s| s as i32), Ordering::Relaxed);
        self.stats.playhead.store(
            if self.pending_player.is_some() {
                0
            } else {
                selected.map_or(0, |s| (self.players[s].pos / 2) as u64)
            },
            Ordering::Relaxed,
        );
    }

    fn process_block(&mut self, out: &mut [f32]) {
        self.apply_stop_epoch();
        let phase = self
            .voice_runtime
            .as_ref()
            .map_or(VoicePhase::Off, |runtime| runtime.phase());
        if phase != self.voice_phase
            && (phase == VoicePhase::Wet || self.voice_phase == VoicePhase::Wet)
        {
            // 进出湿声链路时清空路径限幅的延迟线，避免旧会话尾音被新会话重新输出。
            if let Some(limiter) = self.voice_limiter.as_mut() {
                limiter.reset();
            }
            if let Some(limiter) = self.sfx_limiter.as_mut() {
                limiter.reset();
            }
        }
        self.voice_phase = phase;
        let dev_ch = self.out_channels.max(1);
        let frames = out.len() / dev_ch;
        let used = frames * 2; // 内部立体声：交错 L/R

        // 1. 从环形缓冲取麦克风数据（立体声）
        self.render_sfx(used);
        let mic = &mut self.mic[..used];
        let mut underrun = false;
        for f in mic.as_chunks_mut::<2>().0 {
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

        // 2. 单个音效已经按顺序完成淡出/淡入，不同槽位不会同时写入同一帧。
        let sfx = &mut self.sfx[..used];

        // 3. 混音（增益在实时线程读取，无锁）
        let mg = db_to_lin(self.ctl.mic_gain_db.get());
        let sg = db_to_lin(self.ctl.sfx_gain_db.get());
        let path_active = self.voice_phase == VoicePhase::Wet;
        if path_active {
            // 湿声链路：人声与音效各自限幅后再相加，两路增益互不压制。
            for sample in mic.iter_mut() {
                *sample *= mg;
            }
            for sample in sfx.iter_mut() {
                *sample *= sg;
            }
            if let (Some(voice_limiter), Some(sfx_limiter)) =
                (self.voice_limiter.as_mut(), self.sfx_limiter.as_mut())
            {
                voice_limiter.process_in_place(mic);
                sfx_limiter.process_in_place(sfx);
            }
            for i in 0..used {
                mic[i] += sfx[i];
            }
        } else {
            // 干声链路：沿用混音后单限幅，关闭变声时与既有引擎逐样本一致。
            for i in 0..used {
                mic[i] = mic[i] * mg + sfx[i] * sg;
            }
        }

        // 4. 末端安全限幅 —— 保证送往虚拟麦克风的信号永不削波
        self.safety_limiter.process_in_place(mic);
        self.stats.limiter_gain_db100.store(
            ((self.safety_limiter.current_gain_db() + LIMITER_DB_OFFSET).clamp(0.0, 120.0) * 100.0)
                as u32,
            Ordering::Relaxed,
        );
        // 停止确认要等音效走完当前链路：湿声还多一级音效路径限幅；
        // 人声不参与音效停止判定。
        let tail_frames = self.safety_limiter.latency_frames()
            + if path_active {
                self.sfx_limiter
                    .as_ref()
                    .map_or(0, |limiter| limiter.latency_frames())
            } else {
                0
            };
        if self.active_player.is_none()
            && self.pending_player.is_none()
            && self.sfx_idle_frames >= tail_frames
        {
            self.ctl
                .stopped_epoch
                .store(self.stop_epoch, Ordering::Release);
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
    ///
    /// 最终限幅另有固定 2ms 前视延迟（向上取整到输出采样率的完整帧）。
    pub fn build(
        input_format: StreamFormat,
        output_format: StreamFormat,
        slots: Vec<SfxSlot>,
        buffer_ms: u32,
    ) -> Self {
        Self::build_with_voice(input_format, output_format, slots, buffer_ms, None)
    }

    /// 组装带可选变声 tee 的引擎。
    ///
    /// `voice` 为 `Some` 时，采集侧在重采样之后分出干声与湿声；`None` 时
    /// 与 [`EngineParts::build`] 的行为完全一致。变声块时长按输出采样率解释，
    /// 必须是 `(0, 40]` 毫秒内的有限值。
    pub fn build_with_voice(
        input_format: StreamFormat,
        output_format: StreamFormat,
        slots: Vec<SfxSlot>,
        buffer_ms: u32,
        voice: Option<VoiceSetup>,
    ) -> Self {
        if let Some(setup) = &voice {
            assert!(
                setup.block_ms.is_finite()
                    && setup.block_ms > 0.0
                    && setup.block_ms <= MAX_VOICE_BLOCK_MS,
                "变声块时长必须为 (0, 40] ms 内的有限值，实际 {}",
                setup.block_ms
            );
        }
        // 水位按输出侧的样本数计（立体声 → ×2）
        let target_frames = (output_format.sample_rate as u64 * buffer_ms as u64 / 1000) as usize;
        let target_samples = target_frames * 2;
        // 环形缓冲给到目标水位的 4 倍以上，留足抖动余量；下限 16384 样本（约 170ms 立体声）
        let capacity = (target_samples * 4).next_power_of_two().max(16384);

        let (producer, consumer) = RingBuffer::<f32>::new(capacity);
        // 命令队列容量给足即可，控制事件频率极低
        let (cmd_tx, cmd_rx) = RingBuffer::<QueuedCommand>::new(256);

        let stats = Arc::new(EngineStats {
            active_slot: AtomicI32::new(-1),
            ..Default::default()
        });
        let control = Arc::new(PlaybackControl {
            cmd: Mutex::new(cmd_tx),
            stop_epoch: AtomicU64::new(0),
            stopped_epoch: AtomicU64::new(0),
            looping_override: AtomicU32::new(0),
            mic_gain_db: AtomicF32::new(0.0),
            sfx_gain_db: AtomicF32::new(-6.0),
            stats: stats.clone(),
            slot_count: slots.len(),
        });

        let base_ratio = output_format.sample_rate as f64 / input_format.sample_rate.max(1) as f64;

        let max_capture_ratio = base_ratio * 1.02;
        let capture_chunk_frames = ((8192.0 / max_capture_ratio).floor() as usize).clamp(1, 1024);
        let capture_output_samples =
            ((capture_chunk_frames + 1) as f64 * max_capture_ratio).ceil() as usize + 8;
        let (voice_tee, voice_out) = match voice {
            Some(setup) => {
                let block_frames = ((output_format.sample_rate as f32 * setup.block_ms / 1000.0)
                    .round() as usize)
                    .max(1);
                let tee = VoiceTee::new(setup, block_frames, output_format.sample_rate);
                (Some(tee), vec![0.0; capture_output_samples + block_frames])
            }
            None => (None, Vec::new()),
        };
        let voice_runtime = voice_tee.as_ref().map(|tee| Arc::clone(&tee.runtime));
        // 路径限幅只在存在变声会话时预分配；湿声以外的阶段完全跳过。
        let (voice_limiter, sfx_limiter) = if voice_tee.is_some() {
            let make_path_limiter = || {
                Limiter::new_with_channels(
                    output_format.sample_rate,
                    2,
                    PATH_LIMITER_CEILING_DB,
                    2.0,
                    80.0,
                )
            };
            (Some(make_path_limiter()), Some(make_path_limiter()))
        } else {
            (None, None)
        };
        let capture = CaptureProcessor {
            in_channels: input_format.ch(),
            mono: vec![0.0; capture_chunk_frames],
            resampled: vec![0.0; capture_output_samples],
            resampler: LinearResampler::new(base_ratio),
            drift: DriftController::new(base_ratio, target_samples),
            ring: producer,
            capacity,
            stats: stats.clone(),
            voice: voice_tee,
            voice_out,
        };

        // 固定内部分块容量；更大的设备回调会完整处理成多个块。
        const FRAMES_CAP: usize = 8192;
        let players = slots
            .iter()
            .map(|s| FilePlayer::new(s, output_format.sample_rate))
            .collect();

        let render = RenderProcessor {
            ring: consumer,
            cmd: cmd_rx,
            stop_epoch: 0,
            out_channels: output_format.ch(),
            frames_cap: FRAMES_CAP,
            mic: vec![0.0; FRAMES_CAP * 2],
            sfx: vec![0.0; FRAMES_CAP * 2],
            players,
            active_player: None,
            pending_player: None,
            applied_looping_override: 0,
            sfx_idle_frames: 0,
            voice_limiter,
            sfx_limiter,
            safety_limiter: Limiter::with_channels(output_format.sample_rate, 2),
            ctl: control.clone(),
            stats: stats.clone(),
            last_frame: [0.0, 0.0],
            silent_run: 0,
            meter_peak: 0.0,
            meter_rms: 0.0,
            sample_rate: output_format.sample_rate,
            voice_runtime,
            voice_phase: VoicePhase::Off,
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
    use crate::test_support::count_allocations;
    use crate::voice::{VoiceBackend, VoiceParams, VoicePhase, VoiceRuntime, VoiceSetup};
    use std::cell::Cell;

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

    #[test]
    fn exclusive_new_trigger_replaces_prior_pcm_and_reports_latest_slot() {
        // Distinct channel fingerprints reveal overlap directly: A only in L,
        // B only in R. Both remain long enough that natural completion cannot hide it.
        let a = 构造槽位([0.25, 0.0].repeat(48_000), true);
        let b = 构造槽位([0.0, 0.4].repeat(48_000), true);
        let format = StreamFormat::stereo(48_000);
        let mut e = EngineParts::build(format, format, vec![a, b], 20);
        e.control.mic_gain_db.set(-100.0);
        e.control.sfx_gain_db.set(0.0);
        let mut output = vec![0.0; 4_096];
        assert!(e.control.play(0));
        e.render.process(&mut output);
        assert!((output[3_000] - 0.25).abs() < 1e-6);
        assert!(e.control.play(1));
        e.render.process(&mut output);
        let left = output[3_000];
        let right = output[3_001];
        let active_slot = e.control.active_slot();
        assert!(left.abs() < 1e-6 && (right - 0.4).abs() < 1e-6 && active_slot == 1,
            "B must replace A with exclusive PCM and active_slot=1; got L={left}, R={right}, active_slot={active_slot}");
        assert!(
            output
                .as_chunks::<2>()
                .0
                .iter()
                .all(|f| f[0].abs() < 1e-6 || f[1].abs() < 1e-6),
            "旧音效和新音效不得在任何一帧重叠，包括限幅器的固定延迟"
        );
        let max_step = output
            .as_chunks::<2>()
            .0
            .windows(2)
            .flat_map(|w| [(w[1][0] - w[0][0]).abs(), (w[1][1] - w[0][1]).abs()])
            .fold(0.0, f32::max);
        assert!(max_step < 0.002, "顺序淡出/淡入应避免切换跳变: {max_step}");
    }

    #[test]
    fn rapid_switch_keeps_latest_selection_without_render_allocation() {
        let format = StreamFormat::stereo(48_000);
        let mut e = EngineParts::build(
            format,
            format,
            vec![
                构造槽位([0.25, 0.0].repeat(48_000), true),
                构造槽位([0.0, 0.4].repeat(48_000), true),
            ],
            20,
        );
        e.control.sfx_gain_db.set(0.0);
        let mut output = vec![0.0; 4096];
        assert!(e.control.play(0));
        e.render.process(&mut output);
        assert!(e.control.play(1));
        e.render.process(&mut output[..64]);
        assert_eq!(e.control.active_slot(), 1, "淡出期间 UI 也应选择 B");
        assert_eq!(e.control.playhead_frames(), 0);
        assert!(e.control.play(0));
        assert!(e.control.play(1));
        assert!(e.control.play(0));
        assert!(!e.control.play(99), "无效槽位不能中断最后有效选择");
        let allocations = count_allocations(|| e.render.process(&mut output));
        assert_eq!(allocations, 0, "连续切换的回调仍须零分配");
        assert_eq!(e.control.active_slot(), 0);
        assert!(
            output.as_chunks::<2>().0.iter().all(|f| f[1] == 0.0),
            "尚未开始的 B 应被最新 A 取代，不能先播放 B"
        );
        assert!((output[3000] - 0.25).abs() < 1e-6);
        assert_eq!(e.render.players.iter().filter(|p| p.active).count(), 1);
    }

    #[test]
    fn play_barrier_runs_once_before_pcm_and_never_for_rejected_requests() {
        let format = StreamFormat::stereo(48_000);
        let mut e =
            EngineParts::build(format, format, vec![构造槽位(vec![0.25; 96_000], true)], 20);
        let control = e.control.clone();
        control.sfx_gain_db.set(0.0);
        let calls = Cell::new(0);
        assert!(!control.play_with_barrier(99, || calls.set(calls.get() + 1)));
        for _ in 0..256 {
            assert!(control.play(0));
        }
        assert!(!control.play_with_barrier(0, || calls.set(calls.get() + 1)));
        assert_eq!(calls.get(), 0, "拒绝请求不能关闭正在试听的音效");
        control.stop_all();
        let mut output = vec![0.0; 4096];
        e.render.process(&mut output);
        assert!(control.play_with_barrier(0, || {
            calls.set(calls.get() + 1);
            e.render.process(&mut output);
            assert!(output.iter().all(|s| *s == 0.0), "屏障完成前新音效不能开始");
        }));
        assert_eq!(calls.get(), 1);
        e.render.process(&mut output);
        assert!((output[3000] - 0.25).abs() < 1e-6);
        control.stop_all();
        e.render.process(&mut output);
        assert!(control.play_linked_with_barrier(0, &control, || calls.set(calls.get() + 1)));
        assert_eq!(calls.get(), 2, "同一控制器作镜像时屏障也只能执行一次");
    }

    #[test]
    fn cancelled_barrier_must_not_publish_a_late_play_after_stop() {
        let format = StreamFormat::stereo(48_000);
        let mut e = EngineParts::build(
            format,
            format,
            vec![构造槽位([0.25, 0.0].repeat(4800), true)],
            20,
        );
        let control = e.control.clone();
        control.mic_gain_db.set(-100.0);
        control.sfx_gain_db.set(0.0);
        let cancelled = true;
        let accepted = control.play_with_checked_barrier(0, || {
            // A newer Stop arrives while the control barrier is retiring an
            // older preview; the pending Play must be cancellable afterwards.
            control.stop_epoch.fetch_add(1, Ordering::Release);
            !cancelled
        });
        let mut output = vec![0.0; 9000 * 2];
        e.render.process(&mut output);
        assert!(
            output.iter().all(|s| *s == 0.0),
            "cancelled older Play emitted audible PCM after Stop"
        );
        assert!(!accepted);
        assert!(control.stop_all_finished());
    }

    #[test]
    fn checked_barrier_rejection_keeps_current_sound_position_and_microphone() {
        let format = StreamFormat::stereo(48_000);
        let mut e = EngineParts::build(
            format,
            format,
            vec![
                构造槽位([0.25, 0.0].repeat(4800), true),
                构造槽位([0.0, 0.4].repeat(4800), true),
            ],
            20,
        );
        let control = e.control.clone();
        control.sfx_gain_db.set(0.0);
        assert!(control.play(0));
        let mut output = vec![0.0; 1000 * 2];
        e.render.process(&mut output);
        let before = control.playhead_frames();
        let calls = Cell::new(0);
        assert!(!control.play_with_checked_barrier(1, || {
            calls.set(calls.get() + 1);
            false
        }));
        e.capture.process(&vec![0.2; 8192]);
        assert_eq!(count_allocations(|| e.render.process(&mut output)), 0);
        assert_eq!(control.active_slot(), 0);
        assert_eq!(control.playhead_frames(), before + 1000);
        assert!(
            output[500 * 2..]
                .as_chunks::<2>()
                .0
                .iter()
                .all(|f| { (f[0] - 0.45).abs() < 1e-5 && (f[1] - 0.2).abs() < 1e-5 }),
            "cancelled switch must retain A and uninterrupted microphone PCM"
        );
        assert_eq!(calls.get(), 1);
        assert!(!control.play_with_checked_barrier(99, || {
            calls.set(99);
            true
        }));
        for _ in 0..256 {
            assert!(control.set_slot_gain(0, 0.0));
        }
        assert!(!control.play_with_checked_barrier(1, || {
            calls.set(99);
            true
        }));
        assert_eq!(
            calls.get(),
            1,
            "invalid/full requests must not run a cancellation barrier"
        );
        e.render.process(&mut output);
        assert_eq!(control.active_slot(), 0);
        // The same endpoint supplied as both linked routes is still invoked
        // once and uses the checked result, without self-deadlocking.
        assert!(!control.play_linked_with_checked_barrier(1, &control, || {
            calls.set(calls.get() + 1);
            false
        }));
        assert_eq!(calls.get(), 2);
        e.render.process(&mut output);
        assert_eq!(control.active_slot(), 0);
    }

    #[test]
    fn stop_ack_waits_for_callback_fade_and_limiter_tail_while_mic_continues() {
        let format = StreamFormat::stereo(48_000);
        let mut e =
            EngineParts::build(format, format, vec![构造槽位(vec![0.25; 96_000], true)], 20);
        e.control.sfx_gain_db.set(0.0);
        assert!(e.control.play(0));
        let mut output = vec![0.0; 4096];
        e.render.process(&mut output);
        e.capture.process(&vec![0.2; 8192]);
        assert!(e.control.stop_all());
        assert!(
            !e.control.stop_all_finished(),
            "未观察停止代次时不能确认静音"
        );
        e.render.process(&mut output[..64]);
        assert!(!e.control.stop_all_finished(), "淡出尚未完成");
        e.render.process(&mut output);
        assert!(e.control.stop_all_finished());
        assert!(
            output[2000..].iter().all(|s| *s > 0.1),
            "停止音效后麦克风仍应透传"
        );
        // 统计尚未反映的排队播放也不能绕过停止确认。
        assert!(e.control.play(0));
        assert!(e.control.stop_all());
        assert!(!e.control.stop_all_finished());
        e.render.process(&mut output);
        assert!(e.control.stop_all_finished());
    }

    /// 渲染若干个缓冲区并返回观测到的峰值
    fn 渲染取峰值(e: &mut EngineParts, buffers: usize) -> f32 {
        let mut out = vec![0f32; 512];
        let mut peak = 0f32;
        for _ in 0..buffers {
            e.render.process(&mut out);
            peak = peak.max(取峰值(&out));
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
        let peak = 取峰值(&out);
        assert!((peak - 0.5).abs() < 0.05, "透传峰值应约 0.5，实际 {peak}");
        for f in out.as_chunks::<2>().0.iter().take(100) {
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
        for f in input.as_chunks_mut::<2>().0 {
            f[0] = 0.5;
        }
        e.capture.process(&input);
        let mut out = vec![0f32; 1024];
        e.render.process(&mut out);
        let peak = 取峰值(&out);
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
    fn 多槽位切换应始终只播放最后触发的音效() {
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
        assert!(
            (渲染取峰值(&mut e, 6) - 0.3).abs() < 0.05,
            "槽位 0 应独立可播"
        );

        ctl.stop(0);
        let _ = 渲染取峰值(&mut e, 6);

        ctl.play(1);
        assert!(
            (渲染取峰值(&mut e, 6) - 0.4).abs() < 0.05,
            "槽位 1 应独立可播"
        );

        // 新选择代替前一槽位；先越过顺序淡出/淡入，再测稳态。
        ctl.play(2);
        let _ = 渲染取峰值(&mut e, 4);
        let p = 渲染取峰值(&mut e, 6);
        assert!((p - 0.2).abs() < 0.01, "只应播放最后选择的 0.2，实际 {p}");
        assert_eq!(ctl.active_slot(), 2);

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
        // 前视限幅保留了 2ms 旧信号；先越过固定延迟再测量新增益。
        let _ = 渲染取峰值(&mut e, 1);
        let quiet = 渲染取峰值(&mut e, 6);
        let delta = 20.0 * (quiet / full).log10();
        assert!(
            (delta + 12.0).abs() < 1.5,
            "应衰减约 12dB，实测 {delta:.1}dB（{full:.3} → {quiet:.3}）"
        );

        // 调回去
        ctl.set_slot_gain(0, 0.0);
        let _ = 渲染取峰值(&mut e, 1);
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
        assert!(
            st.out_peak_db < -80.0,
            "静音时峰值应 < -80dBFS，实际 {}",
            st.out_peak_db
        );
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
        assert!(
            st.out_peak_db > -6.0,
            "限幅后应接近 -1dBFS，实际 {:.2}",
            st.out_peak_db
        );
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
        assert!(
            after < before + 200,
            "重触发后播放位置应回到开头附近，before={before} after={after}"
        );
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
    fn looping_static_false_finishes_once_while_true_repeats_in_actual_pcm() {
        let format = StreamFormat::stereo(48_000);
        for looping in [false, true] {
            let mut e = EngineParts::build(
                format,
                format,
                vec![构造槽位(vec![0.3; 2048 * 2], looping)],
                20,
            );
            e.control.sfx_gain_db.set(0.0);
            assert!(e.control.play(0));
            let mut output = vec![0.0; 6000 * 2];
            e.render.process(&mut output);
            assert!((output[1500 * 2] - 0.3).abs() < 1e-6);
            let repeated_peak = output[2500 * 2..]
                .iter()
                .map(|s| s.abs())
                .fold(0.0, f32::max);
            if looping {
                assert!((repeated_peak - 0.3).abs() < 1e-6);
                assert!(e.control.is_playing());
            } else {
                assert_eq!(repeated_peak, 0.0, "静态 loop=false 不能进入第二轮");
                assert!(!e.control.is_playing());
            }
        }
    }

    #[test]
    fn looping_runtime_disable_finishes_current_cycle_without_repeating_head() {
        let format = StreamFormat::stereo(48_000);
        let samples: Vec<_> = (0..2048)
            .flat_map(|i| if i < 1024 { [0.25, 0.0] } else { [0.0, 0.4] })
            .collect();
        let mut e = EngineParts::build(format, format, vec![构造槽位(samples, true)], 20);
        e.control.sfx_gain_db.set(0.0);
        assert!(e.control.play(0));
        e.render.process(&mut vec![0.0; 1000 * 2]);
        assert_eq!(e.control.playhead_frames(), 1000);
        e.control.set_looping(false);
        let mut output = vec![0.0; 3000 * 2];
        let allocations = count_allocations(|| e.render.process(&mut output));
        assert_eq!(allocations, 0);
        assert!(
            (output[400 * 2 + 1] - 0.4).abs() < 1e-6,
            "关闭后继续当前轮，不立即停下或重启"
        );
        assert!(
            output[120 * 2..]
                .as_chunks::<2>()
                .0
                .iter()
                .all(|f| f[0] == 0.0),
            "关闭循环后尾部不得交叉混入下一轮头部"
        );
        assert!(output[1200 * 2..].iter().all(|s| *s == 0.0));
        assert!(!e.control.is_playing());
        assert!(e.control.play(0));
        e.render.process(&mut output);
        assert!(
            output[2200 * 2..].iter().all(|s| *s == 0.0),
            "下一次触发也只播一轮"
        );
    }

    #[test]
    fn looping_runtime_enable_preserves_position_and_smooths_future_boundaries() {
        let format = StreamFormat::stereo(48_000);
        let samples: Vec<_> = (0..4800)
            .flat_map(|i| {
                let sample = i as f32 / 4800.0 * 1.6 - 0.8;
                [sample, sample]
            })
            .collect();
        let mut e = EngineParts::build(format, format, vec![构造槽位(samples, false)], 20);
        e.control.sfx_gain_db.set(0.0);
        assert!(e.control.play(0));
        e.render.process(&mut vec![0.0; 1000 * 2]);
        e.control.set_looping(true);
        e.render.process(&mut [0.0; 128 * 2]);
        assert_eq!(e.control.playhead_frames(), 1128, "开启循环不能倒退游标");
        let mut output = vec![0.0; 20_000 * 2];
        e.render.process(&mut output);
        let max_step = output
            .as_chunks::<2>()
            .0
            .windows(2)
            .map(|w| (w[1][0] - w[0][0]).abs())
            .fold(0.0, f32::max);
        assert!(
            max_step < 0.01,
            "初始非循环的素材也必须准备平滑循环边界: {max_step}"
        );
        assert!(e.control.is_playing());
        assert!(output[10_000 * 2..].iter().any(|s| s.abs() > 0.2));
    }

    #[test]
    fn looping_runtime_updates_survive_full_queue_and_never_revive_stopped_selection() {
        let format = StreamFormat::stereo(48_000);
        let mut e = EngineParts::build(
            format,
            format,
            vec![
                构造槽位([0.25, 0.0].repeat(2048), true),
                构造槽位([0.0, 0.4].repeat(2048), true),
            ],
            20,
        );
        e.control.sfx_gain_db.set(0.0);
        assert!(e.control.play(0));
        e.render.process(&mut [0.0; 1000 * 2]);
        for _ in 0..256 {
            assert!(e.control.set_slot_gain(0, 0.0));
        }
        assert!(!e.control.play(1));
        e.control.set_looping(false);
        let mut output = vec![0.0; 6000 * 2];
        e.render.process(&mut output);
        assert!(!e.control.is_playing(), "满队列不能阻止循环参数更新");
        e.control.set_looping(true);
        e.render.process(&mut output);
        assert!(
            output.iter().all(|s| *s == 0.0),
            "参数更新不能复活已结束音效"
        );
        assert!(e.control.play(0));
        e.render.process(&mut [0.0; 1000 * 2]);
        assert!(e.control.play(1));
        e.render.process(&mut [0.0; 64 * 2]);
        e.control.set_looping(false);
        e.control.set_looping(true);
        e.control.set_looping(false);
        e.render.process(&mut output);
        assert!(output
            .as_chunks::<2>()
            .0
            .iter()
            .all(|f| f[0] == 0.0 || f[1] == 0.0));
        assert!(output[4000 * 2..].iter().all(|s| *s == 0.0));
        assert!(
            !e.control.is_playing(),
            "快速切换按最后一次 false，仅播 B 一轮"
        );
        for _ in 0..256 {
            assert!(e.control.play(0));
        }
        e.control.stop_all();
        e.control.set_looping(true);
        e.render.process(&mut output);
        assert!(
            output.iter().all(|s| *s == 0.0),
            "开启循环不能恢复 stop 取消的排队选择"
        );
        assert!(e.control.stop_all_finished());
    }

    // Channel fingerprints make the product's single-sound contract observable
    // in the rendered PCM, including fade transitions and limiter lookahead.
    fn assert_single_sound_pcm(output: &[f32], channels: usize) {
        assert!(
            output.chunks_exact(channels).all(|frame| {
                frame.iter().all(|sample| sample.is_finite())
                    && (frame[0].abs() < 1e-6 || frame[1].abs() < 1e-6)
            }),
            "a rendered frame must never contain both A (L) and B (R)"
        );
    }

    #[derive(Clone, Copy, Debug)]
    enum ComboAction {
        PlayA,
        PlayB,
        Stop,
        LoopOff,
        LoopOn,
    }

    #[test]
    fn combination_all_three_action_sequences_keep_one_sound_and_final_loop_choice() {
        const ACTIONS: [ComboAction; 5] = [
            ComboAction::PlayA,
            ComboAction::PlayB,
            ComboAction::Stop,
            ComboAction::LoopOff,
            ComboAction::LoopOn,
        ];
        // 125 triples × 3 initial selections × 2 initial modes × 3 callback
        // schedules. The clip outlasts all intervening callbacks, so this
        // independent oracle needs only the user's last play/stop and mode.
        let format = StreamFormat::stereo(48_000);
        let slots = [
            构造槽位([0.25, 0.0].repeat(4800), false),
            构造槽位([0.0, 0.4].repeat(4800), false),
        ];
        let mut output = vec![0.0; 9000 * 2];
        for initial_selection in [None, Some(0), Some(1)] {
            for initial_loop in [false, true] {
                for callback_frames in [0, 17, 997] {
                    for first in ACTIONS {
                        for second in ACTIONS {
                            for third in ACTIONS {
                                let sequence = [first, second, third];
                                let mut e = EngineParts::build(format, format, slots.to_vec(), 20);
                                e.control.mic_gain_db.set(-100.0);
                                e.control.sfx_gain_db.set(0.0);
                                e.control.set_looping(initial_loop);
                                if let Some(slot) = initial_selection {
                                    assert!(e.control.play(slot));
                                    e.render.process(&mut output[..1000 * 2]);
                                    assert_single_sound_pcm(&output[..1000 * 2], 2);
                                }
                                let mut selected = initial_selection;
                                let mut looping = initial_loop;
                                for action in sequence {
                                    match action {
                                        ComboAction::PlayA => {
                                            assert!(e.control.play(0));
                                            selected = Some(0);
                                        }
                                        ComboAction::PlayB => {
                                            assert!(e.control.play(1));
                                            selected = Some(1);
                                        }
                                        ComboAction::Stop => {
                                            assert!(e.control.stop_all());
                                            selected = None;
                                        }
                                        ComboAction::LoopOff => {
                                            e.control.set_looping(false);
                                            looping = false;
                                        }
                                        ComboAction::LoopOn => {
                                            e.control.set_looping(true);
                                            looping = true;
                                        }
                                    }
                                    let pcm = &mut output[..callback_frames * 2];
                                    assert_eq!(count_allocations(|| e.render.process(pcm)), 0);
                                    assert_single_sound_pcm(pcm, 2);
                                }
                                assert_eq!(count_allocations(|| e.render.process(&mut output)), 0);
                                assert_single_sound_pcm(&output, 2);
                                let expected = selected.filter(|_| looping);
                                assert_eq!(e.control.active_slot(), expected.map_or(-1, |s| s as i32),
                                    "selection={initial_selection:?}, initial_loop={initial_loop}, callback={callback_frames}, sequence={sequence:?}");
                                assert_eq!(e.control.is_playing(), expected.is_some());
                                match expected {
                                    Some(0) => assert!(output[8000 * 2..]
                                        .as_chunks::<2>()
                                        .0
                                        .iter()
                                        .all(|f| (f[0] - 0.25).abs() < 1e-6 && f[1] == 0.0)),
                                    Some(1) => assert!(output[8000 * 2..]
                                        .as_chunks::<2>()
                                        .0
                                        .iter()
                                        .all(|f| f[0] == 0.0 && (f[1] - 0.4).abs() < 1e-6)),
                                    None => {
                                        assert!(output[8000 * 2..].iter().all(|s| *s == 0.0));
                                        assert!(e.control.stop_all_finished());
                                    }
                                    _ => unreachable!(),
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn combination_seeded_long_trace_handles_tiny_and_oversized_callbacks_without_overlap() {
        let format = StreamFormat::stereo(48_000);
        let mut e = EngineParts::build(
            format,
            format,
            vec![
                构造槽位([0.25, 0.0].repeat(4800), false),
                构造槽位([0.0, 0.4].repeat(4800), false),
            ],
            20,
        );
        e.control.mic_gain_db.set(-100.0);
        e.control.sfx_gain_db.set(0.0);
        let mut output = vec![0.0; 17_003 * 2];
        let mut seed = 0x8d7a_54b9_37c2_120fu64;
        for step in 0..4096 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            match (seed >> 32) % 7 {
                0 | 1 => assert!(e.control.play(((seed >> 32) % 7) as usize)),
                2 => {
                    assert!(e.control.stop_all());
                }
                3 => e.control.set_looping(false),
                4 => e.control.set_looping(true),
                5 => assert!(!e.control.play(99)),
                _ => {}
            }
            let frames = [0, 1, 17, 128, 997, 8193, 17_003][(seed as usize >> 8) % 7];
            let pcm = &mut output[..frames * 2];
            assert_eq!(
                count_allocations(|| e.render.process(pcm)),
                0,
                "step={step}"
            );
            assert_single_sound_pcm(pcm, 2);
            if step % 127 == 0 {
                // Stopping, toggling either direction, then waiting must never
                // resurrect a queued, active, or naturally completed sound.
                e.control.stop_all();
                e.control.set_looping(false);
                e.control.set_looping(true);
                assert_eq!(count_allocations(|| e.render.process(&mut output)), 0);
                assert_single_sound_pcm(&output, 2);
                assert!(output[8000 * 2..].iter().all(|s| *s == 0.0));
                assert!(e.control.stop_all_finished());
                assert!(!e.control.is_playing());
            }
        }
        e.control.set_looping(false);
        e.render.process(&mut output);
        assert_single_sound_pcm(&output, 2);
        assert!(output[8000 * 2..].iter().all(|s| *s == 0.0));
        assert!(!e.control.is_playing());
    }

    #[test]
    fn 过载信号也不削波() {
        let mut e = 构造引擎();
        let input: Vec<f32> = (0..9600).map(|i| (i as f32 * 0.1).sin() * 8.0).collect();
        e.capture.process(&input);
        let mut out = vec![0f32; 8192];
        e.render.process(&mut out);
        let peak = 取峰值(&out);
        assert!(peak <= 0.92, "限幅后峰值应 ≤ -1dBFS，实际 {peak}");
    }

    #[test]
    fn 单个音效过载时限幅器应介入() {
        let mut e = EngineParts::build(
            StreamFormat::new(48_000, 1),
            StreamFormat::new(48_000, 2),
            vec![构造槽位(vec![1.8f32; 96000], true)],
            20,
        );
        let ctl = e.control.clone();
        ctl.sfx_gain_db.set(0.0);
        ctl.play(0);
        let _ = 渲染取峰值(&mut e, 8);
        let s = ctl.stats();
        assert!(
            s.limiter_gain_db < -0.5,
            "单路 1.8 过载应触发限幅，实际增益 {}dB",
            s.limiter_gain_db
        );
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
        assert!(
            (peak - 0.501).abs() < 0.03,
            "默认增益应约 -6dB，实际峰值 {peak}"
        );
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
                mono.extend(out.as_chunks::<2>().0.iter().map(|f| f[0]));
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

    #[test]
    fn 第两百五十七槽位的播放增益和停止应独立() {
        let slots: Vec<_> = (0..257)
            .map(|i| 构造槽位(vec![if i == 256 { 0.6 } else { 0.2 }; 4000], true))
            .collect();
        let mut e = EngineParts::build(
            StreamFormat::new(48_000, 1),
            StreamFormat::stereo(48_000),
            slots,
            20,
        );
        e.control.sfx_gain_db.set(0.0);
        assert!(e.control.set_slot_gain(256, -6.0));
        assert!(e.control.play(256));
        let peak = 渲染取峰值(&mut e, 6);
        assert!((peak - 0.6 * db_to_lin(-6.0)).abs() < 0.01);
        assert_eq!(e.control.active_slot(), 256);
        assert!(!e.render.players[0].active);
        assert_eq!(e.render.players[0].gain.get(), 0.0);
        assert!(e.control.stop(256));
        let _ = 渲染取峰值(&mut e, 4);
        assert!(渲染取峰值(&mut e, 2) < 1e-6);
    }

    #[test]
    fn 停止全部必须取消排满队列的播放命令() {
        let mut e = EngineParts::build(
            StreamFormat::stereo(48_000),
            StreamFormat::stereo(48_000),
            vec![构造槽位(vec![0.5; 10_000], true)],
            20,
        );
        for _ in 0..256 {
            assert!(e.control.play(0));
        }
        assert!(!e.control.play(0), "满队列应返回背压");
        assert!(e.control.stop_all(), "高优先级停止不能被队列占满阻止");
        let mut out = vec![0.0; 2000];
        e.render.process(&mut out);
        assert!(
            out.iter().all(|&s| s == 0.0),
            "停止前的待播命令必须全部失效"
        );
        assert!(!e.control.is_playing());
        assert!(e.control.play(0));
        assert!(渲染取峰值(&mut e, 4) > 0.2, "消费旧队列后新播放应正常");
    }

    #[test]
    fn 停止之后立即播放的新代次不能被旧停止清掉() {
        let mut e = EngineParts::build(
            StreamFormat::stereo(48_000),
            StreamFormat::stereo(48_000),
            vec![构造槽位(vec![0.5; 10_000], true)],
            20,
        );
        assert!(e.control.set_slot_gain(0, -12.0));
        assert!(e.control.play(0));
        assert!(e.control.stop_all());
        assert!(e.control.play(0));
        assert!(渲染取峰值(&mut e, 4) > 0.05);
        assert_eq!(
            e.render.players[0].gain.get(),
            -12.0,
            "旧代次持久增益仍应生效"
        );
        assert!(e.control.is_playing());
    }

    #[test]
    fn 循环直流每圈不应出现音量凹陷() {
        let slot = 构造槽位(vec![0.5; 2000], true);
        let mut p = FilePlayer::new(&slot, 48_000);
        p.start();
        let mut out = vec![0.0; 10_000];
        p.render_add(&mut out);
        for &s in &out[1000..] {
            assert!((s - 0.5).abs() < 1e-6, "循环边界不得淡出到零：{s}");
        }
    }

    #[test]
    fn 不连续循环首尾应交叉淡变() {
        let samples: Vec<_> = (0..2000)
            .flat_map(|i| {
                let s = i as f32 / 2000.0 * 1.6 - 0.8;
                [s, s]
            })
            .collect();
        let slot = 构造槽位(samples, true);
        let mut p = FilePlayer::new(&slot, 48_000);
        p.start();
        let mut out = vec![0.0; 20_000];
        p.render_add(&mut out);
        let max_step = out
            .as_chunks::<2>()
            .0
            .iter()
            .map(|f| f[0])
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(max_step < 0.01, "原始首尾跳变约 1.6，应被平滑：{max_step}");
    }

    #[test]
    fn 重触发淡出结束应立即进入淡入而不留整块静音() {
        let slot = 构造槽位(vec![0.5; 20_000], true);
        let format = StreamFormat::stereo(48_000);
        let mut big = EngineParts::build(format, format, vec![slot.clone()], 20);
        let mut small = EngineParts::build(format, format, vec![slot], 20);
        for engine in [&mut big, &mut small] {
            engine.control.sfx_gain_db.set(0.0);
            engine.control.play(0);
            engine.render.process(&mut vec![0.0; 2000]);
            engine.control.play(0);
        }
        let mut actual = vec![0.0; 4000];
        let mut expected = vec![0.0; 4000];
        big.render.process(&mut actual);
        for chunk in expected.chunks_mut(128) {
            small.render.process(chunk);
        }
        assert_eq!(
            actual, expected,
            "重触发时音频不得因回调大小产生不同静音间隔"
        );
        assert!(actual[2000] > 0.49);
    }

    #[test]
    fn 超大输出回调应完整渲染且与小块波形一致() {
        for channels in [1, 2, 6] {
            let slot = 构造槽位(vec![0.5; 48_000], true);
            let mut big = EngineParts::build(
                StreamFormat::new(48_000, 1),
                StreamFormat::new(48_000, channels),
                vec![slot.clone()],
                20,
            );
            let mut small = EngineParts::build(
                StreamFormat::new(48_000, 1),
                StreamFormat::new(48_000, channels),
                vec![slot],
                20,
            );
            big.control.play(0);
            small.control.play(0);
            let mut actual = vec![f32::NAN; 20_000 * channels as usize];
            let mut expected = vec![f32::NAN; actual.len()];
            big.render.process(&mut actual);
            for chunk in expected.chunks_mut(257 * channels as usize) {
                small.render.process(chunk);
            }
            assert_eq!(big.control.stats().rendered_frames, 20_000);
            assert!(actual[10_000 * channels as usize..]
                .iter()
                .all(|s| s.abs() > 0.2));
            assert_eq!(
                actual, expected,
                "{channels}声道的大回调不得截断或改变消费速率"
            );
        }
    }

    #[test]
    fn 大采集回调与高比率转换不得在实时路径分配() {
        for (in_rate, out_rate) in [(48_000, 48_000), (8_000, 192_000), (192_000, 8_000)] {
            let mut e = EngineParts::build(
                StreamFormat::new(in_rate, 6),
                StreamFormat::stereo(out_rate),
                vec![],
                20,
            );
            let input = vec![0.3; 20_000 * 6];
            let mut out = vec![0.0; 20_000 * 2];
            let n = count_allocations(|| {
                e.capture.process(&input);
                e.render.process(&mut out);
            });
            assert_eq!(n, 0, "{in_rate}→{out_rate} 采集和渲染必须零分配");
            assert_eq!(e.control.stats().captured_frames, 20_000);
            assert_eq!(e.control.stats().rendered_frames, 20_000);
        }
    }

    // ─────────────────────────────────────────── 变声 tee 测试设施

    const 变声块毫秒: f32 = 10.0;
    const 变声块帧: usize = 480;
    const 变声延迟帧: usize = 480;
    /// 湿声链路比干声多一级路径限幅，其前视延迟由限幅器参数决定。
    fn 路径前视帧() -> usize {
        Limiter::with_channels(48_000, 2).latency_frames()
    }

    fn 取峰值(buf: &[f32]) -> f32 {
        buf.iter().fold(0f32, |m, s| m.max(s.abs()))
    }
    const 变声缓冲毫秒: u32 = 100;
    const 小回调帧: usize = 128;
    /// 每个样本对应一个唯一的源序号，用来读出切换处的时间不连续。
    const 斜坡步长: f32 = 5e-6;

    struct DelayBackend {
        delay_frames: usize,
        line: Vec<f32>,
        position: usize,
    }

    impl DelayBackend {
        fn new(delay_frames: usize) -> Self {
            Self {
                delay_frames,
                line: vec![0.0; delay_frames],
                position: 0,
            }
        }
    }

    impl VoiceBackend for DelayBackend {
        fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
            let frames = input.len().min(output.len());
            if frames == 0 {
                return 0;
            }
            if self.delay_frames == 0 {
                output[..frames].copy_from_slice(&input[..frames]);
                return frames;
            }
            for (sample, slot) in input[..frames].iter().zip(output[..frames].iter_mut()) {
                *slot = self.line[self.position];
                self.line[self.position] = *sample;
                self.position = (self.position + 1) % self.delay_frames;
            }
            frames
        }

        fn latency_frames(&self) -> usize {
            self.delay_frames
        }

        fn reset(&mut self) {
            self.line.fill(0.0);
            self.position = 0;
        }

        fn set_params(&mut self, _params: VoiceParams) {}
    }

    fn 构造带变声引擎(delay_frames: usize) -> (EngineParts, Arc<VoiceRuntime>) {
        构造带变声引擎与缓冲(delay_frames, 变声缓冲毫秒)
    }

    fn 构造带变声引擎与缓冲(
        delay_frames: usize,
        buffer_ms: u32,
    ) -> (EngineParts, Arc<VoiceRuntime>) {
        let runtime = Arc::new(VoiceRuntime::new());
        let setup = VoiceSetup::new(
            VoiceParams::default(),
            变声块毫秒,
            runtime.clone(),
            Box::new(DelayBackend::new(delay_frames)),
        );
        let engine = EngineParts::build_with_voice(
            StreamFormat::new(48_000, 1),
            StreamFormat::new(48_000, 2),
            空槽位(),
            buffer_ms,
            Some(setup),
        );
        (engine, runtime)
    }

    fn 构造带槽位变声引擎(slots: Vec<SfxSlot>) -> (EngineParts, Arc<VoiceRuntime>) {
        let runtime = Arc::new(VoiceRuntime::new());
        let setup = VoiceSetup::new(
            VoiceParams::default(),
            变声块毫秒,
            runtime.clone(),
            Box::new(DelayBackend::new(0)),
        );
        let engine = EngineParts::build_with_voice(
            StreamFormat::new(48_000, 1),
            StreamFormat::stereo(48_000),
            slots,
            变声缓冲毫秒,
            Some(setup),
        );
        (engine, runtime)
    }

    /// 以静音预填并把渲染侧推进到湿声稳态；随后由调用方恢复增益与音效。
    fn 静音进入湿声(e: &mut EngineParts, runtime: &VoiceRuntime) {
        e.control.mic_gain_db.set(-100.0);
        e.control.sfx_gain_db.set(-100.0);
        let 静音 = vec![0.0f32; 变声块帧];
        let mut out = vec![0.0f32; 变声块帧 * 2];
        for _ in 0..10 {
            e.capture.process(&静音);
        }
        runtime.request_enable();
        for _ in 0..10 {
            e.capture.process(&静音);
            e.render.process(&mut out);
            if e.render.voice_phase() == VoicePhase::Wet {
                return;
            }
        }
        panic!("静音预热后渲染侧应进入湿声");
    }

    fn 正弦块(频率: f32, 起始帧: usize, 帧数: usize) -> Vec<f32> {
        (0..帧数)
            .map(|i| {
                let n = 起始帧 + i;
                (std::f32::consts::TAU * 频率 * n as f32 / 48_000.0).sin() * 4.0
            })
            .collect()
    }

    /// 提交停止并逐帧渲染，返回确认完成所需的帧数。
    fn 等待停止确认(e: &mut EngineParts, 单帧: &mut [f32]) -> usize {
        assert!(e.control.stop_all());
        let mut frames = 0usize;
        while !e.control.stop_all_finished() {
            e.render.process(单帧);
            frames += 1;
            assert!(frames < 10_000, "停止确认迟迟不完成");
        }
        frames
    }

    fn 斜坡块(起始: usize, 帧数: usize) -> Vec<f32> {
        (0..帧数).map(|i| (起始 + i) as f32 * 斜坡步长).collect()
    }

    fn 构造指定块时长的引擎(block_ms: f32) -> EngineParts {
        let runtime = Arc::new(VoiceRuntime::new());
        let setup = VoiceSetup::new(
            VoiceParams::default(),
            block_ms,
            runtime,
            Box::new(DelayBackend::new(0)),
        );
        EngineParts::build_with_voice(
            StreamFormat::new(48_000, 1),
            StreamFormat::new(48_000, 2),
            空槽位(),
            20,
            Some(setup),
        )
    }

    /// 由输出斜坡值反推源样本序号，再用「已喂帧数 − 源序号 − 环内帧数」
    /// 消掉固定的传输延迟；干湿切换前后的差值就是重复或跳过的样本数。
    fn 变声偏移(e: &EngineParts, fed: usize, last_left: f32) -> i64 {
        let fill_frames = (e.stats.ring_fill.load(Ordering::Relaxed) / 2) as i64;
        let source_index = (last_left / 斜坡步长).round() as i64;
        fed as i64 - source_index - fill_frames
    }

    fn 采集并渲染(e: &mut EngineParts, fed: &mut usize, out: &mut [f32]) {
        let input = 斜坡块(*fed, 变声块帧);
        e.capture.process(&input);
        *fed += 变声块帧;
        e.render.process(out);
    }

    #[test]
    fn 关闭变声时输出与现有引擎一致() {
        const 缓冲毫秒: u32 = 20;
        let (mut voiced, runtime) = 构造带变声引擎与缓冲(变声延迟帧, 缓冲毫秒);
        let mut plain = EngineParts::build(
            StreamFormat::new(48_000, 1),
            StreamFormat::new(48_000, 2),
            空槽位(),
            缓冲毫秒,
        );
        assert_eq!(runtime.phase(), VoicePhase::Off);

        let mut plain_out = vec![0.0; 8192 * 2];
        let mut voiced_out = vec![0.0; 8192 * 2];
        let mut fed = 0usize;

        // 两个引擎使用相同缓冲参数，先同步预热到漂移与限幅的稳态。
        for _ in 0..5 {
            let input: Vec<f32> = (0..480)
                .map(|i| ((fed + i) as f32 * 0.01).sin() * 0.2)
                .collect();
            fed += 480;
            plain.capture.process(&input);
            voiced.capture.process(&input);
        }
        for _ in 0..200 {
            let input: Vec<f32> = (0..480)
                .map(|i| ((fed + i) as f32 * 0.01).sin() * 0.2)
                .collect();
            fed += 480;
            plain.capture.process(&input);
            voiced.capture.process(&input);
            plain.render.process(&mut plain_out[..480 * 2]);
            voiced.render.process(&mut voiced_out[..480 * 2]);
        }

        // 再比较同一组稳态渲染输出，覆盖不同回调大小。
        let mut compared_frames = 0usize;
        for frames in [997, 128, 4800, 37, 1024, 8192] {
            let input: Vec<f32> = (0..frames)
                .map(|i| ((fed + i) as f32 * 0.01).sin() * 0.3)
                .collect();
            fed += frames;
            plain.capture.process(&input);
            voiced.capture.process(&input);
            plain.render.process(&mut plain_out[..frames * 2]);
            voiced.render.process(&mut voiced_out[..frames * 2]);
            assert_eq!(
                plain_out[..frames * 2],
                voiced_out[..frames * 2],
                "Off 阶段稳态输出必须逐样本一致（{frames} 帧）"
            );
            compared_frames += frames;
        }
        assert!(compared_frames > 0);
        assert_eq!(voiced.render.voice_phase(), VoicePhase::Off);
    }

    fn 路径限幅上限() -> f32 {
        db_to_lin(PATH_LIMITER_CEILING_DB)
    }

    fn 末端限幅上限() -> f32 {
        db_to_lin(-1.0)
    }

    #[test]
    fn 人声峰值不改变音效增益() {
        let 音效电平 = 0.1f32;
        let 音效槽 = || vec![音效电平; 96_000];
        let 静音槽 = || vec![0.0f32; 96_000];
        let (mut 双路引擎, 双路运行时) =
            构造带槽位变声引擎(vec![构造槽位(音效槽(), true)]);
        let (mut 音效基准, 音效运行时) =
            构造带槽位变声引擎(vec![构造槽位(音效槽(), true)]);
        let (mut 人声基准, 人声运行时) =
            构造带槽位变声引擎(vec![构造槽位(静音槽(), true)]);

        静音进入湿声(&mut 双路引擎, &双路运行时);
        静音进入湿声(&mut 音效基准, &音效运行时);
        静音进入湿声(&mut 人声基准, &人声运行时);

        for 引擎 in [&mut 双路引擎, &mut 音效基准, &mut 人声基准] {
            引擎.control.mic_gain_db.set(0.0);
            引擎.control.sfx_gain_db.set(0.0);
            assert!(引擎.control.play(0));
        }

        let 人声峰值 = vec![4.0f32; 变声块帧];
        let 静音输入 = vec![0.0f32; 变声块帧];
        let mut 双路输出 = vec![0.0f32; 变声块帧 * 2];
        let mut 音效输出 = vec![0.0f32; 变声块帧 * 2];
        let mut 人声输出 = vec![0.0f32; 变声块帧 * 2];
        for _ in 0..12 {
            双路引擎.capture.process(&人声峰值);
            音效基准.capture.process(&静音输入);
            人声基准.capture.process(&人声峰值);
            双路引擎.render.process(&mut 双路输出);
            音效基准.render.process(&mut 音效输出);
            人声基准.render.process(&mut 人声输出);
        }

        for i in 0..双路输出.len() {
            let 独立合成 = 音效输出[i] + 人声输出[i];
            assert!(
                (双路输出[i] - 独立合成).abs() < 1e-6,
                "人声峰值不得改变音效路输出：第 {i} 样本 {}，独立合成 {独立合成}",
                双路输出[i]
            );
        }
        let 音效峰值 = 取峰值(&音效输出);
        assert!(
            (音效峰值 - 音效电平).abs() < 1e-5,
            "音效路应保持原电平 {音效电平}，实测峰值 {音效峰值}"
        );
        let 人声限幅峰值 = 取峰值(&人声输出);
        assert!(
            人声限幅峰值 <= 路径限幅上限() + 1e-5 && 人声限幅峰值 > 路径限幅上限() * 0.98,
            "人声应被路径限幅压到 −3 dBFS，实测 {人声限幅峰值}"
        );
    }

    #[test]
    fn 音效峰值不改变人声增益() {
        let 人声电平 = 0.1f32;
        let 音效峰值槽 = || vec![4.0f32; 96_000];
        let 静音槽 = || vec![0.0f32; 96_000];
        let (mut 双路引擎, 双路运行时) =
            构造带槽位变声引擎(vec![构造槽位(音效峰值槽(), true)]);
        let (mut 人声基准, 人声运行时) =
            构造带槽位变声引擎(vec![构造槽位(静音槽(), true)]);
        let (mut 音效基准, 音效运行时) =
            构造带槽位变声引擎(vec![构造槽位(音效峰值槽(), true)]);

        静音进入湿声(&mut 双路引擎, &双路运行时);
        静音进入湿声(&mut 人声基准, &人声运行时);
        静音进入湿声(&mut 音效基准, &音效运行时);

        for 引擎 in [&mut 双路引擎, &mut 人声基准, &mut 音效基准] {
            引擎.control.mic_gain_db.set(0.0);
            引擎.control.sfx_gain_db.set(0.0);
            assert!(引擎.control.play(0));
        }

        let 人声输入 = vec![人声电平; 变声块帧];
        let 静音输入 = vec![0.0f32; 变声块帧];
        let mut 双路输出 = vec![0.0f32; 变声块帧 * 2];
        let mut 人声输出 = vec![0.0f32; 变声块帧 * 2];
        let mut 音效输出 = vec![0.0f32; 变声块帧 * 2];
        for _ in 0..12 {
            双路引擎.capture.process(&人声输入);
            人声基准.capture.process(&人声输入);
            音效基准.capture.process(&静音输入);
            双路引擎.render.process(&mut 双路输出);
            人声基准.render.process(&mut 人声输出);
            音效基准.render.process(&mut 音效输出);
        }

        for i in 0..双路输出.len() {
            let 独立合成 = 人声输出[i] + 音效输出[i];
            assert!(
                (双路输出[i] - 独立合成).abs() < 1e-6,
                "音效峰值不得改变人声路输出：第 {i} 样本 {}，独立合成 {独立合成}",
                双路输出[i]
            );
        }
        let 人声峰值 = 取峰值(&人声输出);
        assert!(
            (人声峰值 - 人声电平).abs() < 1e-5,
            "人声路应保持原电平 {人声电平}，实测峰值 {人声峰值}"
        );
        let 音效限幅峰值 = 取峰值(&音效输出);
        assert!(
            音效限幅峰值 <= 路径限幅上限() + 1e-5 && 音效限幅峰值 > 路径限幅上限() * 0.98,
            "音效应被路径限幅压到 −3 dBFS，实测 {音效限幅峰值}"
        );
    }

    #[test]
    fn 两路同时过载触发末端安全限幅() {
        let (mut e, runtime) =
            构造带槽位变声引擎(vec![构造槽位(vec![4.0f32; 96_000], true)]);
        静音进入湿声(&mut e, &runtime);
        e.control.mic_gain_db.set(0.0);
        e.control.sfx_gain_db.set(0.0);
        assert!(e.control.play(0));

        let mut out = vec![0.0f32; 变声块帧 * 2];
        let mut 已喂帧 = 0usize;
        for _ in 0..12 {
            let mic = 正弦块(440.0, 已喂帧, 变声块帧);
            已喂帧 += 变声块帧;
            e.capture.process(&mic);
            e.render.process(&mut out);
        }
        let mic = 正弦块(440.0, 已喂帧, 变声块帧);
        e.capture.process(&mic);
        e.render.process(&mut out);

        let 峰值 = 取峰值(&out);
        assert!(
            峰值 <= 末端限幅上限() + 1e-5,
            "两路过载后输出峰值 {峰值} 超过 −1 dBFS"
        );
        assert!(
            峰值 > 末端限幅上限() * 0.98,
            "末端安全限幅应真正参与，峰值仅 {峰值}"
        );
        assert!(out.iter().all(|s| s.abs() < 1.0), "输出不得进入硬削波");
        let 平顶 = out
            .iter()
            .filter(|s| (s.abs() - 末端限幅上限()).abs() < 1e-5)
            .count();
        assert!(
            平顶 as f32 / (out.len() as f32) < 0.05,
            "过载应靠增益控制而非硬削波，平顶比例 {平顶}/{}",
            out.len()
        );
        assert!(
            e.stats.snapshot().limiter_gain_db < -3.0,
            "末端限幅应显示明显增益衰减，实测 {} dB",
            e.stats.snapshot().limiter_gain_db
        );
    }

    #[test]
    fn 停止尾部包含音效限幅与末端限幅() {
        let 淡出帧 = (48_000.0 * FADE_MS / 1000.0).round() as usize;
        let 槽 = || 构造槽位(vec![0.25f32; 96_000], true);

        let (mut 湿声引擎, runtime) = 构造带槽位变声引擎(vec![槽()]);
        静音进入湿声(&mut 湿声引擎, &runtime);
        湿声引擎.control.mic_gain_db.set(-100.0);
        湿声引擎.control.sfx_gain_db.set(0.0);
        assert!(湿声引擎.control.play(0));
        let mut 块 = vec![0.0f32; 变声块帧 * 2];
        for _ in 0..10 {
            湿声引擎.capture.process(&vec![0.0f32; 变声块帧]);
            湿声引擎.render.process(&mut 块);
        }
        assert_eq!(湿声引擎.render.voice_phase(), VoicePhase::Wet);

        let format = StreamFormat::stereo(48_000);
        let mut 干声引擎 = EngineParts::build(format, format, vec![槽()], 变声缓冲毫秒);
        干声引擎.control.mic_gain_db.set(-100.0);
        干声引擎.control.sfx_gain_db.set(0.0);
        assert!(干声引擎.control.play(0));
        for _ in 0..10 {
            干声引擎.render.process(&mut 块);
        }
        assert_eq!(干声引擎.render.voice_phase(), VoicePhase::Off);

        let mut 单帧 = vec![0.0f32; 2];
        let 湿声等待 = 等待停止确认(&mut 湿声引擎, &mut 单帧);
        let 干声等待 = 等待停止确认(&mut 干声引擎, &mut 单帧);

        assert!(
            (淡出帧..淡出帧 + 2 * 路径前视帧()).contains(&干声等待),
            "干声停止应只等末端限幅尾部，实测 {干声等待} 帧"
        );
        assert!(
            湿声等待 > 淡出帧 + 路径前视帧(),
            "湿声停止必须越过音效与末端两级尾部，实测 {湿声等待} 帧"
        );
        assert!(
            湿声等待 < 淡出帧 + 3 * 路径前视帧(),
            "人声限幅尾部不应计入停止确认，实测 {湿声等待} 帧"
        );
        assert_eq!(
            湿声等待 - 干声等待,
            路径前视帧(),
            "湿声应比干声多等一级音效路径限幅：湿声 {湿声等待}，干声 {干声等待}"
        );
    }

    #[test]
    fn 预热期干声继续写环() {
        const 缓冲毫秒: u32 = 20;
        let (mut e, runtime) = 构造带变声引擎与缓冲(2_400, 缓冲毫秒);
        let mut out = vec![0.0; 小回调帧 * 2];

        // 先把环预填到目标水位；之后采集与渲染等量推进。
        for _ in 0..20 {
            e.capture.process(&vec![0.1; 小回调帧]);
        }
        let fill_before = e.stats.ring_fill.load(Ordering::Relaxed);
        runtime.request_enable();
        assert_eq!(runtime.phase(), VoicePhase::Warmup);

        for _ in 0..8 {
            let input = vec![0.1; 小回调帧];
            let allocations = count_allocations(|| {
                e.capture.process(&input);
                e.render.process(&mut out);
            });
            assert_eq!(allocations, 0, "预热回调必须零分配");
        }

        let fill_after = e.stats.ring_fill.load(Ordering::Relaxed);
        assert_eq!(
            e.stats.underruns.load(Ordering::Relaxed),
            0,
            "预热期干声继续写环，不应欠载"
        );
        let target_samples = 48_000usize / 1000 * 缓冲毫秒 as usize * 2;
        assert!(
            fill_after >= target_samples,
            "预热期水位被抽干：目标 {target_samples}，实测 {fill_after}（起始 {fill_before}）"
        );
        assert_eq!(
            runtime.phase(),
            VoicePhase::Warmup,
            "算法延迟未走完时不应切换"
        );

        for _ in 0..40 {
            let input = vec![0.1; 小回调帧];
            e.capture.process(&input);
            e.render.process(&mut out);
        }
        assert_eq!(runtime.phase(), VoicePhase::Wet);
        assert_eq!(e.stats.underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn 紧缓冲小回调在湿声阶段不欠载() {
        const 缓冲毫秒: u32 = 10;
        const 运行帧数: usize = 48_000 * 5;
        for 回调帧 in [128usize, 256] {
            let (mut e, runtime) = 构造带变声引擎与缓冲(变声延迟帧, 缓冲毫秒);
            let mut out = vec![0.0; 回调帧 * 2];
            let mut fed = 0usize;

            // 预填环形缓冲，隔离启动阶段本身的欠载。
            for _ in 0..20 {
                e.capture.process(&vec![0.1; 回调帧]);
                fed += 回调帧;
            }
            runtime.request_enable();
            assert_eq!(runtime.phase(), VoicePhase::Warmup);

            let mut wet_started = false;
            let mut wet_underruns = 0u64;
            let mut min_wet_fill = usize::MAX;
            let mut last_underruns = e.stats.underruns.load(Ordering::Relaxed);
            while fed < 运行帧数 {
                let frames = 回调帧.min(运行帧数 - fed);
                e.capture.process(&vec![0.1; frames]);
                fed += frames;
                e.render.process(&mut out[..frames * 2]);
                let underruns = e.stats.underruns.load(Ordering::Relaxed);
                if runtime.phase() == VoicePhase::Wet {
                    wet_started = true;
                }
                if wet_started {
                    wet_underruns += underruns - last_underruns;
                    min_wet_fill = min_wet_fill.min(e.stats.ring_fill.load(Ordering::Relaxed));
                }
                last_underruns = underruns;
            }

            assert_eq!(
                runtime.phase(),
                VoicePhase::Wet,
                "{回调帧} 帧回调应在预热后进入湿声"
            );
            assert!(wet_started, "{回调帧} 帧回调未观察到湿声阶段");
            assert_eq!(
                wet_underruns, 0,
                "{回调帧} 帧回调在 10 ms 缓冲下湿声阶段不应欠载"
            );
            assert!(min_wet_fill > 0, "{回调帧} 帧回调湿声阶段环形缓冲被抽干");
            assert_eq!(
                e.stats.underruns.load(Ordering::Relaxed),
                0,
                "{回调帧} 帧回调整个运行期都不应欠载"
            );
            assert_eq!(
                e.stats.overruns.load(Ordering::Relaxed),
                0,
                "{回调帧} 帧回调不应撑爆环形缓冲"
            );
        }
    }

    #[test]
    #[should_panic(expected = "变声块时长")]
    fn 非有限变声块时长应被拒绝() {
        let _ = 构造指定块时长的引擎(f32::NAN);
    }

    #[test]
    #[should_panic(expected = "变声块时长")]
    fn 过长变声块时长应被拒绝() {
        let _ = 构造指定块时长的引擎(40.1);
    }

    #[test]
    fn 启用切换重复不超过算法延迟() {
        let (mut e, runtime) = 构造带变声引擎(变声延迟帧);
        let mut out = vec![0.0; 变声块帧 * 2];
        let mut fed = 0usize;

        // 预填后先排空到目标水位，让漂移比率收敛到 1，避免时间基偏差混入测量。
        for _ in 0..10 {
            e.capture.process(&斜坡块(fed, 变声块帧));
            fed += 变声块帧;
        }
        for _ in 0..160 {
            采集并渲染(&mut e, &mut fed, &mut out);
        }
        let dry_offset = 变声偏移(&e, fed, out[out.len() - 2]);
        assert_eq!(runtime.phase(), VoicePhase::Off);

        runtime.request_enable();
        for _ in 0..80 {
            采集并渲染(&mut e, &mut fed, &mut out);
        }
        assert_eq!(runtime.phase(), VoicePhase::Wet);
        let wet_offset = 变声偏移(&e, fed, out[out.len() - 2]);
        let repeated = wet_offset - dry_offset;
        let latency = (变声延迟帧 + 变声块帧 + 路径前视帧()) as i64;
        assert!(
            repeated > 0,
            "启用后应测到干湿切换的重复，实测 {repeated} 样本"
        );
        assert!(
            repeated <= latency,
            "重复 {repeated} 样本超过算法延迟上限 {latency}"
        );
        let input = 斜坡块(fed, 变声块帧);
        let allocations = count_allocations(|| {
            e.capture.process(&input);
            e.render.process(&mut out);
        });
        assert_eq!(allocations, 0, "湿声回调必须零分配");
        assert_eq!(e.stats.underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn 关闭切换跳过不超过算法延迟() {
        let (mut e, runtime) = 构造带变声引擎(变声延迟帧);
        let mut out = vec![0.0; 变声块帧 * 2];
        let mut fed = 0usize;

        // 预填后先排空到目标水位，让漂移比率收敛到 1，避免时间基偏差混入测量。
        for _ in 0..10 {
            e.capture.process(&斜坡块(fed, 变声块帧));
            fed += 变声块帧;
        }
        for _ in 0..160 {
            采集并渲染(&mut e, &mut fed, &mut out);
        }
        runtime.request_enable();
        for _ in 0..80 {
            采集并渲染(&mut e, &mut fed, &mut out);
        }
        assert_eq!(runtime.phase(), VoicePhase::Wet);
        let wet_offset = 变声偏移(&e, fed, out[out.len() - 2]);

        runtime.request_disable();
        assert_eq!(runtime.phase(), VoicePhase::FadingOut);
        let input = 斜坡块(fed, 变声块帧);
        let allocations = count_allocations(|| {
            e.capture.process(&input);
            e.render.process(&mut out);
        });
        fed += 变声块帧;
        assert_eq!(allocations, 0, "淡出回调必须零分配");
        assert_eq!(
            runtime.phase(),
            VoicePhase::FadingOut,
            "首个淡出块不应提前结束"
        );
        for _ in 0..40 {
            采集并渲染(&mut e, &mut fed, &mut out);
        }
        assert_eq!(runtime.phase(), VoicePhase::Off);
        let dry_offset = 变声偏移(&e, fed, out[out.len() - 2]);
        let skipped = wet_offset - dry_offset;
        let latency = (变声延迟帧 + 变声块帧 + 路径前视帧()) as i64;
        assert!(
            skipped > 0,
            "关闭后应测到干湿切换的跳过，实测 {skipped} 样本"
        );
        assert!(
            skipped <= latency,
            "跳过 {skipped} 样本超过算法延迟上限 {latency}"
        );
        assert_eq!(e.stats.underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn 旧代次不能切换新会话() {
        let (mut old_engine, old_runtime) = 构造带变声引擎(变声延迟帧);
        let (mut new_engine, new_runtime) = 构造带变声引擎(变声延迟帧);
        let input = vec![0.1; 变声块帧];

        old_engine.capture.process(&input);
        new_engine.capture.process(&input);
        old_runtime.request_enable();
        for _ in 0..10 {
            old_engine.capture.process(&input);
            new_engine.capture.process(&input);
        }

        assert_ne!(
            old_runtime.phase(),
            VoicePhase::Off,
            "旧会话应响应自己的请求"
        );
        assert_eq!(
            new_runtime.phase(),
            VoicePhase::Off,
            "旧会话请求不能改变新会话"
        );
        let mut out = vec![0.0; 变声块帧 * 2];
        new_engine.render.process(&mut out);
        assert_eq!(new_engine.render.voice_phase(), VoicePhase::Off);
    }

    #[test]
    fn 关闭后的旧预热不能切回湿声() {
        let (mut e, runtime) = 构造带变声引擎(变声延迟帧);
        let input = vec![0.1; 变声块帧];
        runtime.request_enable();
        e.capture.process(&input);
        assert_eq!(runtime.phase(), VoicePhase::Warmup);
        runtime.request_disable();
        assert_eq!(runtime.phase(), VoicePhase::Off);

        let mut out = vec![0.0; 变声块帧 * 2];
        for _ in 0..40 {
            e.capture.process(&input);
            e.render.process(&mut out);
        }
        assert_eq!(runtime.phase(), VoicePhase::Off);
        assert_eq!(e.render.voice_phase(), VoicePhase::Off);
    }
}
