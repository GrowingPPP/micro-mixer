//! 变声参数。

/// 音高偏移范围（半音）。
const PITCH_RANGE: (f32, f32) = (-24.0, 24.0);
/// 共振峰偏移范围（半音）。
const FORMANT_RANGE: (f32, f32) = (-12.0, 12.0);
/// 噪声门阈值的默认值与范围（dBFS）。
const GATE_DEFAULT_DB: f32 = -45.0;
const GATE_RANGE_DB: (f32, f32) = (-60.0, 0.0);
/// 三段均衡增益的范围（dB）。
const EQ_GAIN_RANGE_DB: (f32, f32) = (-12.0, 12.0);
/// 软削波量的范围。
const DRIVE_RANGE: (f32, f32) = (0.0, 1.0);
/// 环形调制频率的默认值与范围（Hz）。
const ROBOT_HZ_DEFAULT: f32 = 50.0;
const ROBOT_HZ_RANGE: (f32, f32) = (20.0, 1000.0);
/// 环形调制混合的默认值与范围。
const ROBOT_MIX_DEFAULT: f32 = 0.8;
const ROBOT_MIX_RANGE: (f32, f32) = (0.0, 1.0);

/// 变声参数。
///
/// 音高与共振峰均以半音为单位；默认值 0 表示不改变对应特性。
/// 音高与共振峰之外的数值只在其布尔开关打开时生效。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoiceParams {
    /// 音高偏移（半音）。
    pub pitch_semitones: f32,
    /// 共振峰偏移（半音）。
    pub formant_semitones: f32,
    /// 噪声门开关。
    pub gate_enabled: bool,
    /// 噪声门阈值（dBFS）。
    pub gate_db: f32,
    /// 低频搁架开关。
    pub eq_low_enabled: bool,
    /// 低频搁架增益（dB）。
    pub eq_low_db: f32,
    /// 中频峰值开关。
    pub eq_mid_enabled: bool,
    /// 中频峰值增益（dB）。
    pub eq_mid_db: f32,
    /// 高频搁架开关。
    pub eq_high_enabled: bool,
    /// 高频搁架增益（dB）。
    pub eq_high_db: f32,
    /// 软削波开关。
    pub drive_enabled: bool,
    /// 软削波量；0 表示直通。
    pub drive: f32,
    /// 环形调制开关。
    pub robot_enabled: bool,
    /// 环形调制频率（Hz）。
    pub robot_hz: f32,
    /// 环形调制混合；0 表示直通。
    pub robot_mix: f32,
}

impl Default for VoiceParams {
    fn default() -> Self {
        Self::neutral()
    }
}

impl VoiceParams {
    /// 音高与共振峰为 0、全部效果关闭的中性参数。
    pub fn neutral() -> Self {
        Self {
            pitch_semitones: 0.0,
            formant_semitones: 0.0,
            gate_enabled: false,
            gate_db: GATE_DEFAULT_DB,
            eq_low_enabled: false,
            eq_low_db: 0.0,
            eq_mid_enabled: false,
            eq_mid_db: 0.0,
            eq_high_enabled: false,
            eq_high_db: 0.0,
            drive_enabled: false,
            drive: 0.0,
            robot_enabled: false,
            robot_hz: ROBOT_HZ_DEFAULT,
            robot_mix: ROBOT_MIX_DEFAULT,
        }
    }

    /// 把非有限值替换为对应默认值，并把数值钳制到设计范围。
    pub fn sanitized(self) -> Self {
        Self {
            pitch_semitones: finite_or(self.pitch_semitones, 0.0)
                .clamp(PITCH_RANGE.0, PITCH_RANGE.1),
            formant_semitones: finite_or(self.formant_semitones, 0.0)
                .clamp(FORMANT_RANGE.0, FORMANT_RANGE.1),
            gate_enabled: self.gate_enabled,
            gate_db: finite_or(self.gate_db, GATE_DEFAULT_DB)
                .clamp(GATE_RANGE_DB.0, GATE_RANGE_DB.1),
            eq_low_enabled: self.eq_low_enabled,
            eq_low_db: finite_or(self.eq_low_db, 0.0).clamp(EQ_GAIN_RANGE_DB.0, EQ_GAIN_RANGE_DB.1),
            eq_mid_enabled: self.eq_mid_enabled,
            eq_mid_db: finite_or(self.eq_mid_db, 0.0).clamp(EQ_GAIN_RANGE_DB.0, EQ_GAIN_RANGE_DB.1),
            eq_high_enabled: self.eq_high_enabled,
            eq_high_db: finite_or(self.eq_high_db, 0.0)
                .clamp(EQ_GAIN_RANGE_DB.0, EQ_GAIN_RANGE_DB.1),
            drive_enabled: self.drive_enabled,
            drive: finite_or(self.drive, 0.0).clamp(DRIVE_RANGE.0, DRIVE_RANGE.1),
            robot_enabled: self.robot_enabled,
            robot_hz: finite_or(self.robot_hz, ROBOT_HZ_DEFAULT)
                .clamp(ROBOT_HZ_RANGE.0, ROBOT_HZ_RANGE.1),
            robot_mix: finite_or(self.robot_mix, ROBOT_MIX_DEFAULT)
                .clamp(ROBOT_MIX_RANGE.0, ROBOT_MIX_RANGE.1),
        }
    }

    /// 效果是否全部关闭，或开关打开但数值等价于直通。
    ///
    /// 噪声门没有等价直通的阈值，因此只要打开就视为非中性；EQ 的 0 dB、
    /// 软削波的 0 量、环形调制的 0 混合在实现中逐样本直通。
    pub fn is_neutral_effects(&self) -> bool {
        let params = (*self).sanitized();
        let eq_active = (params.eq_low_enabled && params.eq_low_db != 0.0)
            || (params.eq_mid_enabled && params.eq_mid_db != 0.0)
            || (params.eq_high_enabled && params.eq_high_db != 0.0);
        let drive_active = params.drive_enabled && params.drive != 0.0;
        let robot_active = params.robot_enabled && params.robot_mix != 0.0;

        !(params.gate_enabled || eq_active || drive_active || robot_active)
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 默认变声参数为中性() {
        let params = VoiceParams::default();
        assert_eq!(params.pitch_semitones, 0.0);
        assert_eq!(params.formant_semitones, 0.0);
    }

    #[test]
    fn 默认变声参数为中性且效果全关() {
        let params = VoiceParams::neutral();
        assert_eq!(params.pitch_semitones, 0.0);
        assert_eq!(params.formant_semitones, 0.0);
        assert!(!params.gate_enabled);
        assert_eq!(params.gate_db, -45.0);
        assert!(!params.eq_low_enabled);
        assert_eq!(params.eq_low_db, 0.0);
        assert!(!params.eq_mid_enabled);
        assert_eq!(params.eq_mid_db, 0.0);
        assert!(!params.eq_high_enabled);
        assert_eq!(params.eq_high_db, 0.0);
        assert!(!params.drive_enabled);
        assert_eq!(params.drive, 0.0);
        assert!(!params.robot_enabled);
        assert_eq!(params.robot_hz, 50.0);
        assert_eq!(params.robot_mix, 0.8);
        assert!(params.is_neutral_effects());
        assert_eq!(VoiceParams::default(), params);
    }

    #[test]
    fn 效果参数越界被钳制() {
        let upper = VoiceParams {
            pitch_semitones: 30.0,
            formant_semitones: 20.0,
            gate_enabled: true,
            gate_db: 10.0,
            eq_low_enabled: true,
            eq_low_db: 20.0,
            eq_mid_enabled: true,
            eq_mid_db: 20.0,
            eq_high_enabled: true,
            eq_high_db: 20.0,
            drive_enabled: true,
            drive: 2.0,
            robot_enabled: true,
            robot_hz: 2000.0,
            robot_mix: 1.5,
        }
        .sanitized();
        assert_eq!(upper.pitch_semitones, 24.0);
        assert_eq!(upper.formant_semitones, 12.0);
        assert_eq!(upper.gate_db, 0.0);
        assert_eq!(upper.eq_low_db, 12.0);
        assert_eq!(upper.eq_mid_db, 12.0);
        assert_eq!(upper.eq_high_db, 12.0);
        assert_eq!(upper.drive, 1.0);
        assert_eq!(upper.robot_hz, 1000.0);
        assert_eq!(upper.robot_mix, 1.0);

        let lower = VoiceParams {
            pitch_semitones: -30.0,
            formant_semitones: -20.0,
            gate_enabled: true,
            gate_db: -80.0,
            eq_low_enabled: true,
            eq_low_db: -20.0,
            eq_mid_enabled: true,
            eq_mid_db: -20.0,
            eq_high_enabled: true,
            eq_high_db: -20.0,
            drive_enabled: true,
            drive: -1.0,
            robot_enabled: true,
            robot_hz: 1.0,
            robot_mix: -1.0,
        }
        .sanitized();
        assert_eq!(lower.pitch_semitones, -24.0);
        assert_eq!(lower.formant_semitones, -12.0);
        assert_eq!(lower.gate_db, -60.0);
        assert_eq!(lower.eq_low_db, -12.0);
        assert_eq!(lower.eq_mid_db, -12.0);
        assert_eq!(lower.eq_high_db, -12.0);
        assert_eq!(lower.drive, 0.0);
        assert_eq!(lower.robot_hz, 20.0);
        assert_eq!(lower.robot_mix, 0.0);

        let non_finite = VoiceParams {
            pitch_semitones: f32::NAN,
            formant_semitones: f32::INFINITY,
            gate_db: f32::NEG_INFINITY,
            eq_low_db: f32::NAN,
            eq_mid_db: f32::INFINITY,
            eq_high_db: f32::NEG_INFINITY,
            drive: f32::NAN,
            robot_hz: f32::INFINITY,
            robot_mix: f32::NAN,
            ..VoiceParams::neutral()
        }
        .sanitized();
        assert_eq!(non_finite.pitch_semitones, 0.0);
        assert_eq!(non_finite.formant_semitones, 0.0);
        assert_eq!(non_finite.gate_db, -45.0);
        assert_eq!(non_finite.eq_low_db, 0.0);
        assert_eq!(non_finite.eq_mid_db, 0.0);
        assert_eq!(non_finite.eq_high_db, 0.0);
        assert_eq!(non_finite.drive, 0.0);
        assert_eq!(non_finite.robot_hz, 50.0);
        assert_eq!(non_finite.robot_mix, 0.8);
    }
}
