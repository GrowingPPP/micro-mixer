//! 帧联动前视峰值限幅器。
//!
//! 将音频延迟 `attack_ms`（默认 2 ms），用延迟窗口中的最大峰值提前调低增益。
//! 左右声道共用增益，避免破坏声像；恢复时慢放。输出幅度通过乘法控制，
//! 不把过载波形硬裁成平顶。所有缓冲在构造时分配，处理时无分配、无锁。

use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct Limiter {
    ceiling: f32,
    release_coeff: f32,
    gain: f32,
    channels: usize,
    latency_frames: usize,
    delay: Vec<f32>,
    delay_pos: usize,
    frame_index: u64,
    /// 单调递减峰值队列；最大长度 latency_frames + 1。
    peaks: VecDeque<(u64, f32)>,
    gr_samples: u64,
}

impl Limiter {
    /// 单声道构造。`attack_ms` 是前视延迟，建议 1–5 ms；释放建议 50–200 ms。
    pub fn new(sample_rate: u32, ceiling_db: f32, attack_ms: f32, release_ms: f32) -> Self {
        Self::new_with_channels(sample_rate, 1, ceiling_db, attack_ms, release_ms)
    }

    /// 按帧控制增益，同一帧的所有声道使用相同增益。
    pub fn new_with_channels(
        sample_rate: u32,
        channels: usize,
        ceiling_db: f32,
        attack_ms: f32,
        release_ms: f32,
    ) -> Self {
        let sr = sample_rate.max(1) as f32;
        let channels = channels.max(1);
        let latency_frames = (sr * attack_ms.max(0.01) / 1000.0).ceil() as usize;
        Self {
            ceiling: 10f32.powf(ceiling_db / 20.0),
            release_coeff: (-1.0 / (sr * release_ms.max(1.0) / 1000.0)).exp(),
            gain: 1.0,
            channels,
            latency_frames,
            delay: vec![0.0; latency_frames * channels],
            delay_pos: 0,
            frame_index: 0,
            peaks: VecDeque::with_capacity(latency_frames + 1),
            gr_samples: 0,
        }
    }

    /// 默认单声道参数：ceiling −1 dBFS，前视 2 ms，release 80 ms。
    pub fn with_sample_rate(sample_rate: u32) -> Self {
        Self::new(sample_rate, -1.0, 2.0, 80.0)
    }

    pub fn with_channels(sample_rate: u32, channels: usize) -> Self {
        Self::new_with_channels(sample_rate, channels, -1.0, 2.0, 80.0)
    }

    /// 固定延迟（帧）；启动时同等长度的输出为静音。
    pub fn latency_frames(&self) -> usize {
        self.latency_frames
    }

    /// 就地处理交错样本。调用方必须提供完整帧；不足一帧的尾部输出静音。
    pub fn process_in_place(&mut self, buf: &mut [f32]) {
        let mut frames = buf.chunks_exact_mut(self.channels);
        for frame in frames.by_ref() {
            let peak = frame.iter().fold(0.0f32, |p, s| p.max(s.abs()));
            let earliest = self.frame_index.saturating_sub(self.latency_frames as u64);
            while self
                .peaks
                .front()
                .is_some_and(|&(index, _)| index < earliest)
            {
                self.peaks.pop_front();
            }
            while self.peaks.back().is_some_and(|&(_, p)| p <= peak) {
                self.peaks.pop_back();
            }
            self.peaks.push_back((self.frame_index, peak));
            let window_peak = self.peaks.front().map_or(0.0, |&(_, p)| p);
            let target = if window_peak > self.ceiling {
                // 小余量防止乘法舍入超过 ceiling，无需逐样本硬裁波形。
                self.ceiling * (1.0 - f32::EPSILON) / window_peak
            } else {
                1.0
            };
            self.gain = if target < self.gain {
                target
            } else {
                target + self.release_coeff * (self.gain - target)
            };
            if self.gain < 0.999 {
                self.gr_samples += self.channels as u64;
            }
            let base = self.delay_pos * self.channels;
            for (c, sample) in frame.iter_mut().enumerate() {
                let delayed = self.delay[base + c];
                self.delay[base + c] = *sample;
                *sample = delayed * self.gain;
            }
            self.delay_pos = (self.delay_pos + 1) % self.latency_frames;
            self.frame_index += 1;
        }
        frames.into_remainder().fill(0.0);
    }

    pub fn gain_reduction_samples(&self) -> u64 {
        self.gr_samples
    }

    pub fn reset(&mut self) {
        self.gain = 1.0;
        self.gr_samples = 0;
        self.delay.fill(0.0);
        self.delay_pos = 0;
        self.frame_index = 0;
        self.peaks.clear();
    }

    pub fn current_gain_db(&self) -> f32 {
        20.0 * self.gain.max(1e-6).log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peak(buf: &[f32]) -> f32 {
        buf.iter().fold(0f32, |m, s| m.max(s.abs()))
    }

    #[test]
    fn 不超限的输入不应被改动() {
        let mut lim = Limiter::with_sample_rate(48_000);
        let mut buf = vec![0.5f32; 4800];
        let orig = buf.clone();
        lim.process_in_place(&mut buf);
        assert!(buf[..lim.latency_frames()].iter().all(|&s| s == 0.0));
        for (a, b) in buf[lim.latency_frames()..].iter().zip(orig.iter()) {
            assert!((a - b).abs() < 1e-6, "除固定前视延迟外，小信号不应被压缩");
        }
    }

    #[test]
    fn 过载输入必须被压到上限内() {
        let mut lim = Limiter::with_sample_rate(48_000);
        let mut buf: Vec<f32> = (0..9600)
            .map(|i| (i as f32 * 0.05).sin() * 4.0) // 远超 0dBFS
            .collect();
        lim.process_in_place(&mut buf);
        assert!(peak(&buf) <= 0.9, "峰值 {} 超出 ceiling", peak(&buf));
    }

    #[test]
    fn 直流过载也要被限制() {
        let mut lim = Limiter::with_sample_rate(48_000);
        let mut buf = vec![10.0f32; 4800];
        lim.process_in_place(&mut buf);
        assert!(peak(&buf) <= 0.9, "直流峰值 {} 超出 ceiling", peak(&buf));
    }

    #[test]
    fn 静音输入保持静音() {
        let mut lim = Limiter::with_sample_rate(48_000);
        let mut buf = vec![0.0f32; 4800];
        lim.process_in_place(&mut buf);
        assert_eq!(peak(&buf), 0.0);
    }

    #[test]
    fn 持续过载正弦不得被硬裁成平顶() {
        let mut lim = Limiter::with_sample_rate(48_000);
        let source: Vec<f32> = (0..24_000)
            .map(|i| (std::f32::consts::TAU * 440.0 * i as f32 / 48_000.0).sin() * 4.0)
            .collect();
        let mut out = source.clone();
        lim.process_in_place(&mut out);
        let stable = &out[4800..];
        let flat = stable
            .iter()
            .filter(|s| (s.abs() - lim.ceiling).abs() < 1e-5)
            .count();
        assert!(
            flat as f32 / (stable.len() as f32) < 0.01,
            "硬削波平顶比例必须 <1%，实际 {flat}/{}",
            stable.len()
        );
        assert!(peak(stable) <= lim.ceiling);
        // 峰值应接近 ceiling，不能靠把整个信号压到很小通过测试。
        assert!(peak(stable) > lim.ceiling * 0.98);
        let gain = lim.ceiling / 4.0;
        let latency = lim.latency_frames();
        let rms_error = (out[4800..]
            .iter()
            .zip(&source[4800 - latency..])
            .map(|(a, b)| (a - b * gain).powi(2))
            .sum::<f32>()
            / stable.len() as f32)
            .sqrt();
        assert!(
            rms_error < 0.002,
            "稳定过载应保持正弦形状，RMS 误差 {rms_error}"
        );
    }

    #[test]
    fn 孤立突变应前视限制且左右声道增益一致() {
        let mut lim = Limiter::with_channels(48_000, 2);
        let latency = lim.latency_frames();
        assert_eq!(latency, 96, "48kHz 默认前视 2ms");
        let mut buf = vec![0.0; (latency * 4 + 1) * 2];
        buf[latency * 2] = 12.0;
        buf[latency * 2 + 1] = -3.0;
        lim.process_in_place(&mut buf);
        assert!(buf[..latency * 2].iter().all(|&s| s == 0.0));
        let pulse = &buf[latency * 4..latency * 4 + 2];
        assert!(pulse[0].abs() <= lim.ceiling && pulse[0].abs() > lim.ceiling * 0.99);
        assert!(
            (pulse[1] / pulse[0] + 0.25).abs() < 1e-6,
            "声道共用增益，不得改变声像"
        );
    }

    #[test]
    fn 分块限幅波形应完全一致且重置清空延迟() {
        let source: Vec<f32> = (0..12_000).map(|i| (i as f32 * 0.02).sin() * 4.0).collect();
        let mut whole = source.clone();
        let mut chunked = source;
        let mut a = Limiter::with_channels(44_100, 2);
        let mut b = a.clone();
        a.process_in_place(&mut whole);
        for chunk in chunked.chunks_mut(74) {
            b.process_in_place(chunk);
        }
        assert_eq!(whole, chunked);
        b.reset();
        let mut silence = vec![0.0; 1000];
        b.process_in_place(&mut silence);
        assert!(silence.iter().all(|&s| s == 0.0));
        assert_eq!(b.gain_reduction_samples(), 0);
    }
}
