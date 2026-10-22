//! 固定块变声链：输入块累积、湿声 FIFO 与旁路。

use super::{EffectChain, VoiceBackend, VoiceControl, VoiceParams};

/// 有界湿声 FIFO。
///
/// 容量在构造时一次分配；满时丢弃最旧样本并累计丢弃帧数，处理路径不扩容。
struct WetFifo {
    samples: Vec<f32>,
    head: usize,
    len: usize,
    dropped: u64,
}

impl WetFifo {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "FIFO 容量必须大于 0");
        Self {
            samples: vec![0.0; capacity],
            head: 0,
            len: 0,
            dropped: 0,
        }
    }

    fn push(&mut self, samples: &[f32]) {
        for &sample in samples {
            if self.len == self.samples.len() {
                self.head = (self.head + 1) % self.samples.len();
                self.len -= 1;
                self.dropped = self.dropped.saturating_add(1);
            }
            let tail = (self.head + self.len) % self.samples.len();
            self.samples[tail] = sample;
            self.len += 1;
        }
    }

    fn pop_into(&mut self, output: &mut [f32]) -> usize {
        let frames = self.len.min(output.len());
        for (offset, slot) in output[..frames].iter_mut().enumerate() {
            *slot = self.samples[(self.head + offset) % self.samples.len()];
        }
        self.head = (self.head + frames) % self.samples.len();
        self.len -= frames;
        frames
    }

    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// 把任意回调大小的输入适配到固定算法块，并支持逐样本旁路。
///
/// 输入累积满 `block_frames` 后才调用后端；湿声进入有界 FIFO，再按输出缓冲
/// 容量取出。实时路径不分配、不加锁、不做 IO。
pub struct VoiceChain {
    backend: Box<dyn VoiceBackend>,
    block_frames: usize,
    input: Vec<f32>,
    input_fill: usize,
    wet: Vec<f32>,
    fifo: WetFifo,
    bypass: bool,
    /// 效果链只在提供真实采样率的构造入口创建；兼容入口保持仅后端路径。
    effects: Option<EffectChain>,
    control: VoiceControl,
    applied_params: VoiceParams,
}

impl VoiceChain {
    /// 创建固定块链；`block_frames` 必须大于 0。
    ///
    /// 兼容入口只保留后端移调与共振峰路径，不创建效果链；需要噪声门、EQ、
    /// 软削波或环形调制时必须使用 [`Self::new_with_control`] 提供真实采样率。
    /// FIFO 容量按四个算法块预分配，用于吸收输出缓冲暂时不足造成的积压；
    /// 超过容量的最旧湿声会被丢弃并计数。
    pub fn new(backend: Box<dyn VoiceBackend>, block_frames: usize) -> Self {
        let control = VoiceControl::new(VoiceParams::neutral());
        let applied_params = control.params();
        Self::new_inner(backend, block_frames, None, control, applied_params)
    }

    /// 创建带共享参数快照与控制句柄的固定块链。
    ///
    /// 构造时把当前快照提交给效果链与后端；之后每个算法块边界读取一次快照，
    /// 只有参数变化时才更新效果系数与后端参数。效果顺序为噪声门 → 后端 →
    /// 均衡 → 软削波 → 环形调制。
    pub fn new_with_control(
        backend: Box<dyn VoiceBackend>,
        block_frames: usize,
        sample_rate: u32,
        control: VoiceControl,
    ) -> Self {
        let applied_params = control.params();
        let mut effects = EffectChain::new(sample_rate);
        effects.set_params(applied_params);
        Self::new_inner(
            backend,
            block_frames,
            Some(effects),
            control,
            applied_params,
        )
    }

    fn new_inner(
        backend: Box<dyn VoiceBackend>,
        block_frames: usize,
        effects: Option<EffectChain>,
        control: VoiceControl,
        applied_params: VoiceParams,
    ) -> Self {
        assert!(block_frames > 0, "块长必须大于 0");
        let mut backend = backend;
        backend.set_params(applied_params);

        Self {
            backend,
            block_frames,
            input: vec![0.0; block_frames],
            input_fill: 0,
            wet: vec![0.0; block_frames],
            fifo: WetFifo::new(block_frames.saturating_mul(4)),
            bypass: false,
            effects,
            control,
            applied_params,
        }
    }

    /// 喂入输入并写出湿声，返回写入 `output` 的帧数。
    ///
    /// 输入始终被完整消费；输出不足时湿声留在有界 FIFO 中，满则丢弃最旧样本。
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        if self.bypass {
            let frames = input.len().min(output.len());
            output[..frames].copy_from_slice(&input[..frames]);
            return frames;
        }

        let mut consumed = 0;
        let mut written = 0;
        while consumed < input.len() {
            let take = (self.block_frames - self.input_fill).min(input.len() - consumed);
            self.input[self.input_fill..self.input_fill + take]
                .copy_from_slice(&input[consumed..consumed + take]);
            self.input_fill += take;
            consumed += take;

            if self.input_fill == self.block_frames {
                self.apply_pending_params();
                if let Some(effects) = self.effects.as_mut() {
                    effects.process_gate(&mut self.input);
                }
                let produced = self.backend.process(&self.input, &mut self.wet);
                self.input_fill = 0;
                if let Some(effects) = self.effects.as_mut() {
                    effects.process_post(&mut self.wet[..produced]);
                }
                self.fifo.push(&self.wet[..produced]);
                written += self.fifo.pop_into(&mut output[written..]);
            }
        }
        written += self.fifo.pop_into(&mut output[written..]);
        written
    }

    /// 算法延迟：后端报告值加一个块，覆盖输入累积带来的最坏对齐延迟。
    pub fn latency_frames(&self) -> usize {
        self.backend
            .latency_frames()
            .saturating_add(self.block_frames)
    }

    /// 发布新参数，不重建后端；下一次块边界生效。
    ///
    /// 该入口与共享控制句柄使用同一份快照，内部会串行化控制侧写入，
    /// 不能在音频回调线程调用。
    pub fn set_params(&mut self, params: VoiceParams) {
        self.control.set_params(params);
    }

    /// 在算法块边界读取一次快照；参数未变化时不重复提交。
    fn apply_pending_params(&mut self) {
        let params = self.control.param_buffer().read(self.applied_params);
        if params == self.applied_params {
            return;
        }
        self.applied_params = params;
        if let Some(effects) = self.effects.as_mut() {
            effects.set_params(params);
        }
        self.backend.set_params(params);
    }

    /// 切换旁路。
    ///
    /// 进入旁路时清空残留 FIFO 与未成块的输入，避免恢复处理时混入旧样本。
    pub fn set_bypass(&mut self, bypass: bool) {
        if bypass && !self.bypass {
            self.fifo.clear();
            self.input_fill = 0;
        }
        self.bypass = bypass;
    }

    /// 清空 FIFO、输入累积、效果状态与后端状态。
    ///
    /// 参数快照保持不变；重置后的效果链与新建并提交同一快照时一致。
    pub fn reset(&mut self) {
        self.fifo.clear();
        self.input_fill = 0;
        self.backend.reset();
        if let Some(effects) = self.effects.as_mut() {
            effects.reset();
        }
    }

    /// FIFO 满时累计丢弃的帧数。
    pub fn overflowed_frames(&self) -> u64 {
        self.fifo.dropped()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use super::*;
    use crate::test_support::count_allocations;
    use crate::voice::VoiceControl;

    const BLOCK: usize = 128;
    const SAMPLE_RATE: u32 = 48_000;

    struct DelayBackend {
        delay_frames: usize,
        line: Vec<f32>,
        position: usize,
        calls: Rc<Cell<usize>>,
    }

    impl DelayBackend {
        fn new(delay_frames: usize) -> Self {
            Self {
                delay_frames,
                line: vec![0.0; delay_frames],
                position: 0,
                calls: Rc::new(Cell::new(0)),
            }
        }

        fn calls(&self) -> Rc<Cell<usize>> {
            self.calls.clone()
        }
    }

    impl VoiceBackend for DelayBackend {
        fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
            self.calls.set(self.calls.get() + 1);
            let frames = input.len().min(output.len());
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

    fn 创建链(delay_frames: usize) -> (VoiceChain, Rc<Cell<usize>>) {
        let backend = DelayBackend::new(delay_frames);
        let calls = backend.calls();
        (VoiceChain::new(Box::new(backend), BLOCK), calls)
    }

    #[test]
    fn 旁路逐样本一致() {
        let (mut chain, calls) = 创建链(7);
        let mut output = vec![0.0; BLOCK];

        let first: Vec<f32> = (0..BLOCK).map(|frame| frame as f32).collect();
        assert_eq!(chain.process(&first, &mut output[..1]), 1);
        assert_eq!(calls.get(), 1, "进入旁路前应调用一次后端");

        chain.set_bypass(true);
        let input: Vec<f32> = (0..37).map(|frame| (frame as f32 * 0.25).sin()).collect();
        let mut bypassed = vec![0.0; input.len()];
        assert_eq!(chain.process(&input, &mut bypassed), input.len());
        assert_eq!(bypassed, input);
        assert_eq!(calls.get(), 1, "旁路时不得调用后端");
    }

    #[test]
    fn 任意回调大小保持样本数() {
        let (mut chain, _calls) = 创建链(5);
        let mut produced = 0;

        for &frames in &[1, 127, 100, 3, 25] {
            let input = vec![0.5; frames];
            let mut output = vec![0.0; BLOCK];
            produced += chain.process(&input, &mut output);
        }

        assert_eq!(produced, 2 * BLOCK);
        assert_eq!(chain.overflowed_frames(), 0);
    }

    #[test]
    fn 输出缓冲不足时有界丢弃() {
        let (mut chain, _calls) = 创建链(0);
        let input = vec![0.25; BLOCK];
        let mut output = vec![0.0; 1];
        let mut produced = 0;

        for _ in 0..20 {
            produced += chain.process(&input, &mut output);
        }

        assert_eq!(produced, 20);
        assert!(chain.overflowed_frames() > 0);
    }

    #[test]
    fn 块处理零分配() {
        let (mut chain, _calls) = 创建链(3);
        let input = vec![0.1; BLOCK];
        let mut output = vec![0.0; BLOCK];

        assert_eq!(
            count_allocations(|| {
                chain.process(&input, &mut output);
            }),
            0
        );
    }

    #[test]
    fn 延迟等于后端延迟加一个块() {
        let (chain, _calls) = 创建链(11);

        assert_eq!(chain.latency_frames(), 11 + BLOCK);
    }

    struct ProbeBackend {
        transform: fn(f32) -> f32,
    }

    impl VoiceBackend for ProbeBackend {
        fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
            let frames = input.len().min(output.len());
            for (slot, sample) in output[..frames].iter_mut().zip(&input[..frames]) {
                *slot = (self.transform)(*sample);
            }
            frames
        }

        fn latency_frames(&self) -> usize {
            0
        }

        fn reset(&mut self) {}

        fn set_params(&mut self, _params: VoiceParams) {}
    }

    fn 乘二(sample: f32) -> f32 {
        sample * 2.0
    }

    fn 乘百(sample: f32) -> f32 {
        sample * 100.0
    }

    fn 平方(sample: f32) -> f32 {
        sample * sample
    }

    fn 创建参数链(params: VoiceParams, transform: fn(f32) -> f32) -> VoiceChain {
        VoiceChain::new_with_control(
            Box::new(ProbeBackend { transform }),
            BLOCK,
            SAMPLE_RATE,
            VoiceControl::new(params),
        )
    }

    fn 处理恒定输入(chain: &mut VoiceChain, value: f32, blocks: usize) -> Vec<f32> {
        let input = vec![value; BLOCK];
        let mut output = vec![0.0; BLOCK];
        let mut collected = Vec::with_capacity(BLOCK * blocks);
        for _ in 0..blocks {
            let written = chain.process(&input, &mut output);
            collected.extend_from_slice(&output[..written]);
        }
        collected
    }

    fn 峰值(samples: &[f32]) -> f32 {
        samples
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()))
    }

    #[test]
    fn 效果顺序为门限移调均衡削波环形调制() {
        // 门限在移调之前：低于门限的信号不应被后端放大。
        let mut gate_first = 创建参数链(
            VoiceParams {
                gate_enabled: true,
                gate_db: -20.0,
                ..VoiceParams::neutral()
            },
            乘百,
        );
        let gated = 处理恒定输入(&mut gate_first, 0.01, 200);
        let gated_tail = &gated[gated.len() - BLOCK * 20..];
        assert!(
            峰值(gated_tail) < 1e-6,
            "门限应先于移调，门限后峰值 {}",
            峰值(gated_tail)
        );

        // 软削波在移调之后：后端先把信号放大到 1 再削波，输出约为 1；
        // 若削波在前，后端会把削波结果再放大到约 2。
        let mut clip_after = 创建参数链(
            VoiceParams {
                drive_enabled: true,
                drive: 1.0,
                ..VoiceParams::neutral()
            },
            乘二,
        );
        let clipped = 处理恒定输入(&mut clip_after, 0.5, 200);
        let clipped_peak = 峰值(&clipped[clipped.len() - BLOCK * 20..]);
        assert!(
            (0.99..=1.0).contains(&clipped_peak),
            "软削波应在移调之后，稳态峰值 {clipped_peak}"
        );

        // 均衡在移调之后：后端平方得到 0.25，低频搁架 +12 dB 后约为 1；
        // 若均衡在前，平方后的结果约为 4。
        let mut eq_after = 创建参数链(
            VoiceParams {
                eq_low_enabled: true,
                eq_low_db: 12.0,
                ..VoiceParams::neutral()
            },
            平方,
        );
        let equalized = 处理恒定输入(&mut eq_after, 0.5, 400);
        let equalized_tail = &equalized[equalized.len() - BLOCK * 20..];
        let equalized_steady = equalized_tail[equalized_tail.len() / 2];
        assert!(
            (0.9..1.1).contains(&equalized_steady),
            "均衡应在移调之后，稳态 {equalized_steady}"
        );

        // 环形调制在移调之后：后端平方得到恒定正信号，调制后应出现过零的负样本。
        let mut robot_after = 创建参数链(
            VoiceParams {
                robot_enabled: true,
                robot_hz: 50.0,
                robot_mix: 1.0,
                ..VoiceParams::neutral()
            },
            平方,
        );
        let modulated = 处理恒定输入(&mut robot_after, 0.5, 400);
        let modulated_tail = &modulated[modulated.len() - BLOCK * 60..];
        let low = modulated_tail
            .iter()
            .fold(f32::INFINITY, |low, sample| low.min(*sample));
        let high = modulated_tail
            .iter()
            .fold(f32::NEG_INFINITY, |high, sample| high.max(*sample));
        assert!(
            low < -0.2 && high > 0.2,
            "环形调制应在移调之后，范围 {low}..{high}"
        );
    }

    #[test]
    fn 效果全关时湿声与仅移调一致() {
        let params = VoiceParams {
            pitch_semitones: 7.0,
            formant_semitones: -4.0,
            ..VoiceParams::neutral()
        };
        let control = VoiceControl::new(params);
        let mut chain = VoiceChain::new_with_control(
            Box::new(DelayBackend::new(5)),
            BLOCK,
            SAMPLE_RATE,
            control,
        );
        let mut reference = DelayBackend::new(5);
        let mut output = vec![0.0; BLOCK];
        let mut expected = vec![0.0; BLOCK];

        for block in 0..32 {
            let input: Vec<f32> = (0..BLOCK)
                .map(|index| ((block * BLOCK + index) as f32 * 0.01).sin() * 0.4)
                .collect();
            let written = chain.process(&input, &mut output);
            let produced = reference.process(&input, &mut expected);
            assert_eq!(written, produced);
            assert_eq!(
                &output[..written],
                &expected[..produced],
                "效果全关时湿声必须与仅移调路径逐样本一致"
            );
        }
    }

    #[test]
    fn 旁路不调用后端且效果状态不推进() {
        let control = VoiceControl::new(VoiceParams {
            gate_enabled: true,
            gate_db: -20.0,
            ..VoiceParams::neutral()
        });
        let backend = DelayBackend::new(0);
        let calls = backend.calls();
        let mut chain =
            VoiceChain::new_with_control(Box::new(backend), BLOCK, SAMPLE_RATE, control);
        let mut output = vec![0.0; BLOCK];

        // 先让噪声门完全打开，记录冻结前的效果状态。
        let loud = vec![0.9f32; BLOCK];
        for _ in 0..40 {
            chain.process(&loud, &mut output);
        }

        chain.set_bypass(true);
        let calls_before = calls.get();
        let silence = vec![0.0f32; BLOCK];
        for _ in 0..200 {
            let written = chain.process(&silence, &mut output);
            assert_eq!(written, BLOCK);
            assert_eq!(output, silence, "旁路必须逐样本直通");
        }
        assert_eq!(calls.get(), calls_before, "旁路不得调用后端");

        // 解除旁路后立即输入低于门限的信号。若旁路期间推进了门限状态，
        // 门限应已关闭并把信号衰减；冻结状态下首个样本仍按原增益通过。
        chain.set_bypass(false);
        let quiet = vec![0.05f32; BLOCK];
        chain.process(&quiet, &mut output);
        assert!(
            output[0].abs() > 0.04,
            "旁路期间不得推进效果状态，首个样本 {}",
            output[0]
        );
    }

    struct RecordingBackend {
        params: VoiceParams,
        seen: Rc<RefCell<Vec<VoiceParams>>>,
    }

    impl VoiceBackend for RecordingBackend {
        fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
            self.seen.borrow_mut().push(self.params);
            let frames = input.len().min(output.len());
            output[..frames].copy_from_slice(&input[..frames]);
            frames
        }

        fn latency_frames(&self) -> usize {
            0
        }

        fn reset(&mut self) {}

        fn set_params(&mut self, params: VoiceParams) {
            self.params = params;
        }
    }

    #[test]
    fn 参数在块边界整块生效() {
        let control = VoiceControl::new(VoiceParams::neutral());
        let seen = Rc::new(RefCell::new(Vec::new()));
        let backend = RecordingBackend {
            params: VoiceParams::neutral(),
            seen: seen.clone(),
        };
        let mut chain =
            VoiceChain::new_with_control(Box::new(backend), BLOCK, SAMPLE_RATE, control.clone());
        let mut output = vec![0.0; BLOCK];

        control.set_params(预设甲());
        let half = vec![0.25f32; BLOCK / 2];
        assert_eq!(chain.process(&half, &mut output), 0, "未成块不应产生湿声");
        assert!(seen.borrow().is_empty(), "未到块边界不应读取参数");

        control.set_params(预设乙());
        assert_eq!(chain.process(&half, &mut output), BLOCK, "补齐一块");
        assert_eq!(
            seen.borrow().as_slice(),
            &[预设乙()],
            "块边界应读取最近一次完整快照"
        );
    }

    struct CellParamBackend {
        applied: Rc<Cell<VoiceParams>>,
    }

    impl VoiceBackend for CellParamBackend {
        fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
            let frames = input.len().min(output.len());
            output[..frames].copy_from_slice(&input[..frames]);
            frames
        }

        fn latency_frames(&self) -> usize {
            0
        }

        fn reset(&mut self) {}

        fn set_params(&mut self, params: VoiceParams) {
            self.applied.set(params);
        }
    }

    #[test]
    fn 参数生效块零分配() {
        let control = VoiceControl::new(VoiceParams::neutral());
        let applied = Rc::new(Cell::new(VoiceParams::neutral()));
        let mut chain = VoiceChain::new_with_control(
            Box::new(CellParamBackend {
                applied: applied.clone(),
            }),
            BLOCK,
            SAMPLE_RATE,
            control.clone(),
        );
        let input = vec![0.2f32; BLOCK];
        let mut output = vec![0.0f32; BLOCK];
        assert_eq!(chain.process(&input, &mut output), BLOCK);

        let params = VoiceParams {
            gate_enabled: true,
            gate_db: -40.0,
            eq_low_enabled: true,
            eq_low_db: 3.0,
            eq_mid_enabled: true,
            eq_mid_db: -2.0,
            eq_high_enabled: true,
            eq_high_db: 4.0,
            drive_enabled: true,
            drive: 0.7,
            robot_enabled: true,
            robot_hz: 120.0,
            robot_mix: 0.6,
            ..VoiceParams::neutral()
        };
        control.set_params(params);

        assert_eq!(
            count_allocations(|| {
                chain.process(&input, &mut output);
            }),
            0,
            "参数生效块不得分配"
        );
        assert_eq!(applied.get(), params, "生效块必须提交新快照");
    }

    #[test]
    fn 有源效果整块处理零分配() {
        let params = VoiceParams {
            gate_enabled: true,
            gate_db: -40.0,
            eq_low_enabled: true,
            eq_low_db: 3.0,
            eq_mid_enabled: true,
            eq_mid_db: -2.0,
            eq_high_enabled: true,
            eq_high_db: 4.0,
            drive_enabled: true,
            drive: 0.7,
            robot_enabled: true,
            robot_hz: 120.0,
            robot_mix: 0.6,
            ..VoiceParams::neutral()
        };
        let control = VoiceControl::new(params);
        let mut chain = VoiceChain::new_with_control(
            Box::new(DelayBackend::new(3)),
            BLOCK,
            SAMPLE_RATE,
            control,
        );
        let input = vec![0.25f32; BLOCK];
        let mut output = vec![0.0f32; BLOCK];

        assert_eq!(
            count_allocations(|| {
                chain.process(&input, &mut output);
            }),
            0,
            "有源效果的整块处理不得分配"
        );
    }

    fn 预设甲() -> VoiceParams {
        VoiceParams {
            pitch_semitones: 6.0,
            formant_semitones: -4.0,
            gate_enabled: true,
            gate_db: -28.0,
            eq_low_enabled: true,
            eq_low_db: 2.0,
            eq_mid_enabled: true,
            eq_mid_db: -1.0,
            eq_high_enabled: true,
            eq_high_db: 3.0,
            drive_enabled: true,
            drive: 0.3,
            robot_enabled: true,
            robot_hz: 70.0,
            robot_mix: 0.5,
        }
    }

    fn 预设乙() -> VoiceParams {
        VoiceParams {
            pitch_semitones: -8.0,
            formant_semitones: 5.0,
            gate_enabled: false,
            gate_db: -50.0,
            eq_low_enabled: false,
            eq_low_db: -4.0,
            eq_mid_enabled: false,
            eq_mid_db: 5.0,
            eq_high_enabled: false,
            eq_high_db: -6.0,
            drive_enabled: false,
            drive: 0.8,
            robot_enabled: false,
            robot_hz: 800.0,
            robot_mix: 0.2,
        }
    }
}
