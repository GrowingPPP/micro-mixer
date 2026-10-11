//! 线性插值重采样器（单声道）+ 时钟漂移控制器。
//!
//! M1 先用线性插值跑通链路；M1-08 会换成 `rubato` 的 sinc 重采样。
//! 但**漂移控制器的接口现在就是最终形态** —— 换重采样器时不用改上层。

/// 单声道线性插值重采样器。**无分配、无锁**，可在实时线程使用。
#[derive(Debug, Clone)]
pub struct LinearResampler {
    /// 输出采样数 / 输入采样数
    ratio: f64,
    /// 下一个输出样本在**当前输入块**中的读位置（可为负，表示要回看上一样本）
    pos: f64,
    /// 上一块的最后一个样本
    prev: f32,
    primed: bool,
}

impl LinearResampler {
    pub fn new(ratio: f64) -> Self {
        Self {
            ratio: ratio.clamp(0.25, 4.0),
            pos: 0.0,
            prev: 0.0,
            primed: false,
        }
    }

    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// 设置重采样比率。内部做范围钳制，防止漂移控制器发散。
    pub fn set_ratio(&mut self, ratio: f64) {
        self.ratio = ratio.clamp(0.25, 4.0);
    }

    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.prev = 0.0;
        self.primed = false;
    }

    /// 把 `input` 重采样写入 `out`，返回实际写入的样本数。
    ///
    /// 要求 `out` 足够大（建议 `ceil(input.len() * ratio) + 8`）。
    /// 未写满的尾部不会丢失 —— 位置状态会带到下一次调用。
    pub fn process(&mut self, input: &[f32], out: &mut [f32]) -> usize {
        let n = input.len();
        if n == 0 || out.is_empty() {
            return 0;
        }
        if !self.primed {
            self.prev = input[0];
            self.primed = true;
        }

        let mut written = 0usize;
        let last = n as isize - 1;
        while written < out.len() {
            let idx = self.pos.floor() as isize;
            if idx >= last {
                break; // 需要下一块的样本了
            }
            let next = idx + 1;
            let frac = (self.pos - idx as f64) as f32;
            let a = if idx < 0 { self.prev } else { input[idx as usize] };
            let b = input[next as usize];
            out[written] = a + (b - a) * frac;
            written += 1;
            self.pos += 1.0 / self.ratio;
        }

        // 位置平移到下一块的坐标系：下一块的第 0 个样本 = 本块的第 n 个
        self.prev = input[n - 1];
        self.pos -= n as f64;
        // 防止长期累积误差把位置推到不可用的范围
        if self.pos < -4.0 {
            self.pos = -1.0;
        }
        written
    }
}

/// 时钟漂移控制器。
///
/// 真实麦克风与虚拟设备是**两个独立时钟域**，即使名义采样率相同，
/// 实际频率也会差 ±10–100 ppm。不做处理时环形缓冲会逐渐被抽干或撑爆，
/// 表现为周期性爆音或延迟持续增大。
///
/// 做法：监控缓冲水位，微调重采样比率把长期漂移吸收掉。
#[derive(Debug, Clone)]
pub struct DriftController {
    base_ratio: f64,
    /// 目标水位（样本数）
    target_fill: f64,
    /// 比例增益：水位偏差 100% 时，比率修正多少
    gain: f64,
    /// 单次修正上限，防止抖动
    max_correction: f64,
}

impl DriftController {
    /// `target_fill` 建议为 10–20 ms 对应的样本数。
    pub fn new(base_ratio: f64, target_fill: usize) -> Self {
        Self {
            base_ratio,
            target_fill: target_fill.max(1) as f64,
            gain: 0.20,
            // ±2%：稳态漂移只需 ~0.01%，余量用于消化启动瞬态。
            // 再大就会有可闻的音高偏移（2% ≈ 34 音分）。
            max_correction: 0.02,
        }
    }

    /// 根据当前水位算出应该使用的重采样比率。
    ///
    /// **符号很关键**：水位偏高 = 产出多于消费 → 必须**降低**比率。
    /// 若写成 `1 + corr`（水位越高比率越大）就构成正反馈，
    /// 缓冲会被一路顶到容量上限并持续溢出。
    pub fn ratio_for(&self, fill: usize) -> f64 {
        let err = (fill as f64 - self.target_fill) / self.target_fill;
        let corr = (-err * self.gain).clamp(-self.max_correction, self.max_correction);
        self.base_ratio * (1.0 + corr)
    }

    pub fn target_fill(&self) -> usize {
        self.target_fill as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 单位比率应当近似恒等() {
        let mut r = LinearResampler::new(1.0);
        let input: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let mut out = vec![0f32; 128];
        let n = r.process(&input, &mut out);
        assert!(n >= 99, "应输出约 100 个样本，实际 {n}");
        // 前几个样本应基本一致（有 1 个样本的相位偏移）
        for i in 1..10 {
            assert!((out[i] - input[i]).abs() < 2.0, "第 {i} 个样本偏差过大");
        }
    }

    #[test]
    fn 两倍上采样样本数应翻倍() {
        let mut r = LinearResampler::new(2.0);
        let input = vec![0.5f32; 100];
        let mut out = vec![0f32; 256];
        let n = r.process(&input, &mut out);
        assert!((190..=210).contains(&n), "2x 上采样应输出约 200，实际 {n}");
    }

    #[test]
    fn 分块处理与整块结果一致() {
        let input: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.01).sin()).collect();

        let mut whole = LinearResampler::new(1.0);
        let mut out_whole = vec![0f32; 1200];
        let n_whole = whole.process(&input, &mut out_whole);

        let mut chunked = LinearResampler::new(1.0);
        let mut out_chunked = Vec::new();
        for chunk in input.chunks(64) {
            let mut buf = vec![0f32; 128];
            let n = chunked.process(chunk, &mut buf);
            out_chunked.extend_from_slice(&buf[..n]);
        }

        assert!(
            (n_whole as i64 - out_chunked.len() as i64).abs() <= 2,
            "分块与整块的输出长度应接近：{n_whole} vs {}",
            out_chunked.len()
        );
    }

    #[test]
    fn 漂移控制器在水位达标时不修正() {
        let c = DriftController::new(1.0, 1000);
        assert!((c.ratio_for(1000) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn 水位偏高时必须降低比率() {
        // 这条是防回归的关键：符号写反会构成正反馈，把缓冲顶爆
        let c = DriftController::new(1.0, 1000);
        let r = c.ratio_for(5000);
        assert!(r < 1.0, "水位远高于目标时比率必须 < 1，实际 {r}");
    }

    #[test]
    fn 水位偏低时必须提高比率() {
        let c = DriftController::new(1.0, 1000);
        let r = c.ratio_for(100);
        assert!(r > 1.0, "水位远低于目标时比率必须 > 1，实际 {r}");
    }

    #[test]
    fn 漂移控制器有界() {
        let c = DriftController::new(1.0, 1000);
        for fill in [0usize, 1, 500, 5000, 100_000] {
            let r = c.ratio_for(fill);
            assert!(
                (0.975..=1.025).contains(&r),
                "水位 {fill} 时比率 {r} 越界（应限制在 ±2% 内）"
            );
        }
    }

    #[test]
    fn 漂移控制器能把缓冲拉回目标() {
        // 模拟闭环：启动瞬态把缓冲灌到目标 3 倍，应在一秒内量级收敛
        let target = 1920usize;
        let c = DriftController::new(1.0, target);
        let mut fill = (target * 3) as f64;
        let mut steps = 0;
        // 每步 = 一个 10ms 回调（480 帧 = 960 样本）
        while steps < 1000 && (fill - target as f64).abs() > target as f64 * 0.1 {
            let r = c.ratio_for(fill as usize);
            fill = (fill + (r - 1.0) * 960.0).max(0.0);
            steps += 1;
        }
        assert!(
            steps < 500,
            "应收敛到目标 ±10% 内，用了 {steps} 步（约 {} 秒），最终 {fill}",
            steps as f32 * 0.01
        );
    }
}
