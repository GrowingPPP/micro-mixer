//! 固定块变声链：输入块累积、湿声 FIFO 与旁路。

use super::{VoiceBackend, VoiceParams};

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
}

impl VoiceChain {
    /// 创建固定块链；`block_frames` 必须大于 0。
    ///
    /// FIFO 容量按四个算法块预分配，用于吸收输出缓冲暂时不足造成的积压；
    /// 超过容量的最旧湿声会被丢弃并计数。
    pub fn new(backend: Box<dyn VoiceBackend>, block_frames: usize) -> Self {
        assert!(block_frames > 0, "块长必须大于 0");
        Self {
            backend,
            block_frames,
            input: vec![0.0; block_frames],
            input_fill: 0,
            wet: vec![0.0; block_frames],
            fifo: WetFifo::new(block_frames.saturating_mul(4)),
            bypass: false,
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
                let produced = self.backend.process(&self.input, &mut self.wet);
                self.input_fill = 0;
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

    /// 转发新参数，不重建后端。
    pub fn set_params(&mut self, params: VoiceParams) {
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

    /// 清空 FIFO、输入累积与后端状态。
    pub fn reset(&mut self) {
        self.fifo.clear();
        self.input_fill = 0;
        self.backend.reset();
    }

    /// FIFO 满时累计丢弃的帧数。
    pub fn overflowed_frames(&self) -> u64 {
        self.fifo.dropped()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use crate::test_support::count_allocations;

    const BLOCK: usize = 128;

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
}
