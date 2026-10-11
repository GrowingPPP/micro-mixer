//! 解码探针：检查 `decode_file` 的输出是否与源文件一致。
//!
//! ```bash
//! cargo run --release -p audio-core --example decode_probe -- m0/work/sfx_笑声.wav
//! ```

use audio_core::decode::decode_file;
use std::path::Path;

fn zerocross_freq(samples: &[f32], sr: u32) -> f32 {
    let zc = samples
        .windows(2)
        .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
        .count();
    zc as f32 / 2.0 / (samples.len() as f32 / sr as f32)
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "m0/work/sfx_笑声.wav".into());
    let target: u32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(48_000);

    let t0 = std::time::Instant::now();
    let a = decode_file(Path::new(&path), target)?;
    println!("文件      : {path}");
    println!("目标采样率: {target}");
    println!("解码耗时  : {} ms", t0.elapsed().as_millis());
    println!("输出采样率: {}", a.sample_rate);
    println!("输出样本数: {} ({} 帧)", a.samples.len(), a.frames());
    println!("时长      : {:.3} s", a.duration_secs);
    println!("峰值      : {:.2} dBFS", 20.0 * a.peak().max(1e-9).log10());

    // 取中段做频率分析
    let mono: Vec<f32> = a.samples.as_chunks::<2>().0.iter().map(|f| f[0]).collect();
    let n = mono.len();
    if n < 4800 {
        println!("样本太少，跳过频谱分析");
        return Ok(());
    }
    let seg = &mono[n / 4..n / 4 + n / 2];
    let seg_v: Vec<f32> = seg
        .iter()
        .map(|s| *s - seg.iter().sum::<f32>() / seg.len() as f32)
        .collect();

    let zc = zerocross_freq(&seg_v, a.sample_rate);
    let n2 = seg_v.len();
    let win: Vec<f32> = (0..n2)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n2 as f32).cos())
        .collect();
    // 简易 DFT 峰值搜索（只扫 50..4000 Hz）
    let sr = a.sample_rate as f32;
    let mut best = (0f32, 0f32);
    let mut f = 50.0f32;
    while f < 4000.0 {
        let mut re = 0f32;
        let mut im = 0f32;
        let w = 2.0 * std::f32::consts::PI * f / sr;
        for (i, s) in seg_v.iter().enumerate() {
            let a = w * i as f32;
            re += s * win[i] * a.cos();
            im += s * win[i] * a.sin();
        }
        let mag = (re * re + im * im).sqrt();
        if mag > best.1 {
            best = (f, mag);
        }
        f += 1.0;
    }

    println!();
    println!("零交叉频率: {:.1} Hz", zc);
    println!("DFT 峰值  : {:.0} Hz", best.0);
    Ok(())
}
