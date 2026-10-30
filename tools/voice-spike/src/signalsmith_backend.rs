use crate::backend::Backend;
use signalsmith_stretch::Stretch;

pub struct SignalsmithBackend {
    stretch: Stretch,
    block_frames: usize,
    latency_frames: usize,
    semitones: f32,
    formant_semitones: f32,
}

impl SignalsmithBackend {
    pub fn new(sample_rate: u32, block_ms: f32, semitones: f32, formant_semitones: f32) -> Self {
        let block_frames = ((sample_rate as f32 * block_ms / 1000.0).round() as usize).max(64);
        let interval = (block_frames / 2).max(1);
        let mut stretch = Stretch::new(1, block_frames, interval);
        stretch.set_transpose_factor_semitones(semitones, None);
        stretch.set_formant_factor_semitones(formant_semitones, false);
        let latency_frames = stretch.input_latency() + stretch.output_latency();
        Self {
            stretch,
            block_frames,
            latency_frames,
            semitones,
            formant_semitones,
        }
    }

    pub fn block_frames(&self) -> usize {
        self.block_frames
    }

    pub fn formant_semitones(&self) -> f32 {
        self.formant_semitones
    }

    pub fn set_pitch(&mut self, semitones: f32, formant_semitones: f32) {
        self.semitones = semitones;
        self.formant_semitones = formant_semitones;
        self.stretch.set_transpose_factor_semitones(semitones, None);
        self.stretch
            .set_formant_factor_semitones(formant_semitones, false);
    }
}

impl Backend for SignalsmithBackend {
    fn name(&self) -> &'static str {
        "signalsmith"
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        let frames = input.len().min(output.len());
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::metrics::measure_allocations;
    use crate::signal::harmonic_voice;

    #[test]
    fn setter_does_not_allocate() {
        let mut backend = SignalsmithBackend::new(48_000, 10.0, 4.0, 3.0);
        let allocations = measure_allocations(|| backend.set_pitch(5.0, 2.0));
        assert_eq!(allocations, 0);
    }

    #[test]
    fn processes_a_second_of_voice_without_nan() {
        let sr = 48_000;
        let mut backend = SignalsmithBackend::new(sr, 10.0, 4.0, 3.0);
        let input = harmonic_voice(sr, 1.0, 220.0);
        let block = 480;
        let mut output = vec![0.0f32; block];
        let mut wet = Vec::with_capacity(input.len());
        for chunk in input.chunks(block) {
            output.fill(0.0);
            let produced = backend.process(chunk, &mut output);
            wet.extend_from_slice(&output[..produced]);
        }
        assert!(wet.iter().all(|s| s.is_finite()));
        assert!(wet.iter().any(|s| s.abs() > 1e-4));
        assert!(backend.latency_frames() > 0);
    }
}
