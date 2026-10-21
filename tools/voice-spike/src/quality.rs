pub fn midi_ratio(semitones: f32) -> f32 {
    2.0f32.powf(semitones / 12.0)
}

pub fn estimate_frequency(samples: &[f32], sample_rate: u32) -> f32 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::BlockAdapter;
    use crate::pitch_shift_backend::PitchShiftBackend;
    use crate::signal::sine_sweep;

    #[test]
    fn estimates_a_440_hz_sine() {
        let samples = sine_sweep(48_000, 0.5, 440.0, 440.0);
        let hz = estimate_frequency(&samples, 48_000);
        assert!((hz - 440.0).abs() < 5.0, "hz={hz}");
    }

    #[test]
    fn pitch_shift_raises_440_to_octave() {
        let sr = 48_000;
        let input = sine_sweep(sr, 1.0, 440.0, 440.0);
        let mut adapter = BlockAdapter::new(PitchShiftBackend::new(sr, 12.0), 128);
        let mut out = vec![0.0f32; 128];
        let mut wet = Vec::new();
        for chunk in input.chunks(128) {
            out.fill(0.0);
            let produced = adapter.process(chunk, &mut out);
            wet.extend_from_slice(&out[..produced]);
        }
        let skip = adapter.latency_frames().min(wet.len());
        let hz = estimate_frequency(&wet[skip..], sr);
        assert!((hz - 880.0).abs() < 30.0, "hz={hz}");
    }

    #[test]
    fn semitone_ratio_matches_octave() {
        assert!((midi_ratio(12.0) - 2.0).abs() < 1e-6);
        assert!((midi_ratio(-12.0) - 0.5).abs() < 1e-6);
    }
}
