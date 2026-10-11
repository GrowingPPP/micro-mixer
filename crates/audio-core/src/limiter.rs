//! 峰值限幅器 —— 保证送往麦克风的信号**永不削波**。
//!
//! 放在混音链路的最后一级。无论用户怎么叠加音效、怎么拉增益，
//! 输出都不会超过 ceiling，因此不会产生刺耳的削波失真。
//!
//! **无分配、无锁**，可在实时线程使用。

#[derive(Debug, Clone)]
pub struct Limiter {
    ceiling: f32,
    attack_coeff: f32,
    release_coeff: f32,
    envelope: f32,
    gain: f32,
    /// 统计：被压制的样本数，用于诊断
    gr_samples: u64,
}

impl Limiter {
    /// `ceiling_db` 建议 −1.0 dBFS；`attack_ms` 1–5；`release_ms` 50–200。
    pub fn new(sample_rate: u32, ceiling_db: f32, attack_ms: f32, release_ms: f32) -> Self {
        let sr = sample_rate.max(1) as f32;
        Self {
            ceiling: 10f32.powf(ceiling_db / 20.0),
            attack_coeff: (-1.0 / (sr * attack_ms.max(0.01) / 1000.0)).exp(),
            release_coeff: (-1.0 / (sr * release_ms.max(1.0) / 1000.0)).exp(),
            envelope: 0.0,
            gain: 1.0,
            gr_samples: 0,
        }
    }

    /// 默认参数：ceiling −1 dBFS，attack 2ms，release 80ms。
    pub fn with_sample_rate(sample_rate: u32) -> Self {
        Self::new(sample_rate, -1.0, 2.0, 80.0)
    }

    /// 就地处理交错样本。**实时安全。**
    pub fn process_in_place(&mut self, buf: &mut [f32]) {
        for s in buf.iter_mut() {
            let abs = s.abs();
            // 峰值包络：快攻慢放
            let coeff = if abs > self.envelope {
                self.attack_coeff
            } else {
                self.release_coeff
            };
            self.envelope = abs + coeff * (self.envelope - abs);

            // 目标增益：包络超过 ceiling 时按比例压缩
            let target_gain = if self.envelope > self.ceiling {
                self.ceiling / self.envelope
            } else {
                1.0
            };
            // 增益平滑（下降快、恢复慢），避免抽吸感
            let g_coeff = if target_gain < self.gain {
                self.attack_coeff
            } else {
                self.release_coeff
            };
            self.gain = target_gain + g_coeff * (self.gain - target_gain);

            if self.gain < 0.999 {
                self.gr_samples += 1;
            }
            // 硬性兜底：任何情况下都不允许超过 ceiling
            *s = (*s * self.gain).clamp(-self.ceiling, self.ceiling);
        }
    }

    /// 被压制的样本数（诊断用）。
    pub fn gain_reduction_samples(&self) -> u64 {
        self.gr_samples
    }

    pub fn reset(&mut self) {
        self.envelope = 0.0;
        self.gain = 1.0;
        self.gr_samples = 0;
    }

    /// 当前增益（dB），用于 UI 显示。
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
        for (a, b) in buf.iter().zip(orig.iter()) {
            assert!((a - b).abs() < 0.05, "小信号不应被压缩");
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
}
