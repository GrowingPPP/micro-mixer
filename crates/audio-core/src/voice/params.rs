//! 变声参数。

/// 变声参数。
///
/// 音高与共振峰均以半音为单位；默认值 0 表示不改变对应特性。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VoiceParams {
    /// 音高偏移（半音）。
    pub pitch_semitones: f32,
    /// 共振峰偏移（半音）。
    pub formant_semitones: f32,
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
}
