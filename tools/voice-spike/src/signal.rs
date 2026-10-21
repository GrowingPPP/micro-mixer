use anyhow::{Context, Result};
use hound::{WavSpec, WavWriter};
use std::f32::consts::TAU;
use std::path::Path;

pub fn sine_sweep(sample_rate: u32, seconds: f32, from_hz: f32, to_hz: f32) -> Vec<f32> {
    let frames = (sample_rate as f32 * seconds).round() as usize;
    let mut out = Vec::with_capacity(frames);
    let mut phase = 0.0f32;
    for i in 0..frames {
        let progress = if frames > 1 {
            i as f32 / (frames - 1) as f32
        } else {
            0.0
        };
        let hz = from_hz + (to_hz - from_hz) * progress;
        phase += TAU * hz / sample_rate.max(1) as f32;
        out.push((phase.sin() * 0.5).clamp(-1.0, 1.0));
    }
    out
}

pub fn harmonic_voice(sample_rate: u32, seconds: f32, f0: f32) -> Vec<f32> {
    let frames = (sample_rate as f32 * seconds).round() as usize;
    let formants = [(700.0f32, 1.0f32), (1_200.0, 0.6), (2_600.0, 0.25)];
    let mut out = Vec::with_capacity(frames);
    for i in 0..frames {
        let t = i as f32 / sample_rate.max(1) as f32;
        let mut sample = 0.0f32;
        for harmonic in 1..=40 {
            let hz = f0 * harmonic as f32;
            if hz >= sample_rate as f32 * 0.45 {
                break;
            }
            let mut amplitude = 0.0f32;
            for (center, gain) in formants {
                let width = (center * 0.18).max(60.0);
                let d = (hz - center) / width;
                amplitude += gain * (-0.5 * d * d).exp();
            }
            sample += amplitude * (TAU * hz * t).sin() / harmonic as f32;
        }
        out.push((sample * 0.18).clamp(-1.0, 1.0));
    }
    out
}

pub fn impulse_train(sample_rate: u32, seconds: f32, period_frames: usize) -> Vec<f32> {
    let frames = (sample_rate as f32 * seconds).round() as usize;
    let mut out = vec![0.0f32; frames];
    let period = period_frames.max(1);
    let mut i = 0;
    while i < frames {
        out[i] = 1.0;
        i += period;
    }
    out
}

pub fn write_wav(path: &Path, sample_rate: u32, samples: &[f32]) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录 {}", parent.display()))?;
    }
    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer =
        WavWriter::create(path, spec).with_context(|| format!("创建 WAV {}", path.display()))?;
    for &sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        writer.write_sample(value)?;
    }
    writer.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_sweep_has_expected_length_and_bounds() {
        let samples = sine_sweep(48_000, 0.5, 100.0, 1_000.0);
        assert_eq!(samples.len(), 24_000);
        assert!(samples.iter().all(|s| s.is_finite()));
        assert!(samples.iter().all(|s| s.abs() <= 1.0));
    }

    #[test]
    fn harmonic_voice_is_bounded_and_non_silent() {
        let samples = harmonic_voice(48_000, 0.25, 220.0);
        assert_eq!(samples.len(), 12_000);
        let peak = samples.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(peak > 0.05, "peak={peak}");
        assert!(peak <= 1.0);
    }

    #[test]
    fn impulse_train_places_requested_pulses() {
        let samples = impulse_train(48_000, 0.03, 480);
        let hits: Vec<usize> = samples
            .iter()
            .enumerate()
            .filter_map(|(i, s)| (*s > 0.9).then_some(i))
            .collect();
        assert_eq!(hits, vec![0, 480, 960]);
    }

    #[test]
    fn wav_round_trips_length_and_sample_rate() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("voice-spike-{}.wav", std::process::id()));
        let samples = sine_sweep(48_000, 0.01, 440.0, 440.0);
        write_wav(&path, 48_000, &samples).unwrap();
        let mut reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().sample_rate, 48_000);
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.samples::<i16>().count(), samples.len());
        let _ = std::fs::remove_file(path);
    }
}
