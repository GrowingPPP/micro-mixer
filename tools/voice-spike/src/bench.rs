use crate::adapter::BlockAdapter;
use crate::backend::Backend;
use crate::metrics::{measure_allocations, timing_stats};
use crate::pitch_shift_backend::PitchShiftBackend;
use crate::signal::{harmonic_voice, write_wav};
use crate::signalsmith_backend::SignalsmithBackend;
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendChoice {
    Signalsmith,
    PitchShift,
    Passthrough,
}

impl BackendChoice {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "signalsmith" => Some(Self::Signalsmith),
            "pitch" | "pitch_shift" => Some(Self::PitchShift),
            "passthrough" => Some(Self::Passthrough),
            _ => None,
        }
    }
}

impl Backend for Box<dyn Backend> {
    fn name(&self) -> &'static str {
        (**self).name()
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        (**self).process(input, output)
    }

    fn latency_frames(&self) -> usize {
        (**self).latency_frames()
    }

    fn reset(&mut self) {
        (**self).reset()
    }
}

#[derive(Debug, Clone)]
pub struct CaseConfig {
    pub choice: BackendChoice,
    pub block_ms: f32,
    pub chunk_frames: usize,
    pub seconds: f32,
    pub sample_rate: u32,
    pub semitones: f32,
    pub formant_semitones: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaseResult {
    pub backend: String,
    pub block_ms: f32,
    pub chunk_frames: usize,
    pub sample_rate: u32,
    pub seconds: f32,
    pub input_frames: usize,
    pub output_frames: usize,
    pub latency_backend_frames: usize,
    pub latency_adapter_frames: usize,
    pub latency_measured_frames: Option<usize>,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub realtime_factor: f64,
    pub allocations: usize,
    pub underruns: u64,
}

fn backend_block_frames(cfg: &CaseConfig) -> usize {
    match cfg.choice {
        BackendChoice::PitchShift => 128,
        _ => ((cfg.sample_rate as f32 * cfg.block_ms / 1000.0).round() as usize).max(1),
    }
}

fn make_backend(cfg: &CaseConfig) -> Box<dyn Backend> {
    match cfg.choice {
        BackendChoice::Signalsmith => Box::new(SignalsmithBackend::new(
            cfg.sample_rate,
            cfg.block_ms,
            cfg.semitones,
            cfg.formant_semitones,
        )),
        BackendChoice::PitchShift => {
            Box::new(PitchShiftBackend::new(cfg.sample_rate, cfg.semitones))
        }
        BackendChoice::Passthrough => Box::new(crate::backend::PassthroughBackend),
    }
}

fn measure_impulse_latency(cfg: &CaseConfig) -> Option<usize> {
    let block = backend_block_frames(cfg);
    let mut adapter = BlockAdapter::new(make_backend(cfg), block);
    let mut input = vec![0.0f32; cfg.chunk_frames.max(1)];
    input[0] = 1.0;
    let mut output = vec![0.0f32; cfg.chunk_frames.max(1)];
    let mut consumed = 0usize;
    let mut output_index = 0usize;
    let max_frames = cfg.sample_rate as usize * 2;
    while consumed < max_frames {
        output.fill(0.0);
        let produced = adapter.process(&input, &mut output);
        if let Some(pos) = output[..produced].iter().position(|s| s.abs() > 1e-6) {
            // 输出流里的位置 + 首次出样本的块缓冲延迟。
            return Some(output_index + pos + block);
        }
        output_index += produced;
        consumed += input.len();
        input.fill(0.0);
    }
    None
}

pub fn run_case(cfg: &CaseConfig) -> Result<CaseResult> {
    let signal = harmonic_voice(cfg.sample_rate, cfg.seconds, 220.0);
    let block = backend_block_frames(cfg);
    let backend = make_backend(cfg);
    let latency_backend = backend.latency_frames();
    let mut adapter = BlockAdapter::new(backend, block);
    let latency_adapter = adapter.latency_frames();
    let chunk = cfg.chunk_frames.max(1);
    let mut out = vec![0.0f32; chunk];
    let mut durations = Vec::with_capacity(signal.len().div_ceil(chunk));
    let mut underruns = 0u64;
    let mut output_frames = 0usize;
    let mut consumed_frames = 0usize;
    let warmup_chunks = block.div_ceil(chunk) + 2;
    let started = Instant::now();
    let allocations = measure_allocations(|| {
        for (index, input) in signal.chunks(chunk).enumerate() {
            out.fill(0.0);
            let start = Instant::now();
            let produced = adapter.process(input, &mut out);
            durations.push(start.elapsed());
            output_frames += produced;
            consumed_frames += input.len();
            // 块长与回调长度不整除时，单次回调短于请求长度属于块适配的
            // 正常粒度；只有积压超过适配器标称延迟才计为欠载。
            if index >= warmup_chunks && output_frames + latency_adapter < consumed_frames {
                underruns += 1;
            }
        }
        // 输入结束后排空适配器，把最后不足一块的输出计入总量。
        loop {
            out.fill(0.0);
            let produced = adapter.process(&[], &mut out);
            if produced == 0 {
                break;
            }
            output_frames += produced;
        }
    });
    let elapsed = started.elapsed().as_secs_f64();
    let stats = timing_stats(&durations);
    Ok(CaseResult {
        backend: cfg.choice_name().to_string(),
        block_ms: cfg.block_ms,
        chunk_frames: chunk,
        sample_rate: cfg.sample_rate,
        seconds: cfg.seconds,
        input_frames: signal.len(),
        output_frames,
        latency_backend_frames: latency_backend,
        latency_adapter_frames: latency_adapter,
        latency_measured_frames: measure_impulse_latency(cfg),
        p50_ms: stats.p50_ms,
        p99_ms: stats.p99_ms,
        max_ms: stats.max_ms,
        realtime_factor: output_frames as f64 / (elapsed * cfg.sample_rate as f64),
        allocations,
        underruns,
    })
}

impl CaseConfig {
    fn choice_name(&self) -> &'static str {
        match self.choice {
            BackendChoice::Signalsmith => "signalsmith",
            BackendChoice::PitchShift => "pitch_shift",
            BackendChoice::Passthrough => "passthrough",
        }
    }
}

pub fn render_ab(dir: &Path, cfg: &CaseConfig) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("创建目录 {}", dir.display()))?;
    let signal = harmonic_voice(cfg.sample_rate, 10.0, 220.0);
    write_wav(&dir.join("dry.wav"), cfg.sample_rate, &signal)?;
    for choice in [BackendChoice::Signalsmith, BackendChoice::PitchShift] {
        let mut case = cfg.clone();
        case.choice = choice;
        let block = backend_block_frames(&case);
        let mut adapter = BlockAdapter::new(make_backend(&case), block);
        let mut wet = Vec::with_capacity(signal.len());
        let mut out = vec![0.0f32; cfg.chunk_frames.max(1)];
        for chunk in signal.chunks(cfg.chunk_frames.max(1)) {
            out.fill(0.0);
            let produced = adapter.process(chunk, &mut out);
            wet.extend_from_slice(&out[..produced]);
        }
        let name = match choice {
            BackendChoice::Signalsmith => "wet-signalsmith.wav",
            BackendChoice::PitchShift => "wet-pitch-shift.wav",
            BackendChoice::Passthrough => "wet-passthrough.wav",
        };
        write_wav(&dir.join(name), cfg.sample_rate, &wet)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_case_produces_frames_and_zero_allocations() {
        let cfg = CaseConfig {
            choice: BackendChoice::Passthrough,
            block_ms: 10.0,
            chunk_frames: 128,
            seconds: 0.5,
            sample_rate: 48_000,
            semitones: 0.0,
            formant_semitones: 0.0,
        };
        let result = run_case(&cfg).unwrap();
        assert_eq!(result.input_frames, 24_000);
        assert_eq!(result.output_frames, 24_000);
        assert_eq!(result.allocations, 0);
        assert_eq!(result.underruns, 0);
        assert!(result.p99_ms >= 0.0);
    }
}
