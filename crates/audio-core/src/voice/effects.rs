//! 人声效果链：噪声门、三段均衡、软削波与环形调制。
//!
//! 效果链在音频回调内运行，处理路径不分配、不加锁、不做 IO。
//! 参数在块边界提交；效果开关用 20 ms 干湿交叉淡化，避免切换咔哒声。

use std::f32::consts::{FRAC_1_SQRT_2, TAU};

use super::VoiceParams;

/// 效果开关的干湿交叉淡化时长。
const CROSSFADE_SECONDS: f32 = 0.020;
/// 噪声门增益打开时间。
const GATE_ATTACK_SECONDS: f32 = 0.005;
/// 噪声门增益关闭时间与包络释放时间。
const GATE_RELEASE_SECONDS: f32 = 0.120;
/// 噪声门在低于关闭阈值后的保持时间。
const GATE_HOLD_SECONDS: f32 = 0.050;
/// 噪声门迟滞：打开阈值与关闭阈值相差的 dB 数。
const GATE_HYSTERESIS_DB: f32 = 3.0;
/// 低频搁架中心频率。
const EQ_LOW_HZ: f32 = 200.0;
/// 中频峰值中心频率与 Q 值。
const EQ_MID_HZ: f32 = 1_000.0;
const EQ_MID_Q: f32 = 0.9;
/// 高频搁架中心频率。
const EQ_HIGH_HZ: f32 = 4_000.0;
/// 软削波的最大预增益；驱动量为 0 时保持逐样本直通。
const DRIVE_SCALE: f32 = 10.0;

/// 效果链。
///
/// 噪声门位于移调之前，其余效果位于移调之后；效果链本身不包含移调后端。
#[derive(Debug)]
pub struct EffectChain {
    gate: NoiseGate,
    eq_low: EqBand,
    eq_mid: EqBand,
    eq_high: EqBand,
    drive: SoftClip,
    robot: RingMod,
}

impl EffectChain {
    /// 按采样率创建效果链；所有系数与状态在这里一次性就绪。
    pub fn new(sample_rate: u32) -> Self {
        assert!(sample_rate > 0, "采样率必须大于 0");
        let sample_rate = sample_rate as f32;
        Self {
            gate: NoiseGate::new(sample_rate),
            eq_low: EqBand::new(sample_rate, EqKind::LowShelf { hz: EQ_LOW_HZ }),
            eq_mid: EqBand::new(
                sample_rate,
                EqKind::Peak {
                    hz: EQ_MID_HZ,
                    q: EQ_MID_Q,
                },
            ),
            eq_high: EqBand::new(sample_rate, EqKind::HighShelf { hz: EQ_HIGH_HZ }),
            drive: SoftClip::new(sample_rate),
            robot: RingMod::new(sample_rate),
        }
    }

    /// 在块边界提交参数；数值越界或非有限值按 [`VoiceParams::sanitized`] 处理。
    pub fn set_params(&mut self, params: VoiceParams) {
        let params = params.sanitized();
        self.gate.set_params(params.gate_enabled, params.gate_db);
        self.eq_low
            .set_params(params.eq_low_enabled, params.eq_low_db);
        self.eq_mid
            .set_params(params.eq_mid_enabled, params.eq_mid_db);
        self.eq_high
            .set_params(params.eq_high_enabled, params.eq_high_db);
        self.drive.set_params(params.drive_enabled, params.drive);
        self.robot
            .set_params(params.robot_enabled, params.robot_hz, params.robot_mix);
    }

    /// 在移调前运行噪声门。
    pub fn process_gate(&mut self, block: &mut [f32]) {
        for sample in block {
            *sample = self.gate.process(*sample);
        }
    }

    /// 在移调后运行均衡、软削波与环形调制。
    pub fn process_post(&mut self, block: &mut [f32]) {
        for sample in block {
            let mut value = *sample;
            value = self.eq_low.process(value);
            value = self.eq_mid.process(value);
            value = self.eq_high.process(value);
            value = self.drive.process(value);
            value = self.robot.process(value);
            *sample = value;
        }
    }

    /// 清空滤波器、包络、保持计时、环形调制相位与交叉淡化进度。
    ///
    /// 参数目标保持不变；重置后的效果链与刚创建并提交同一参数时一致。
    pub fn reset(&mut self) {
        self.gate.reset();
        self.eq_low.reset();
        self.eq_mid.reset();
        self.eq_high.reset();
        self.drive.reset();
        self.robot.reset();
    }
}

/// 线性干湿混合。
#[inline]
fn blend(dry: f32, wet: f32, mix: f32) -> f32 {
    if mix <= 0.0 {
        dry
    } else if mix >= 1.0 {
        wet
    } else {
        dry + (wet - dry) * mix
    }
}

fn db_to_amplitude(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

/// 按样本推进的线性交叉淡化。
#[derive(Debug, Clone, Copy)]
struct Crossfade {
    mix: f32,
    target: f32,
    step: f32,
}

impl Crossfade {
    fn new(sample_rate: f32) -> Self {
        Self {
            mix: 0.0,
            target: 0.0,
            step: 1.0 / (CROSSFADE_SECONDS * sample_rate),
        }
    }

    fn set_target(&mut self, target: f32) {
        self.target = target.clamp(0.0, 1.0);
    }

    fn next(&mut self) -> f32 {
        if self.mix < self.target {
            self.mix = (self.mix + self.step).min(self.target);
        } else if self.mix > self.target {
            self.mix = (self.mix - self.step).max(self.target);
        }
        self.mix
    }

    /// 关闭且已经淡到干声时，效果无需继续计算。
    fn is_idle(&self) -> bool {
        self.mix == 0.0 && self.target == 0.0
    }

    fn reset(&mut self) {
        self.mix = 0.0;
    }
}

/// 带迟滞与保持时间的噪声门。
///
/// 电平检测使用瞬时上升、120 ms 释放的峰值包络；增益本身按 5 ms 打开、
/// 120 ms 关闭，避免门沿被后续频域移调涂抹。
#[derive(Debug)]
struct NoiseGate {
    fade: Crossfade,
    open_threshold: f32,
    close_threshold: f32,
    envelope: f32,
    envelope_release: f32,
    gain: f32,
    attack_step: f32,
    release_step: f32,
    hold_frames: u32,
    hold_left: u32,
    open: bool,
}

impl NoiseGate {
    fn new(sample_rate: f32) -> Self {
        Self {
            fade: Crossfade::new(sample_rate),
            open_threshold: db_to_amplitude(-45.0),
            close_threshold: db_to_amplitude(-45.0 - GATE_HYSTERESIS_DB),
            envelope: 0.0,
            envelope_release: (-1.0 / (GATE_RELEASE_SECONDS * sample_rate)).exp(),
            gain: 0.0,
            attack_step: 1.0 / (GATE_ATTACK_SECONDS * sample_rate),
            release_step: 1.0 / (GATE_RELEASE_SECONDS * sample_rate),
            hold_frames: (GATE_HOLD_SECONDS * sample_rate).round() as u32,
            hold_left: 0,
            open: false,
        }
    }

    fn set_params(&mut self, enabled: bool, gate_db: f32) {
        self.fade.set_target(if enabled { 1.0 } else { 0.0 });
        self.open_threshold = db_to_amplitude(gate_db);
        self.close_threshold = db_to_amplitude(gate_db - GATE_HYSTERESIS_DB);
    }

    fn process(&mut self, sample: f32) -> f32 {
        if self.fade.is_idle() {
            return sample;
        }

        let level = sample.abs();
        self.envelope = if level > self.envelope {
            level
        } else {
            self.envelope * self.envelope_release
        };

        if self.open {
            if self.envelope >= self.close_threshold {
                self.hold_left = self.hold_frames;
            } else if self.hold_left > 0 {
                self.hold_left -= 1;
            } else {
                self.open = false;
            }
        } else if self.envelope >= self.open_threshold {
            self.open = true;
            self.hold_left = self.hold_frames;
        }

        let target = if self.open { 1.0 } else { 0.0 };
        if target > self.gain {
            self.gain = (self.gain + self.attack_step).min(target);
        } else {
            self.gain = (self.gain - self.release_step).max(target);
        }

        let wet = sample * self.gain;
        let mix = self.fade.next();
        blend(sample, wet, mix)
    }

    fn reset(&mut self) {
        self.envelope = 0.0;
        self.gain = 0.0;
        self.hold_left = 0;
        self.open = false;
        self.fade.reset();
    }
}

/// 滤波器类型与固定频率。
#[derive(Debug, Clone, Copy)]
enum EqKind {
    LowShelf { hz: f32 },
    Peak { hz: f32, q: f32 },
    HighShelf { hz: f32 },
}

impl EqKind {
    fn hz(self) -> f32 {
        match self {
            Self::LowShelf { hz } | Self::Peak { hz, .. } | Self::HighShelf { hz } => hz,
        }
    }
}

/// 归一化后的 biquad 系数。
#[derive(Debug, Clone, Copy)]
struct BiquadCoeffs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl BiquadCoeffs {
    const IDENTITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };

    /// RBJ 搁架与峰值系数，按 a0 归一化。
    ///
    /// 0 dB 时直接使用恒等系数，保证开启与关闭的输出可逐样本比较。
    fn design(kind: EqKind, sample_rate: f32, gain_db: f32) -> Self {
        if gain_db == 0.0 {
            return Self::IDENTITY;
        }

        let amp = 10.0f32.powf(gain_db / 40.0);
        let w0 = TAU * kind.hz() / sample_rate;
        let cos = w0.cos();
        let sin = w0.sin();

        let (b0, b1, b2, a0, a1, a2) = match kind {
            EqKind::LowShelf { .. } => {
                let alpha = sin * FRAC_1_SQRT_2;
                let root = amp.sqrt();
                (
                    amp * ((amp + 1.0) - (amp - 1.0) * cos + 2.0 * root * alpha),
                    2.0 * amp * ((amp - 1.0) - (amp + 1.0) * cos),
                    amp * ((amp + 1.0) - (amp - 1.0) * cos - 2.0 * root * alpha),
                    (amp + 1.0) + (amp - 1.0) * cos + 2.0 * root * alpha,
                    -2.0 * ((amp - 1.0) + (amp + 1.0) * cos),
                    (amp + 1.0) + (amp - 1.0) * cos - 2.0 * root * alpha,
                )
            }
            EqKind::Peak { q, .. } => {
                let alpha = sin / (2.0 * q);
                (
                    1.0 + alpha * amp,
                    -2.0 * cos,
                    1.0 - alpha * amp,
                    1.0 + alpha / amp,
                    -2.0 * cos,
                    1.0 - alpha / amp,
                )
            }
            EqKind::HighShelf { .. } => {
                let alpha = sin * FRAC_1_SQRT_2;
                let root = amp.sqrt();
                (
                    amp * ((amp + 1.0) + (amp - 1.0) * cos + 2.0 * root * alpha),
                    -2.0 * amp * ((amp - 1.0) + (amp + 1.0) * cos),
                    amp * ((amp + 1.0) + (amp - 1.0) * cos - 2.0 * root * alpha),
                    (amp + 1.0) - (amp - 1.0) * cos + 2.0 * root * alpha,
                    2.0 * ((amp - 1.0) - (amp + 1.0) * cos),
                    (amp + 1.0) - (amp - 1.0) * cos - 2.0 * root * alpha,
                )
            }
        };

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }
}

/// 直接 I 型 biquad 状态。
#[derive(Debug, Default, Clone, Copy)]
struct BiquadState {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl BiquadState {
    fn process(&mut self, coeffs: &BiquadCoeffs, sample: f32) -> f32 {
        let output = coeffs.b0 * sample + coeffs.b1 * self.x1 + coeffs.b2 * self.x2
            - coeffs.a1 * self.y1
            - coeffs.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = sample;
        self.y2 = self.y1;
        self.y1 = output;
        output
    }

    /// 0 dB 时让状态按恒等关系推进，避免之后调整增益出现残留瞬态。
    fn push_identity(&mut self, sample: f32) -> f32 {
        self.x2 = self.x1;
        self.x1 = sample;
        self.y2 = self.y1;
        self.y1 = sample;
        sample
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// 单段均衡。
#[derive(Debug)]
struct EqBand {
    fade: Crossfade,
    kind: EqKind,
    sample_rate: f32,
    gain_db: f32,
    coeffs: BiquadCoeffs,
    state: BiquadState,
}

impl EqBand {
    fn new(sample_rate: f32, kind: EqKind) -> Self {
        Self {
            fade: Crossfade::new(sample_rate),
            kind,
            sample_rate,
            gain_db: 0.0,
            coeffs: BiquadCoeffs::IDENTITY,
            state: BiquadState::default(),
        }
    }

    fn set_params(&mut self, enabled: bool, gain_db: f32) {
        self.fade.set_target(if enabled { 1.0 } else { 0.0 });
        if gain_db != self.gain_db {
            self.gain_db = gain_db;
            self.coeffs = BiquadCoeffs::design(self.kind, self.sample_rate, gain_db);
        }
    }

    fn process(&mut self, sample: f32) -> f32 {
        if self.fade.is_idle() {
            return sample;
        }

        let wet = if self.gain_db == 0.0 {
            self.state.push_identity(sample)
        } else {
            self.state.process(&self.coeffs, sample)
        };
        let mix = self.fade.next();
        blend(sample, wet, mix)
    }

    fn reset(&mut self) {
        self.state.reset();
        self.fade.reset();
    }
}

/// 归一化 tanh 软削波。
#[derive(Debug)]
struct SoftClip {
    fade: Crossfade,
    amount: f32,
    normalization: f32,
}

impl SoftClip {
    fn new(sample_rate: f32) -> Self {
        Self {
            fade: Crossfade::new(sample_rate),
            amount: 0.0,
            normalization: 0.0,
        }
    }

    fn set_params(&mut self, enabled: bool, drive: f32) {
        self.fade.set_target(if enabled { 1.0 } else { 0.0 });
        self.amount = drive * DRIVE_SCALE;
        self.normalization = self.amount.tanh();
    }

    fn process(&mut self, sample: f32) -> f32 {
        if self.fade.is_idle() {
            return sample;
        }

        let wet = if self.amount == 0.0 {
            sample
        } else {
            ((self.amount * sample).tanh() / self.normalization).clamp(-1.0, 1.0)
        };
        let mix = self.fade.next();
        blend(sample, wet, mix)
    }

    fn reset(&mut self) {
        self.fade.reset();
    }
}

/// 环形调制。
#[derive(Debug)]
struct RingMod {
    fade: Crossfade,
    sample_rate: f32,
    hz: f32,
    phase: f32,
}

impl RingMod {
    fn new(sample_rate: f32) -> Self {
        Self {
            fade: Crossfade::new(sample_rate),
            sample_rate,
            hz: 50.0,
            phase: 0.0,
        }
    }

    fn set_params(&mut self, enabled: bool, hz: f32, mix: f32) {
        self.hz = hz;
        self.fade.set_target(if enabled { mix } else { 0.0 });
    }

    fn process(&mut self, sample: f32) -> f32 {
        if self.fade.is_idle() {
            return sample;
        }

        let carrier = (TAU * self.phase).sin();
        self.phase = (self.phase + self.hz / self.sample_rate).fract();
        let mix = self.fade.next();
        blend(sample, sample * carrier, mix)
    }

    fn reset(&mut self) {
        self.phase = 0.0;
        self.fade.reset();
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use super::*;
    use crate::test_support::count_allocations;

    const SAMPLE_RATE: u32 = 48_000;

    fn 正弦(frames: usize, hz: f32, amplitude: f32) -> Vec<f32> {
        (0..frames)
            .map(|frame| (TAU * hz * frame as f32 / SAMPLE_RATE as f32).sin() * amplitude)
            .collect()
    }

    fn 单频幅度(samples: &[f32], hz: f32) -> f32 {
        let mut real = 0.0;
        let mut imaginary = 0.0;
        for (index, sample) in samples.iter().enumerate() {
            let angle = TAU * hz * index as f32 / SAMPLE_RATE as f32;
            real += sample * angle.cos();
            imaginary -= sample * angle.sin();
        }
        2.0 * (real * real + imaginary * imaginary).sqrt() / samples.len() as f32
    }

    #[test]
    fn 噪声门阈值以下衰减且以上保持() {
        let input = 正弦(SAMPLE_RATE as usize, 440.0, 0.5);
        let mut above = EffectChain::new(SAMPLE_RATE);
        above.set_params(VoiceParams {
            gate_enabled: true,
            gate_db: -40.0,
            ..VoiceParams::neutral()
        });
        let mut above_output = input.clone();
        above.process_gate(&mut above_output);
        let steady = &above_output[SAMPLE_RATE as usize / 4..];
        let steady_input = &input[SAMPLE_RATE as usize / 4..];
        for (dry, wet) in steady_input.iter().zip(steady) {
            assert!((dry - wet).abs() < 1e-6, "阈值以上应基本保持原样");
        }

        let below_input = 正弦(SAMPLE_RATE as usize, 440.0, 0.01);
        let mut below = EffectChain::new(SAMPLE_RATE);
        below.set_params(VoiceParams {
            gate_enabled: true,
            gate_db: -20.0,
            ..VoiceParams::neutral()
        });
        let mut below_output = below_input;
        below.process_gate(&mut below_output);
        let tail = &below_output[below_output.len() / 2..];
        let peak = tail
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        assert!(peak < 0.001, "阈值以下应衰减到 -60 dBFS 以下");
    }

    #[test]
    fn 三段均衡零分贝开启与关闭状态不同() {
        let disabled = VoiceParams::neutral();
        let enabled = VoiceParams {
            eq_low_enabled: true,
            eq_low_db: 0.0,
            eq_mid_enabled: true,
            eq_mid_db: 0.0,
            eq_high_enabled: true,
            eq_high_db: 0.0,
            ..VoiceParams::neutral()
        };
        assert_ne!(disabled, enabled, "开关状态必须可区分");

        let input = 正弦(SAMPLE_RATE as usize / 2, 440.0, 0.5);
        let mut off_chain = EffectChain::new(SAMPLE_RATE);
        off_chain.set_params(disabled);
        let mut off_output = input.clone();
        off_chain.process_post(&mut off_output);

        let mut on_chain = EffectChain::new(SAMPLE_RATE);
        on_chain.set_params(enabled);
        let mut on_output = input;
        on_chain.process_post(&mut on_output);

        assert_eq!(off_output, on_output, "0 dB 开启应逐样本直通");
    }

    #[test]
    fn 软削波限制峰值且关闭时逐样本直通() {
        let input: Vec<f32> = (0..SAMPLE_RATE as usize)
            .map(|index| if index % 2 == 0 { 4.0 } else { -4.0 })
            .collect();

        let mut enabled = EffectChain::new(SAMPLE_RATE);
        enabled.set_params(VoiceParams {
            drive_enabled: true,
            drive: 1.0,
            ..VoiceParams::neutral()
        });
        let mut saturated = input.clone();
        enabled.process_post(&mut saturated);
        let steady = &saturated[SAMPLE_RATE as usize / 10..];
        assert!(
            steady.iter().all(|sample| sample.abs() <= 1.0),
            "软削波稳态输出不得超过满幅"
        );

        let mut disabled = EffectChain::new(SAMPLE_RATE);
        disabled.set_params(VoiceParams::neutral());
        let mut passthrough = input.clone();
        disabled.process_post(&mut passthrough);
        assert_eq!(passthrough, input, "关闭时不得改变样本");
    }

    #[test]
    fn 环形调制混合为零时逐样本直通() {
        let input = 正弦(SAMPLE_RATE as usize, 440.0, 0.5);

        let mut zero = EffectChain::new(SAMPLE_RATE);
        zero.set_params(VoiceParams {
            robot_enabled: true,
            robot_hz: 50.0,
            robot_mix: 0.0,
            ..VoiceParams::neutral()
        });
        let mut dry = input.clone();
        zero.process_post(&mut dry);
        assert_eq!(dry, input, "混合为零时不得改变样本");

        let frames = SAMPLE_RATE as usize + SAMPLE_RATE as usize / 5;
        let mut full = EffectChain::new(SAMPLE_RATE);
        full.set_params(VoiceParams {
            robot_enabled: true,
            robot_hz: 50.0,
            robot_mix: 1.0,
            ..VoiceParams::neutral()
        });
        let mut wet = 正弦(frames, 440.0, 0.5);
        full.process_post(&mut wet);
        let analysis = &wet[SAMPLE_RATE as usize / 5..];

        let lower = 单频幅度(analysis, 390.0);
        let upper = 单频幅度(analysis, 490.0);
        let carrier = 单频幅度(analysis, 440.0);
        assert!(lower > 0.1 && upper > 0.1, "应出现可测的上下边带");
        assert!(carrier < 0.05, "载波应被环形调制抑制");
    }

    #[test]
    fn 效果处理零分配() {
        let mut chain = EffectChain::new(SAMPLE_RATE);
        let params = VoiceParams {
            gate_enabled: true,
            gate_db: -40.0,
            eq_low_enabled: true,
            eq_low_db: 3.0,
            eq_mid_enabled: true,
            eq_mid_db: -2.0,
            eq_high_enabled: true,
            eq_high_db: 4.0,
            drive_enabled: true,
            drive: 0.7,
            robot_enabled: true,
            robot_hz: 120.0,
            robot_mix: 0.6,
            ..VoiceParams::neutral()
        };
        let mut block = vec![0.25; 1024];

        let allocations = count_allocations(|| {
            chain.set_params(params);
            chain.process_gate(&mut block);
            chain.process_post(&mut block);
        });
        assert_eq!(allocations, 0, "效果处理不得分配");
    }

    #[test]
    fn 效果链重置后相位与门限状态归零() {
        let params = VoiceParams {
            gate_enabled: true,
            gate_db: -25.0,
            eq_low_enabled: true,
            eq_low_db: 6.0,
            eq_mid_enabled: true,
            eq_mid_db: -4.0,
            eq_high_enabled: true,
            eq_high_db: 5.0,
            drive_enabled: true,
            drive: 0.5,
            robot_enabled: true,
            robot_hz: 70.0,
            robot_mix: 0.9,
            ..VoiceParams::neutral()
        };
        let mut input = 正弦(SAMPLE_RATE as usize, 440.0, 0.4);
        for sample in input.iter_mut().skip(SAMPLE_RATE as usize / 4) {
            *sample = 0.0;
        }

        let mut chain = EffectChain::new(SAMPLE_RATE);
        chain.set_params(params);
        let mut first = input.clone();
        chain.process_gate(&mut first);
        chain.process_post(&mut first);

        chain.reset();
        let mut second = input.clone();
        chain.process_gate(&mut second);
        chain.process_post(&mut second);

        assert_eq!(first, second, "重置后相同输入必须得到相同输出");
    }
}
