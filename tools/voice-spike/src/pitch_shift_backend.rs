use crate::backend::Backend;
use pitch_shift::{RawState, Shifter};

pub struct PitchShiftBackend {
    shifter: Shifter<Box<RawState>>,
    state_ptr: *mut RawState,
    sample_rate: f32,
    semitones: f32,
    latency_frames: usize,
}

impl PitchShiftBackend {
    pub fn new(sample_rate: u32, semitones: f32) -> Self {
        let mut state: Box<RawState> = Box::new([0.0; pitch_shift::TOTAL_F32]);
        let state_ptr = state.as_mut() as *mut RawState;
        Self {
            shifter: Shifter::new(state),
            state_ptr,
            sample_rate: sample_rate as f32,
            semitones,
            latency_frames: 1024 - 128,
        }
    }

    fn clear_state(&mut self) {
        // pitch_shift 的相位提取在 FFT bin 恰为 (0, 0) 时产生 NaN，并会永久污染
        // 内部状态。这里直接清零原状态，避免实时路径重新分配。
        // SAFETY: state_ptr 指向 shifter 内部 Box 的堆分配，地址在 self 存活期间不变。
        unsafe {
            (*self.state_ptr).fill(0.0);
        }
    }
}

impl Backend for PitchShiftBackend {
    fn name(&self) -> &'static str {
        "pitch_shift"
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        assert_eq!(input.len(), 128, "pitch_shift 只接受 128 样本块");
        let shifted = self
            .shifter
            .shift(input, self.semitones, 128, self.sample_rate);
        let n = shifted.len().min(output.len());
        output[..n].copy_from_slice(&shifted[..n]);
        if output[..n].iter().any(|s| !s.is_finite()) {
            output[..n].fill(0.0);
            self.clear_state();
        }
        n
    }

    fn latency_frames(&self) -> usize {
        self.latency_frames
    }

    fn reset(&mut self) {
        self.clear_state();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::signal::sine_sweep;

    #[test]
    fn processes_128_sample_blocks_and_reports_latency() {
        let sr = 48_000;
        let mut backend = PitchShiftBackend::new(sr, 4.0);
        let input = sine_sweep(sr, 0.1, 440.0, 440.0);
        let mut output = vec![0.0f32; 128];
        let mut total = 0usize;
        for chunk in input.chunks_exact(128) {
            output.fill(0.0);
            let produced = backend.process(chunk, &mut output);
            assert!(produced <= 128);
            total += produced;
        }
        assert!(total > 0);
        assert_eq!(backend.latency_frames(), 1024 - 128);
        assert!(output.iter().all(|s| s.is_finite()));
    }
}
