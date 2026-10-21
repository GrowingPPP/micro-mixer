use anyhow::{bail, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use voice_spike::bench::{render_ab, run_case, BackendChoice, CaseConfig};

#[derive(Debug, Parser)]
#[command(name = "voice-spike", about = "变声器技术验证基准")]
struct Args {
    #[arg(long, default_value_t = 30.0)]
    seconds: f32,
    #[arg(long, default_value_t = 48_000)]
    sample_rate: u32,
    #[arg(long, default_value = "10")]
    blocks: String,
    #[arg(long, default_value = "128,256,512")]
    chunks: String,
    #[arg(long, default_value = "all")]
    backend: String,
    #[arg(long)]
    json: Option<PathBuf>,
    #[arg(long)]
    render: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let blocks: Vec<f32> = args
        .blocks
        .split(',')
        .map(|v| v.trim().parse::<f32>())
        .collect::<std::result::Result<_, _>>()
        .context("解析 --blocks")?;
    let chunks: Vec<usize> = args
        .chunks
        .split(',')
        .map(|v| v.trim().parse::<usize>())
        .collect::<std::result::Result<_, _>>()
        .context("解析 --chunks")?;
    let choices = match args.backend.as_str() {
        "all" => vec![
            BackendChoice::Signalsmith,
            BackendChoice::PitchShift,
            BackendChoice::Passthrough,
        ],
        other => vec![BackendChoice::parse(other).with_context(|| format!("未知后端 {other}"))?],
    };

    let mut results = Vec::new();
    for choice in choices {
        let block_values: Vec<f32> = if choice == BackendChoice::PitchShift {
            vec![128.0 * 1000.0 / args.sample_rate as f32]
        } else {
            blocks.clone()
        };
        for block_ms in block_values {
            for chunk_frames in &chunks {
                let cfg = CaseConfig {
                    choice,
                    block_ms,
                    chunk_frames: *chunk_frames,
                    seconds: args.seconds,
                    sample_rate: args.sample_rate,
                    semitones: 4.0,
                    formant_semitones: 3.0,
                };
                let result = run_case(&cfg)?;
                println!(
                    "{:<12} block={:>5.1}ms chunk={:>4} lib={:>5} adapter={:>5} measured={:?} p50={:>6.3} p99={:>6.3} max={:>6.3} rt={:>6.2}x alloc={} under={}",
                    result.backend,
                    result.block_ms,
                    result.chunk_frames,
                    result.latency_backend_frames,
                    result.latency_adapter_frames,
                    result.latency_measured_frames,
                    result.p50_ms,
                    result.p99_ms,
                    result.max_ms,
                    result.realtime_factor,
                    result.allocations,
                    result.underruns,
                );
                results.push(result);
            }
        }
    }

    if let Some(path) = args.json {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&results)?;
        std::fs::write(&path, json).with_context(|| format!("写入 {}", path.display()))?;
        println!("JSON: {}", path.display());
    }

    if let Some(dir) = args.render {
        let cfg = CaseConfig {
            choice: BackendChoice::Signalsmith,
            block_ms: 10.0,
            chunk_frames: 480,
            seconds: 10.0,
            sample_rate: args.sample_rate,
            semitones: 4.0,
            formant_semitones: 3.0,
        };
        render_ab(&dir, &cfg)?;
        println!("A/B WAV: {}", dir.display());
    }

    if results.is_empty() {
        bail!("没有执行任何用例");
    }
    Ok(())
}
