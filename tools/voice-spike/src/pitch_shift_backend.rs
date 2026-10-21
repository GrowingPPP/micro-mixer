use crate::backend::{Backend, BackendDiagnostics};
use pitch_shift::{RawState, Shifter};

struct OwnedState {
    ptr: *mut RawState,
}

impl AsMut<RawState> for OwnedState {
    fn as_mut(&mut self) -> &mut RawState {
        // SAFETY: ptr 来自根 Box::into_raw，在 OwnedState 释放前始终有效。
        unsafe { &mut *self.ptr }
    }
}

impl Drop for OwnedState {
    fn drop(&mut self) {
        // SAFETY: ptr 由 Box::into_raw 创建，只在这里回收一次。
        unsafe {
            drop(Box::from_raw(self.ptr));
        }
    }
}

/// 与 OwnedState 指向同一根分配，不负责释放。
#[derive(Clone, Copy)]
struct StateHandle {
    ptr: *mut RawState,
}

/// pitch_shift 2.1.0 加 NaN 恢复层，基准结果不代表纯上游行为。
pub struct PitchShiftBackend {
    shifter: Shifter<OwnedState>,
    state: StateHandle,
    sample_rate: f32,
    semitones: f32,
    latency_frames: usize,
    nan_recoveries: u64,
}

impl PitchShiftBackend {
    pub fn new(sample_rate: u32, semitones: f32) -> Self {
        let ptr = Box::into_raw(Box::new([0.0; pitch_shift::TOTAL_F32]));
        let state = StateHandle { ptr };
        Self {
            shifter: Shifter::new(OwnedState { ptr }),
            state,
            sample_rate: sample_rate as f32,
            semitones,
            latency_frames: 1024 - 128,
            nan_recoveries: 0,
        }
    }

    pub fn nan_recoveries(&self) -> u64 {
        self.nan_recoveries
    }

    fn clear_state(&mut self) {
        // pitch_shift 的相位提取在 FFT bin 恰为 (0, 0) 时产生 NaN，并会永久污染
        // 内部状态。这里直接清零根状态，避免实时路径重新分配。
        // SAFETY: 指针来自根 Box，调用前 shifter 的 &mut 借用已经结束。
        unsafe {
            (*self.state.ptr).fill(0.0);
        }
    }
}

impl Backend for PitchShiftBackend {
    fn name(&self) -> &'static str {
        "pitch_shift"
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        assert_eq!(input.len(), 128, "pitch_shift 只接受 128 样本块");
        assert!(output.len() >= 128, "pitch_shift 需要至少 128 样本输出缓冲");
        let n = {
            let shifted = self
                .shifter
                .shift(input, self.semitones, 128, self.sample_rate);
            let n = shifted.len().min(output.len());
            output[..n].copy_from_slice(&shifted[..n]);
            n
        };
        if output[..n].iter().any(|s| !s.is_finite()) {
            output[..n].fill(0.0);
            self.clear_state();
            self.nan_recoveries += 1;
        }
        n
    }

    fn latency_frames(&self) -> usize {
        self.latency_frames
    }

    fn reset(&mut self) {
        self.clear_state();
    }

    fn diagnostics(&self) -> BackendDiagnostics {
        BackendDiagnostics {
            nan_recoveries: self.nan_recoveries,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::signal::{harmonic_voice, sine_sweep};

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
        eprintln!("sine nan_recoveries={}", backend.nan_recoveries());
    }

    #[test]
    fn processes_harmonic_voice_with_valid_shifted_output() {
        let sr = 48_000;
        let semitones = 4.0;
        let input = harmonic_voice(sr, 2.0, 220.0);
        let mut backend = PitchShiftBackend::new(sr, semitones);
        let mut output = vec![0.0f32; 128];
        let mut wet = Vec::with_capacity(input.len());
        let mut blocks = 0usize;
        for chunk in input.chunks_exact(128) {
            output.fill(0.0);
            let produced = backend.process(chunk, &mut output);
            assert!(produced <= 128);
            wet.extend_from_slice(&output[..produced]);
            blocks += 1;
        }

        let recoveries = backend.nan_recoveries();
        let skip = backend.latency_frames().min(wet.len());
        let tail = &wet[skip..];
        let nonzero = tail.iter().filter(|s| s.abs() > 1e-4).count();
        let zero_blocks = wet
            .chunks(128)
            .skip(backend.latency_frames().div_ceil(128))
            .filter(|block| block.iter().all(|s| *s == 0.0))
            .count();
        let usable_blocks = blocks.saturating_sub(backend.latency_frames().div_ceil(128));
        // 谐波语音的零交叉率由最强谐波决定，不能直接当作基频；这里只记录，
        // 移调频率由下方正弦测试验证。
        let crossings_hz = zero_crossing_hz(tail, sr);
        eprintln!(
            "harmonic nan_recoveries={recoveries} zero_blocks={zero_blocks}/{usable_blocks} \
             nonzero_ratio={:.3} crossings_hz={crossings_hz:.1}",
            nonzero as f32 / tail.len() as f32
        );

        assert!(wet.iter().all(|s| s.is_finite()));
        assert!(
            (recoveries as usize) * 2 < usable_blocks,
            "pitch_shift 2.1.0 在当前输入下不可用：{recoveries}/{usable_blocks} 个块触发恢复"
        );
        assert!(
            nonzero > tail.len() / 2,
            "有效输出不足：{nonzero}/{}",
            tail.len()
        );
    }

    #[test]
    fn shifts_a_440_hz_sine_by_four_semitones() {
        let sr = 48_000;
        let semitones = 4.0;
        let input = sine_sweep(sr, 1.0, 440.0, 440.0);
        let mut backend = PitchShiftBackend::new(sr, semitones);
        let mut output = vec![0.0f32; 128];
        let mut wet = Vec::with_capacity(input.len());
        for chunk in input.chunks_exact(128) {
            output.fill(0.0);
            let produced = backend.process(chunk, &mut output);
            wet.extend_from_slice(&output[..produced]);
        }
        let skip = backend.latency_frames().min(wet.len());
        let tail = &wet[skip..];
        let hz = zero_crossing_hz(tail, sr);
        let expected = 440.0 * 2f32.powf(semitones / 12.0);
        let zero_blocks = wet
            .chunks(128)
            .skip(backend.latency_frames().div_ceil(128))
            .filter(|block| block.iter().all(|s| *s == 0.0))
            .count();
        let usable_blocks = input.len() / 128 - backend.latency_frames().div_ceil(128);
        eprintln!(
            "sine-shift nan_recoveries={} zero_blocks={zero_blocks}/{usable_blocks} \
             hz={hz:.1} expected={expected:.1}",
            backend.nan_recoveries()
        );
        assert!(wet.iter().all(|s| s.is_finite()));
        assert!(
            (backend.nan_recoveries() as usize) * 2 < usable_blocks,
            "pitch_shift 2.1.0 在当前输入下不可用：{}/{} 个块触发恢复",
            backend.nan_recoveries(),
            usable_blocks
        );
        assert!(
            (hz - expected).abs() < expected * 0.05,
            "hz={hz:.1}, expected={expected:.1}"
        );
    }

    fn zero_crossing_hz(samples: &[f32], sample_rate: u32) -> f32 {
        if samples.len() < 2 {
            return 0.0;
        }
        let mut crossings = 0usize;
        let mut previous = samples[0];
        for &sample in &samples[1..] {
            if (previous <= 0.0 && sample > 0.0) || (previous >= 0.0 && sample < 0.0) {
                crossings += 1;
            }
            previous = sample;
        }
        let seconds = samples.len() as f32 / sample_rate.max(1) as f32;
        crossings as f32 / (2.0 * seconds)
    }
}
