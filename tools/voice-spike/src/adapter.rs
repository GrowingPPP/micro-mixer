use crate::backend::Backend;
use std::collections::VecDeque;

pub struct BlockAdapter<B: Backend> {
    backend: B,
    block_frames: usize,
    input: Vec<f32>,
    input_len: usize,
    output: VecDeque<f32>,
    wet: Vec<f32>,
    overflow: u64,
}

impl<B: Backend> BlockAdapter<B> {
    pub fn new(backend: B, block_frames: usize) -> Self {
        let block_frames = block_frames.max(1);
        Self {
            backend,
            block_frames,
            input: vec![0.0; block_frames],
            input_len: 0,
            output: VecDeque::with_capacity(block_frames * 8),
            wet: vec![0.0; block_frames],
            overflow: 0,
        }
    }

    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        let mut produced = 0;
        for &sample in input {
            self.input[self.input_len] = sample;
            self.input_len += 1;
            if self.input_len == self.block_frames {
                self.backend.process(&self.input, &mut self.wet);
                for &sample in &self.wet {
                    if self.output.len() == self.output.capacity() {
                        self.output.pop_front();
                        self.overflow += 1;
                    }
                    self.output.push_back(sample);
                }
                self.input_len = 0;
            }
            while produced < output.len() {
                let Some(sample) = self.output.pop_front() else {
                    break;
                };
                output[produced] = sample;
                produced += 1;
            }
        }
        produced
    }

    pub fn overflow_count(&self) -> u64 {
        self.overflow
    }

    pub fn latency_frames(&self) -> usize {
        self.backend.latency_frames() + self.block_frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{DelayBackend, PassthroughBackend};

    fn first_nonzero(samples: &[f32]) -> Option<usize> {
        samples.iter().position(|s| s.abs() > 1e-6)
    }

    #[test]
    fn passthrough_keeps_sample_count() {
        let mut adapter = BlockAdapter::new(PassthroughBackend, 128);
        let input = vec![0.25f32; 128];
        let mut output = vec![0.0f32; 128];
        let produced = adapter.process(&input, &mut output);
        assert_eq!(produced, 128);
        assert!(output.iter().all(|s| (*s - 0.25).abs() < 1e-6));
        assert_eq!(adapter.overflow_count(), 0);
    }

    #[test]
    fn delay_backend_aligns_expected_latency() {
        let mut adapter = BlockAdapter::new(DelayBackend::new(256), 128);
        let mut input = vec![0.0f32; 1024];
        input[0] = 1.0;
        let mut output = vec![0.0f32; 1024];
        let produced = adapter.process(&input, &mut output);
        assert_eq!(produced, 1024);
        // 输出索引里能看到的是后端延迟 256；适配器首次出样本的 128 帧
        // 体现在时间上，所以总延迟由 latency_frames() 给出 384。
        assert_eq!(first_nonzero(&output), Some(256));
        assert_eq!(adapter.latency_frames(), 384);
    }

    #[test]
    fn arbitrary_chunk_sizes_do_not_overflow() {
        let mut adapter = BlockAdapter::new(PassthroughBackend, 128);
        let mut total_in = 0usize;
        let mut total_out = 0usize;
        for chunk in [128usize, 256, 512] {
            let input = vec![0.1f32; chunk];
            let mut output = vec![0.0f32; chunk];
            total_in += chunk;
            total_out += adapter.process(&input, &mut output);
        }
        assert_eq!(total_out, total_in);
        assert_eq!(adapter.overflow_count(), 0);
    }
}
