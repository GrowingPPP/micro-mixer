//! 隔离测试：绕过整个引擎，直接用 cpal 往设备写一个纯正弦。
//!
//! 用途：判断"播放音高翻倍"的问题出在 cpal 层还是引擎层。
//!
//! ```bash
//! cargo run --release -p backend-cpal --example rawtone -- "豆包会议记录虚拟设备" 440 3 48000
//! # 再加一个输入流，验证"同时开输入输出"是否是元凶：
//! cargo run --release -p backend-cpal --example rawtone -- "豆包会议记录虚拟设备" 440 3 48000 mic
//! ```

use std::f32::consts::PI;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use audio_core::device::{AudioBackend, DeviceId, StreamFormat};
use backend_cpal::CpalBackend;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let device = args.get(1).cloned().unwrap_or_else(|| "default".into());
    let freq: f32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(440.0);
    let secs: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(3);
    let rate: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(48_000);

    let backend = CpalBackend::new();
    let id = if device == "default" {
        backend
            .guess_virtual_output()
            .ok_or_else(|| anyhow::anyhow!("没找到虚拟声卡"))?
    } else {
        DeviceId::new(&device)
    };

    let fmt = backend.output_format(&id, Some(StreamFormat::new(rate, 2)))?;
    println!("设备: {id}");
    println!("协商格式: {fmt}");

    // 相位累加器 —— 严格按协商到的采样率推进
    let phase = Arc::new(AtomicU64::new(0));
    let phase_cb = phase;
    let sr = fmt.sample_rate as f32;
    let step = (freq / sr * (u32::MAX as f32)) as u64; // 每帧的相位增量

    let stream = backend.open_output(
        &id,
        Some(fmt),
        Box::new(move |out: &mut [f32]| {
            let mut p = phase_cb.load(Ordering::Relaxed);
            for frame in out.as_chunks_mut::<2>().0 {
                let s = ((p as f64 / u32::MAX as f64) as f32 * 2.0 * PI).sin() * 0.5;
                frame[0] = s;
                frame[1] = s;
                p = p.wrapping_add(step);
            }
            phase_cb.store(p, Ordering::Relaxed);
        }),
    )?;

    println!("实际流格式: {}", stream.format());

    // 可选的输入流 —— 用来验证"同时开输入和输出"是否会让输出变调
    let _in_stream = if args.iter().any(|a| a == "mic") {
        let mic = backend.default_input().expect("没有默认输入设备");
        let mfmt = backend.input_format(&mic, Some(StreamFormat::new(48_000, 1)))?;
        println!("同时打开输入流: {mic}  {mfmt}");
        Some(backend.open_input(
            &mic,
            Some(mfmt),
            Box::new(|_d: &[f32]| {}), // 什么都不做，只是持有流
        )?)
    } else {
        println!("（只开输出流）");
        None
    };

    println!("播放 {freq} Hz 纯正弦 {secs} 秒 …");
    std::thread::sleep(std::time::Duration::from_secs(secs));
    drop(stream);
    println!("结束");
    Ok(())
}
