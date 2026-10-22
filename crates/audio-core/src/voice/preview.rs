//! 人声试听记录区与状态机。
//!
//! 记录区在会话构造时按输出采样率一次分配三秒单声道容量；音频回调只推进
//! 原子写指针，不分配、不加锁、不做 IO。控制线程通过 [`VoicePreviewControl`]
//! 启动、取消与取回片段，音频侧与渲染侧分别按代次校验写入。

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use super::{VoiceControl, VoicePhase, VoiceRuntime};

/// 试听样本的来源路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoicePreviewSource {
    /// 变声关闭或临时旁路时，复用会话人声链处理后的湿声。
    Capture,
    /// 变声已处于湿声时，从渲染侧人声路径限幅之后取音。
    Render,
}

impl VoicePreviewSource {
    /// 片段是否已经过渲染侧的人声路径限幅。
    pub fn post_limiter(self) -> bool {
        matches!(self, Self::Render)
    }
}

/// 开始试听失败的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewError {
    /// 变声正在预热或淡出，记录区暂不可用。
    Busy,
    /// 记录区没有可用容量。
    Unavailable,
    /// 已有一次试听占用记录区。
    AlreadyActive,
}

/// 一次试听的标识与来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoicePreviewStart {
    pub generation: u64,
    pub source: VoicePreviewSource,
}

/// 冻结后的试听片段。
#[derive(Debug, Clone)]
pub struct VoicePreviewClip {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub post_limiter: bool,
}

/// 运行时记录的试听状态快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PreviewSnapshot {
    pub generation: u64,
    pub source: VoicePreviewSource,
    pub finished: bool,
}

/// 音频回调写入记录区的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviewWrite {
    /// 样本已写入，记录区尚未写满。
    Written,
    /// 记录区已写满；本次调用可能刚写入最后一个样本，也可能此前已满。
    Full,
    /// 代次不匹配或记录区已被控制线程复位，本次写入不得推进写指针。
    Rejected,
}

/// 预分配的单声道试听记录区。
///
/// 写入方只能是采集或渲染回调中的一个。代次与写指针打包在同一个原子量中：
/// 控制线程用整字存储开始或复位记录，音频回调以比较交换推进位置，失败的
/// 写入不会污染新记录的写指针。控制线程只在记录冻结后复制样本。
pub struct VoicePreviewBuffer {
    /// 样本按位存储在预分配的原子槽中；写入方无需可变借用即可推进。
    samples: Vec<AtomicU32>,
    sample_rate: u32,
    /// 高位为代次，低 `pos_bits` 位为写位置。
    state: AtomicU64,
    pos_bits: u32,
    generation_mask: u64,
}

impl VoicePreviewBuffer {
    /// 按采样率与秒数一次分配记录区。
    ///
    /// 非法采样率或非正秒数得到零容量，由控制句柄报告不可用；音频回调不会
    /// 再为记录区分配内存。
    pub fn new(sample_rate: u32, seconds: f32) -> Arc<Self> {
        let capacity = if sample_rate == 0 || !seconds.is_finite() || seconds <= 0.0 {
            0
        } else {
            (sample_rate as f64 * seconds as f64).round() as usize
        };
        // 位置字段要能表示写满后的 position == capacity，因此按容量位宽取字段。
        let pos_bits = if capacity == 0 {
            1
        } else {
            64 - (capacity as u64).leading_zeros()
        };
        assert!(pos_bits < 64, "试听记录区容量超出原子编码上限");
        let generation_bits = 64 - pos_bits;
        let generation_mask = (1u64 << generation_bits) - 1;
        Arc::new(Self {
            samples: (0..capacity).map(|_| AtomicU32::new(0)).collect(),
            sample_rate,
            state: AtomicU64::new(0),
            pos_bits,
            generation_mask,
        })
    }

    fn encode(&self, generation: u64, position: usize) -> u64 {
        ((generation & self.generation_mask) << self.pos_bits) | position as u64
    }

    fn decode(&self, raw: u64) -> (u64, usize) {
        let position_mask = (1u64 << self.pos_bits) - 1;
        let generation = (raw >> self.pos_bits) & self.generation_mask;
        (generation, (raw & position_mask) as usize)
    }

    /// 输出采样率。
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// 固定容量（单声道样本数）。
    pub fn capacity(&self) -> usize {
        self.samples.len()
    }

    /// 已写入的样本数。
    pub(crate) fn write_pos(&self) -> usize {
        self.decode(self.state.load(Ordering::Acquire)).1
    }

    /// 开始一次新代次的写入；代次与零位置整字发布。
    ///
    /// 单个原子存储消除了“新代次已可见、旧写指针尚未归零”的中间状态。
    pub(crate) fn begin(&self, generation: u64) {
        debug_assert!(generation != 0, "试听代次 0 保留给未激活状态");
        self.state
            .store(self.encode(generation, 0), Ordering::Release);
    }

    /// 音频回调写入一个样本。
    ///
    /// 只有打包状态仍与读取时一致才能推进位置；控制线程的复位或新代次会
    /// 让比较交换失败，迟到的样本不会抬高新记录的写指针。
    pub(crate) fn push(&self, generation: u64, sample: f32) -> PreviewWrite {
        let expected = generation & self.generation_mask;
        let state = self.state.load(Ordering::Acquire);
        let (current, position) = self.decode(state);
        if expected == 0 || current != expected {
            return PreviewWrite::Rejected;
        }
        if position >= self.samples.len() {
            return PreviewWrite::Full;
        }
        // 样本先写入，再由成功的比较交换发布位置；交换失败时最多留下一个迟到样本。
        self.samples[position].store(sample.to_bits(), Ordering::Relaxed);
        if self
            .state
            .compare_exchange(state, state + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return PreviewWrite::Rejected;
        }
        if position + 1 >= self.samples.len() {
            PreviewWrite::Full
        } else {
            PreviewWrite::Written
        }
    }

    /// 停止音频侧写入并丢弃本次内容。
    ///
    /// 只收回打包状态；样本数组保持预分配，供下一次试听复用。
    pub(crate) fn reset(&self) {
        self.state.store(0, Ordering::Release);
    }

    /// 在控制线程复制已冻结的样本。
    pub(crate) fn snapshot(&self, count: usize) -> Vec<f32> {
        let count = count.min(self.write_pos()).min(self.samples.len());
        self.samples[..count]
            .iter()
            .map(|sample| f32::from_bits(sample.load(Ordering::Relaxed)))
            .collect()
    }
}

/// 试听控制句柄。
///
/// 所有方法都在控制线程调用；音频回调只通过 [`VoiceRuntime`] 读取代次与来源。
#[derive(Clone)]
pub struct VoicePreviewControl {
    runtime: Arc<VoiceRuntime>,
    buffer: Arc<VoicePreviewBuffer>,
}

impl VoicePreviewControl {
    /// 用会话的人声控制句柄和记录区创建试听控制。
    pub fn new(control: &VoiceControl, buffer: Arc<VoicePreviewBuffer>) -> Self {
        Self {
            runtime: control.runtime(),
            buffer,
        }
    }

    /// 按当前变声阶段选择试听来源并开始记录。
    ///
    /// 关闭与临时旁路复用采集链的湿声；湿声阶段从渲染侧限幅后取音。
    /// 预热与淡出期间信号路径尚未稳定，返回 [`PreviewError::Busy`]。
    pub fn start(&self) -> Result<VoicePreviewStart, PreviewError> {
        if self.buffer.capacity() == 0 {
            return Err(PreviewError::Unavailable);
        }
        let phase = self.runtime.phase();
        let source = match phase {
            VoicePhase::Off => VoicePreviewSource::Capture,
            VoicePhase::Wet => VoicePreviewSource::Render,
            VoicePhase::Warmup | VoicePhase::FadingOut => return Err(PreviewError::Busy),
        };
        let generation = self.runtime.start_preview(source)?;
        self.buffer.begin(generation);
        // 阶段可能在与音频回调并发时改变；来源绑定已过期的阶段则取消本次试听。
        if self.runtime.phase() != phase {
            self.runtime.cancel_preview();
            self.buffer.reset();
            return Err(PreviewError::Busy);
        }
        Ok(VoicePreviewStart { generation, source })
    }

    /// 取消本次试听并清空记录区。
    pub fn stop(&self) {
        self.runtime.cancel_preview();
        self.buffer.reset();
    }

    /// 记录区是否已经写满并冻结。
    pub fn is_finished(&self) -> bool {
        self.runtime
            .preview_snapshot()
            .is_some_and(|snapshot| snapshot.finished)
    }

    /// 取走已冻结的片段；未完成或已取消时返回 `None`。
    ///
    /// 复制完成后释放记录区，下一次试听从写指针零处开始。
    pub fn take(&self) -> Option<VoicePreviewClip> {
        let snapshot = self.runtime.preview_snapshot()?;
        if !snapshot.finished {
            return None;
        }
        let samples = self.buffer.snapshot(self.buffer.write_pos());
        if samples.is_empty() {
            return None;
        }
        if !self.runtime.complete_preview(snapshot.generation) {
            return None;
        }
        self.buffer.reset();
        Some(VoicePreviewClip {
            samples,
            sample_rate: self.buffer.sample_rate(),
            post_limiter: snapshot.source.post_limiter(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::VoiceParams;

    const 采样率: u32 = 48_000;

    fn 创建控制() -> (
        Arc<VoiceRuntime>,
        VoiceControl,
        Arc<VoicePreviewBuffer>,
        VoicePreviewControl,
    ) {
        let runtime = Arc::new(VoiceRuntime::new());
        let control = VoiceControl::with_runtime(runtime.clone(), VoiceParams::neutral());
        let buffer = VoicePreviewBuffer::new(采样率, 3.0);
        let preview = VoicePreviewControl::new(&control, buffer.clone());
        (runtime, control, buffer, preview)
    }

    fn 进入湿声(runtime: &VoiceRuntime, control: &VoiceControl) {
        control.request_enable();
        let generation = control.generation();
        runtime.mark_render_ready(generation);
        assert!(runtime.finish_warmup(generation), "预热完成应进入湿声");
        assert_eq!(control.phase(), VoicePhase::Wet);
    }

    #[test]
    fn 试听记录区容量为三秒且不增长() {
        for 速率 in [48_000, 44_100] {
            let buffer = VoicePreviewBuffer::new(速率, 3.0);
            let 容量 = 速率 as usize * 3;
            assert_eq!(buffer.capacity(), 容量, "{速率} Hz 容量应为三秒");

            buffer.begin(1);
            for index in 0..容量 {
                let expected = if index + 1 == 容量 {
                    PreviewWrite::Full
                } else {
                    PreviewWrite::Written
                };
                assert_eq!(buffer.push(1, index as f32), expected, "写满前应接受样本");
            }
            assert_eq!(buffer.write_pos(), 容量);
            assert_eq!(
                buffer.push(1, 1.0),
                PreviewWrite::Full,
                "写满后不得继续写入"
            );
            assert_eq!(
                buffer.push(1, 2.0),
                PreviewWrite::Full,
                "写满后不得扩大容量"
            );
            assert_eq!(buffer.write_pos(), 容量, "写满后指针不得前进");
            assert_eq!(buffer.capacity(), 容量, "容量固定为三秒");
        }
    }

    #[test]
    fn 试听期间切换阶段取消记录() {
        let (runtime, control, buffer, preview) = 创建控制();
        assert_eq!(control.phase(), VoicePhase::Off);

        let start = preview.start().expect("关闭状态应开始采集试听");
        assert_eq!(start.source, VoicePreviewSource::Capture);
        assert_eq!(
            preview.start(),
            Err(PreviewError::AlreadyActive),
            "同一时间只允许一个记录"
        );
        assert_eq!(buffer.push(start.generation, 0.5), PreviewWrite::Written);

        control.request_enable();
        assert_eq!(control.phase(), VoicePhase::Warmup, "启用后进入预热");
        assert!(preview.take().is_none(), "启用应取消采集试听");
        assert!(!preview.is_finished(), "取消后不得报告完成");
        assert_eq!(preview.start(), Err(PreviewError::Busy), "预热阶段不可试听");

        let generation = control.generation();
        runtime.mark_render_ready(generation);
        assert!(runtime.finish_warmup(generation));
        assert_eq!(control.phase(), VoicePhase::Wet);

        let start = preview.start().expect("湿声阶段应从渲染侧试听");
        assert_eq!(start.source, VoicePreviewSource::Render);
        control.request_disable();
        assert_eq!(control.phase(), VoicePhase::FadingOut);
        assert!(preview.take().is_none(), "关闭应取消渲染侧试听");
        assert_eq!(preview.start(), Err(PreviewError::Busy), "淡出阶段不可试听");
        assert!(runtime.finish_fade_out(control.generation()));
        assert_eq!(control.phase(), VoicePhase::Off);

        进入湿声(&runtime, &control);
        let start = preview.start().expect("再次进入湿声应可试听");
        assert_eq!(start.source, VoicePreviewSource::Render);
        control.request_bypass();
        assert_eq!(control.phase(), VoicePhase::FadingOut);
        assert!(preview.take().is_none(), "临时旁路应取消渲染侧试听");
        assert!(runtime.finish_fade_out(control.generation()));
        assert_eq!(control.phase(), VoicePhase::Off);

        let start = preview.start().expect("关闭状态应再次开始采集试听");
        assert_eq!(start.source, VoicePreviewSource::Capture);
        let 预设 = VoiceParams {
            pitch_semitones: 7.0,
            ..VoiceParams::neutral()
        };
        control.set_params(预设);
        assert!(preview.take().is_none(), "切换预设应取消试听");
    }

    #[test]
    fn 预热或淡出阶段开始试听返回忙碌() {
        let (runtime, control, buffer, preview) = 创建控制();
        control.request_enable();
        assert_eq!(control.phase(), VoicePhase::Warmup);
        assert_eq!(preview.start(), Err(PreviewError::Busy));
        assert_eq!(buffer.write_pos(), 0, "忙碌时不得写入记录区");

        let generation = control.generation();
        runtime.mark_render_ready(generation);
        assert!(runtime.finish_warmup(generation));
        control.request_disable();
        assert_eq!(control.phase(), VoicePhase::FadingOut);
        assert_eq!(preview.start(), Err(PreviewError::Busy));
        assert_eq!(buffer.write_pos(), 0, "淡出时不得写入记录区");
    }

    #[test]
    fn 试听记录区冻结后回放读取稳定() {
        let (runtime, _control, buffer, preview) = 创建控制();
        let start = preview.start().expect("关闭状态应开始采集试听");
        let 容量 = buffer.capacity();
        for index in 0..容量 {
            let expected = if index + 1 == 容量 {
                PreviewWrite::Full
            } else {
                PreviewWrite::Written
            };
            assert_eq!(buffer.push(start.generation, index as f32), expected);
        }
        assert!(runtime.finish_preview(start.generation), "写满应冻结记录");
        assert!(preview.is_finished());

        let 冻结位置 = buffer.write_pos();
        for _ in 0..16 {
            assert_eq!(
                buffer.push(start.generation, -1.0),
                PreviewWrite::Full,
                "冻结后音频线程不得再写入"
            );
        }
        assert_eq!(buffer.write_pos(), 冻结位置, "冻结后写指针不得前进");

        let clip = preview.take().expect("冻结后应可取回片段");
        assert_eq!(clip.samples.len(), 容量);
        assert_eq!(clip.sample_rate, 采样率);
        assert!(!clip.post_limiter, "采集侧片段尚未经过渲染限幅");
        assert_eq!(clip.samples[0], 0.0);
        assert_eq!(clip.samples[容量 - 1], (容量 - 1) as f32);
        assert!(!preview.is_finished(), "取走后记录区应可复用");
        assert!(preview.take().is_none(), "同一片段不得重复取回");
    }

    #[test]
    fn 试听记录区不可用时拒绝启动() {
        let runtime = Arc::new(VoiceRuntime::new());
        let control = VoiceControl::with_runtime(runtime, VoiceParams::neutral());
        let buffer = VoicePreviewBuffer::new(0, 3.0);
        let preview = VoicePreviewControl::new(&control, buffer);
        assert_eq!(preview.start(), Err(PreviewError::Unavailable));
        assert!(!preview.is_finished());
        assert!(preview.take().is_none());
    }

    #[test]
    fn 旧代次试听写入被丢弃() {
        let buffer = VoicePreviewBuffer::new(采样率, 3.0);
        buffer.begin(1);
        assert_eq!(buffer.push(1, 0.25), PreviewWrite::Written);
        assert_eq!(buffer.write_pos(), 1);

        buffer.begin(2);
        assert_eq!(
            buffer.push(1, 0.75),
            PreviewWrite::Rejected,
            "旧代次写入必须丢弃"
        );
        assert_eq!(buffer.write_pos(), 0, "旧代次不得推进新记录");
        assert_eq!(buffer.push(2, 0.5), PreviewWrite::Written);
        assert_eq!(buffer.snapshot(1), vec![0.5], "只保留新代次样本");
    }

    #[test]
    fn 试听新代次未归零前旧满记录不得接受写入() {
        let (runtime, control, buffer, preview) = 创建控制();
        let start = preview.start().expect("关闭状态应开始采集试听");
        let 容量 = buffer.capacity();
        for index in 0..容量 {
            let expected = if index + 1 == 容量 {
                PreviewWrite::Full
            } else {
                PreviewWrite::Written
            };
            assert_eq!(buffer.push(start.generation, index as f32), expected);
        }
        assert!(runtime.finish_preview(start.generation));
        assert!(preview.is_finished());

        // 冻结后未取走时切换预设取消旧记录；记录区仍保留旧代次的满位。
        control.set_params(VoiceParams {
            pitch_semitones: 4.0,
            ..VoiceParams::neutral()
        });
        assert!(!preview.is_finished());

        // 控制线程已发布新代次、尚未归零记录区：旧满位不得接受或冻结新记录。
        let new_generation = runtime
            .start_preview(VoicePreviewSource::Capture)
            .expect("取消后应能发布新试听代次");
        assert_eq!(
            buffer.push(new_generation, 1.0),
            PreviewWrite::Rejected,
            "旧满记录区不得接受新代次写入"
        );
        assert_eq!(buffer.write_pos(), 容量, "旧代次位置不得被新代次推进");
        assert!(!preview.is_finished(), "新记录不得因旧满位提前冻结");
        assert!(preview.take().is_none(), "尚未开始的新记录不可取回");

        // 归零后新记录从零开始。
        buffer.begin(new_generation);
        assert_eq!(buffer.push(new_generation, 1.0), PreviewWrite::Written);
        assert!(!preview.is_finished());
    }
}
