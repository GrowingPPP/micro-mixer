//! 离线全链路探针：解码 → 引擎 → 输出缓冲，**完全不碰音频设备**。
//!
//! 用来判断问题出在"引擎+解码"还是"设备交互"。
//!
//! ```bash
//! cargo run --release -p audio-core --example pipeline_probe -- m0/work/sfx_笑声.wav
//! ```

use audio_core::decode::decode_file;
use audio_core::device::StreamFormat;
use audio_core::engine::{EngineParts, SfxSlot};
use std::path::Path;
use std::sync::Arc;

fn zerocross_freq(samples: &[f32], sr: u32) -> f32 {
    if samples.len() < 2 {
        return 0.0;
    }
    let zc = samples
        .windows(2)
        .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
        .count();
    zc as f32 / 2.0 / (samples.len() as f32 / sr as f32)
}

fn dft_peak(samples: &[f32], sr: u32, lo: f32, hi: f32) -> f32 {
    let n = samples.len();
    if n < 1024 {
        return 0.0;
    }
    let mean = samples.iter().sum::<f32>() / n as f32;
    let win: Vec<f32> = (0..n)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos())
        .collect();
    let mut best = (0f32, 0f32);
    let mut f = lo;
    while f <= hi {
        let w = 2.0 * std::f32::consts::PI * f / sr as f32;
        let (mut re, mut im) = (0f32, 0f32);
        for (i, s) in samples.iter().enumerate() {
            let a = w * i as f32;
            let v = (s - mean) * win[i];
            re += v * a.cos();
            im += v * a.sin();
        }
        let m = (re * re + im * im).sqrt();
        if m > best.1 {
            best = (f, m);
        }
        f += 0.5;
    }
    best.0
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "m0/work/sfx_笑声.wav".into());
    let sr = 48_000u32;

    // 1. 解码
    let mut audio = decode_file(Path::new(&path), sr)?;
    audio.normalize_peak(0.708);
    println!(
        "源文件解码后：{:.3}s  峰值 {:.1} dBFS",
        audio.duration_secs,
        20.0 * audio.peak().max(1e-9).log10()
    );
    let src_mono: Vec<f32> = audio
        .samples
        .as_chunks::<2>()
        .0
        .iter()
        .map(|f| f[0])
        .collect();
    let mid = &src_mono[src_mono.len() / 4..src_mono.len() / 4 + src_mono.len() / 2];
    println!(
        "  解码后频率: 零交叉 {:.1} Hz / DFT {:.0} Hz",
        zerocross_freq(mid, sr),
        dft_peak(mid, sr, 50.0, 4000.0)
    );

    // 2. 引擎
    let slot = SfxSlot {
        name: "probe".into(),
        samples: Arc::new(audio.samples.clone()),
        looping: false,
        gain_db: 0.0,
    };
    let mut parts = EngineParts::build(
        StreamFormat::new(sr, 1),
        StreamFormat::new(sr, 2),
        vec![slot],
        20,
    );
    parts.control.sfx_gain_db.set(0.0);
    parts.control.mic_gain_db.set(-60.0); // 关掉麦克风，只留音效
    parts.control.play(0);

    // 3. 以 512 帧为单位渲染，模拟真实回调
    const FRAMES: usize = 512;
    let mut out = vec![0f32; FRAMES * 2];
    let mut mono: Vec<f32> = Vec::new();
    let total_frames = (audio.duration_secs * sr as f32) as usize;
    let mut rendered = 0usize;
    while rendered < total_frames {
        parts.render.process(&mut out);
        mono.extend(out.as_chunks::<2>().0.iter().map(|f| f[0]));
        rendered += FRAMES;
    }

    println!(
        "\n引擎输出：{} 帧 ({:.3}s)",
        mono.len(),
        mono.len() as f32 / sr as f32
    );
    // 跳过淡入，取中段
    let a = mono.len() / 4;
    let b = a + mono.len() / 2;
    let seg = &mono[a..b];
    println!(
        "  引擎输出频率: 零交叉 {:.1} Hz / DFT {:.0} Hz",
        zerocross_freq(seg, sr),
        dft_peak(seg, sr, 50.0, 4000.0)
    );

    // 4. 检查是否有削波（会引入谐波）
    let peak = mono.iter().fold(0f32, |m, s| m.max(s.abs()));
    let clipped = mono.iter().filter(|s| s.abs() > 0.999).count();
    println!("  输出峰值 {:.3}  削波样本数 {}", peak, clipped);

    Ok(())
}
