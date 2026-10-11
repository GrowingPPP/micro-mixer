//! 隔离测试：反复打开/关闭同一个输出流，观察内存。
//! 用来判断"启停泄漏"到底出在 cpal 层还是我们自己的代码。
use audio_core::device::{AudioBackend, StreamFormat};
use backend_cpal::CpalBackend;
use std::time::Duration;

fn rss() -> u64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

fn main() {
    let b = CpalBackend::new();
    let dev = b.guess_virtual_output().expect("需要虚拟声卡");
    let fmt = b
        .output_format(&dev, Some(StreamFormat::new(48_000, 2)))
        .unwrap();
    println!("设备 {dev}  格式 {fmt}");
    println!("初始内存 {} KB", rss());

    println!("\n[A] 闭包不捕获任何数据");
    for i in 1..=6 {
        let s = b
            .open_output(
                &dev,
                Some(fmt),
                Box::new(|d: &mut [f32]| {
                    for x in d.iter_mut() {
                        *x = 0.0;
                    }
                }),
            )
            .expect("开流失败");
        std::thread::sleep(Duration::from_millis(120));
        drop(s);
        println!("  第 {i:>2} 次后 {} KB", rss());
    }

    println!("\n[B] 闭包捕获 2.5MB 样本（模拟真实用法：引擎把解码样本放进回调）");
    for i in 1..=6 {
        // 2.5MB ≈ 6.5 秒 48kHz 立体声 f32
        let samples: Vec<f32> = vec![0.1; 6 * 48_000 * 2];
        println!("     分配了 {} KB", samples.len() * 4 / 1024);
        let mut pos = 0usize;
        let s = b
            .open_output(
                &dev,
                Some(fmt),
                Box::new(move |d: &mut [f32]| {
                    for x in d.iter_mut() {
                        if pos < samples.len() {
                            *x = samples[pos];
                            pos += 1;
                        } else {
                            *x = 0.0;
                        }
                    }
                }),
            )
            .expect("开流失败");
        std::thread::sleep(Duration::from_millis(120));
        drop(s);
        println!("  第 {i:>2} 次后 {} KB", rss());
    }
    println!("\n结论：若 B 段线性增长而 A 段趋平 → cpal 没有释放回调捕获的数据");
}
