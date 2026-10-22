//! Signalsmith Stretch 变声后端。

use anyhow::{ensure, Result};
use signalsmith_stretch::Stretch;

use super::{VoiceBackend, VoiceParams};

/// 基于 Signalsmith Stretch 的单声道变声后端。
pub struct SignalsmithBackend {
    stretch: Stretch,
    block_frames: usize,
    latency_frames: usize,
}

impl SignalsmithBackend {
    /// 按采样率与目标块时长创建后端。
    ///
    /// 块长按采样率换算后取整，且不低于 64 帧；分析间隔为块长一半。
    pub fn new(sample_rate: u32, block_ms: f32) -> Result<Self> {
        ensure!(sample_rate > 0, "采样率必须大于 0");
        ensure!(
            block_ms.is_finite() && block_ms > 0.0,
            "块时长必须为有限正数"
        );

        let block_frames = ((sample_rate as f32 * block_ms / 1000.0).round() as usize).max(64);
        let interval = block_frames / 2;
        let mut stretch = Stretch::new(1, block_frames, interval);
        stretch.set_transpose_factor_semitones(0.0, None);
        stretch.set_formant_factor_semitones(0.0, false);
        let latency_frames = stretch.input_latency() + stretch.output_latency();
        Ok(Self {
            stretch,
            block_frames,
            latency_frames,
        })
    }

    /// 算法块长（帧）。
    pub fn block_frames(&self) -> usize {
        self.block_frames
    }
}

impl VoiceBackend for SignalsmithBackend {
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        let frames = input.len().min(output.len());
        if frames == 0 {
            return 0;
        }
        self.stretch
            .process(&input[..frames], &mut output[..frames]);
        frames
    }

    fn latency_frames(&self) -> usize {
        self.latency_frames
    }

    fn reset(&mut self) {
        self.stretch.reset();
    }

    fn set_params(&mut self, params: VoiceParams) {
        self.stretch
            .set_transpose_factor_semitones(params.pitch_semitones, None);
        self.stretch
            .set_formant_factor_semitones(params.formant_semitones, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::count_allocations;

    const SAMPLE_RATE: u32 = 48_000;
    const BLOCK_MS: f32 = 10.0;

    fn 谐波语音(seconds: f32) -> Vec<f32> {
        let frames = (SAMPLE_RATE as f32 * seconds) as usize;
        let mut samples = Vec::with_capacity(frames);
        for frame in 0..frames {
            let time = frame as f32 / SAMPLE_RATE as f32;
            let mut sample = 0.0;
            for harmonic in 1..=8 {
                let frequency = 220.0 * harmonic as f32;
                sample += (std::f32::consts::TAU * frequency * time).sin() / harmonic as f32;
            }
            samples.push(sample * 0.1);
        }
        samples
    }

    #[test]
    fn 后端输出有限且非静音() {
        let mut backend = SignalsmithBackend::new(SAMPLE_RATE, BLOCK_MS).expect("创建后端");
        let input = 谐波语音(1.0);
        let block_frames = backend.block_frames();
        let mut output = vec![0.0; block_frames];
        let mut wet = Vec::with_capacity(input.len());

        for chunk in input.chunks(block_frames) {
            let produced = backend.process(chunk, &mut output[..chunk.len()]);
            wet.extend_from_slice(&output[..produced]);
        }

        assert_eq!(wet.len(), input.len());
        assert!(wet.iter().all(|sample| sample.is_finite()));
        assert!(wet.iter().any(|sample| sample.abs() > 1e-4));
        assert!(backend.latency_frames() > 0);
    }

    #[test]
    fn 设置变声参数不分配() {
        let mut backend = SignalsmithBackend::new(SAMPLE_RATE, BLOCK_MS).expect("创建后端");
        let params = VoiceParams {
            pitch_semitones: 4.0,
            formant_semitones: -3.0,
            ..VoiceParams::neutral()
        };

        assert_eq!(count_allocations(|| backend.set_params(params)), 0);
    }
}
