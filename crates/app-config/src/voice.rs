//! 变声配置与预设。
//!
//! 配置层不能依赖 `audio-core`：`VoiceParamsConfig` 与音频层的 `VoiceParams`
//! 保持同名字段和相同取值范围，由 Host、CLI 或会话适配层显式转换。
//! 两边的范围必须同步维护，否则保存的预设会在加载时被静默钳制。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Config;

/// 激活预设缺失时的回退目标；回退同时关闭变声，避免用户误以为仍在变声。
const FALLBACK_PRESET: &str = "builtin:female";

/// 音高偏移范围（半音）。
const PITCH_RANGE: (f32, f32) = (-24.0, 24.0);
/// 共振峰偏移范围（半音）。
const FORMANT_RANGE: (f32, f32) = (-12.0, 12.0);
/// 噪声门阈值默认值与范围（dBFS）。
const GATE_DEFAULT_DB: f32 = -45.0;
const GATE_RANGE_DB: (f32, f32) = (-60.0, 0.0);
/// 三段均衡增益范围（dB）。
const EQ_GAIN_RANGE_DB: (f32, f32) = (-12.0, 12.0);
/// 软削波量范围。
const DRIVE_RANGE: (f32, f32) = (0.0, 1.0);
/// 环形调制频率默认值与范围（Hz）。
const ROBOT_HZ_DEFAULT: f32 = 50.0;
const ROBOT_HZ_RANGE: (f32, f32) = (20.0, 1000.0);
/// 环形调制混合默认值与范围。
const ROBOT_MIX_DEFAULT: f32 = 0.8;
const ROBOT_MIX_RANGE: (f32, f32) = (0.0, 1.0);

/// 持久化的变声参数。
///
/// 字段与 `audio-core::VoiceParams` 一一对应；开关与数值分开保存，
/// 「EQ 开启但 0 dB」和「EQ 关闭」因此不会在往返中丢失区别。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoiceParamsConfig {
    pub pitch_semitones: f32,
    pub formant_semitones: f32,
    pub gate_enabled: bool,
    pub gate_db: f32,
    pub eq_low_enabled: bool,
    pub eq_low_db: f32,
    pub eq_mid_enabled: bool,
    pub eq_mid_db: f32,
    pub eq_high_enabled: bool,
    pub eq_high_db: f32,
    pub drive_enabled: bool,
    pub drive: f32,
    pub robot_enabled: bool,
    pub robot_hz: f32,
    pub robot_mix: f32,
}

impl Default for VoiceParamsConfig {
    fn default() -> Self {
        Self::neutral()
    }
}

impl VoiceParamsConfig {
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

    /// 返回每个越界或非有限字段的说明；加载时据此记录警告后再钳制。
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();
        check_range(&mut issues, "音高", self.pitch_semitones, PITCH_RANGE);
        check_range(&mut issues, "共振峰", self.formant_semitones, FORMANT_RANGE);
        check_range(&mut issues, "噪声门阈值", self.gate_db, GATE_RANGE_DB);
        check_range(&mut issues, "低频增益", self.eq_low_db, EQ_GAIN_RANGE_DB);
        check_range(&mut issues, "中频增益", self.eq_mid_db, EQ_GAIN_RANGE_DB);
        check_range(&mut issues, "高频增益", self.eq_high_db, EQ_GAIN_RANGE_DB);
        check_range(&mut issues, "软削波量", self.drive, DRIVE_RANGE);
        check_range(&mut issues, "环形调制频率", self.robot_hz, ROBOT_HZ_RANGE);
        check_range(&mut issues, "环形调制混合", self.robot_mix, ROBOT_MIX_RANGE);
        issues
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

fn check_range(issues: &mut Vec<String>, label: &str, value: f32, (min, max): (f32, f32)) {
    if !value.is_finite() {
        issues.push(format!("{label}不是有限数值"));
    } else if value < min || value > max {
        issues.push(format!("{label} {value} 超出范围 {min}～{max}"));
    }
}

/// 一条用户或内置变声预设。
#[derive(Debug, Clone, PartialEq)]
pub struct VoicePreset {
    pub id: String,
    pub name: String,
    pub params: VoiceParamsConfig,
    /// 该条目内不认识的字段，按条目原样保留，避免保存时抹掉手写内容。
    #[doc(hidden)]
    pub unknown_lines: Vec<String>,
}

/// `[voice]` 节。
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceConfig {
    pub enabled: bool,
    pub active_preset: String,
    pub bypass_hotkey: String,
    pub next_preset_hotkey: String,
    pub prev_preset_hotkey: String,
    /// 本节内不认识的字段，保存时写回。
    #[doc(hidden)]
    pub unknown_lines: Vec<String>,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            active_preset: FALLBACK_PRESET.to_owned(),
            bypass_hotkey: String::new(),
            next_preset_hotkey: String::new(),
            prev_preset_hotkey: String::new(),
            unknown_lines: Vec::new(),
        }
    }
}

impl VoiceConfig {
    /// 返回实际可用的激活预设与是否应保持启用。
    ///
    /// 目标既不是内置预设也不是有效用户预设时回退到女声并要求关闭变声；
    /// 保存的 `enabled=true` 不能把一个失效引用变成意外的变声行为。
    pub fn effective_active_preset(&self, user_presets: &[VoicePreset]) -> (String, bool) {
        let exists = builtin_voice_presets()
            .iter()
            .any(|preset| preset.id == self.active_preset)
            || user_presets
                .iter()
                .any(|preset| preset.id == self.active_preset);
        if exists {
            (self.active_preset.clone(), self.enabled)
        } else {
            (FALLBACK_PRESET.to_owned(), false)
        }
    }
}

/// 内置处理预设。由代码定义，不写入配置文件。
pub fn builtin_voice_presets() -> Vec<VoicePreset> {
    let female = VoiceParamsConfig {
        pitch_semitones: 4.0,
        formant_semitones: 3.0,
        gate_enabled: true,
        gate_db: -45.0,
        eq_high_enabled: true,
        eq_high_db: 2.0,
        ..VoiceParamsConfig::neutral()
    };
    let male = VoiceParamsConfig {
        pitch_semitones: -4.0,
        formant_semitones: -3.0,
        gate_enabled: true,
        gate_db: -45.0,
        eq_low_enabled: true,
        eq_low_db: 2.0,
        ..VoiceParamsConfig::neutral()
    };
    let child = VoiceParamsConfig {
        pitch_semitones: 7.0,
        formant_semitones: 5.0,
        gate_enabled: true,
        gate_db: -45.0,
        eq_high_enabled: true,
        eq_high_db: 3.0,
        ..VoiceParamsConfig::neutral()
    };
    let deep = VoiceParamsConfig {
        pitch_semitones: -6.0,
        formant_semitones: -4.0,
        gate_enabled: true,
        gate_db: -45.0,
        eq_low_enabled: true,
        eq_low_db: 3.0,
        ..VoiceParamsConfig::neutral()
    };
    let robot = VoiceParamsConfig {
        drive_enabled: true,
        drive: 0.2,
        robot_enabled: true,
        robot_hz: 50.0,
        robot_mix: 0.8,
        ..VoiceParamsConfig::neutral()
    };
    let radio = VoiceParamsConfig {
        gate_enabled: true,
        gate_db: -50.0,
        eq_low_enabled: true,
        eq_low_db: -9.0,
        eq_mid_enabled: true,
        eq_mid_db: 4.0,
        eq_high_enabled: true,
        eq_high_db: -9.0,
        drive_enabled: true,
        drive: 0.35,
        ..VoiceParamsConfig::neutral()
    };
    vec![
        preset("builtin:female", "女声", female),
        preset("builtin:male", "男声", male),
        preset("builtin:child", "小孩", child),
        preset("builtin:deep", "低沉", deep),
        preset("builtin:robot", "机器人", robot),
        preset("builtin:radio", "对讲机", radio),
    ]
}

fn preset(id: &str, name: &str, params: VoiceParamsConfig) -> VoicePreset {
    VoicePreset {
        id: id.to_owned(),
        name: name.to_owned(),
        params,
        unknown_lines: Vec::new(),
    }
}

impl Config {
    /// 同时查询内置与用户预设。
    pub fn find_voice_preset(&self, id: &str) -> Option<VoicePreset> {
        builtin_voice_presets()
            .into_iter()
            .find(|preset| preset.id == id)
            .or_else(|| {
                self.voice_presets
                    .iter()
                    .find(|preset| preset.id == id)
                    .cloned()
            })
    }

    /// 生成新的用户预设 ID：`user:<名称派生片段>-<6 位十六进制>`。
    ///
    /// 派生片段只保留 ASCII 字母与数字，避免 ID 在跨平台路径、命令行和
    /// 遥控 URL 中出现转义差异；中文名称等无法派生时使用 `preset`。
    /// 后缀由进程内计数器递增生成，并对照现有预设检查重复。
    pub fn new_user_preset_id(&self, name: &str) -> String {
        let slug = user_preset_slug(name);
        loop {
            let suffix = next_user_suffix();
            let candidate = format!("user:{slug}-{suffix}");
            if self.find_voice_preset(&candidate).is_none() {
                return candidate;
            }
        }
    }
}

fn user_preset_slug(name: &str) -> String {
    let mut slug = String::new();
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let mut slug: String = slug.trim_matches('-').chars().take(24).collect();
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "preset".to_owned()
    } else {
        slug
    }
}

fn next_user_suffix() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    static SALT: AtomicU64 = AtomicU64::new(0);

    let salt = loop {
        let current = SALT.load(Ordering::Relaxed);
        if current != 0 {
            break current;
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos() as u64);
        let candidate = (nanos ^ (u64::from(std::process::id()) << 32)) | 1;
        if SALT
            .compare_exchange(0, candidate, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            break candidate;
        }
    };
    let value = salt.wrapping_add(SEQUENCE.fetch_add(1, Ordering::Relaxed)) & 0x00ff_ffff;
    format!("{value:06x}")
}

pub(crate) fn parse_voice(value: Option<toml::Value>, warnings: &mut Vec<String>) -> VoiceConfig {
    let mut config = VoiceConfig::default();
    let Some(value) = value else {
        return config;
    };
    let toml::Value::Table(mut table) = value else {
        warnings.push("变声配置节格式无效，已忽略".to_owned());
        return config;
    };

    match table.remove("enabled") {
        Some(toml::Value::Boolean(enabled)) => config.enabled = enabled,
        Some(_) => warnings.push("voice.enabled 不是布尔值，已按关闭处理".to_owned()),
        None => {}
    }
    match table.remove("active_preset") {
        Some(toml::Value::String(id)) if !id.trim().is_empty() => {
            config.active_preset = id;
        }
        Some(toml::Value::String(_)) => {
            warnings.push("voice.active_preset 为空，已使用默认预设".to_owned());
        }
        Some(_) => warnings.push("voice.active_preset 不是字符串，已使用默认预设".to_owned()),
        None => {}
    }
    for (key, slot) in [
        ("bypass_hotkey", &mut config.bypass_hotkey),
        ("next_preset_hotkey", &mut config.next_preset_hotkey),
        ("prev_preset_hotkey", &mut config.prev_preset_hotkey),
    ] {
        match table.remove(key) {
            Some(toml::Value::String(value)) => *slot = value,
            Some(_) => warnings.push(format!("voice.{key} 不是字符串，已忽略")),
            None => {}
        }
    }

    config.unknown_lines = toml_lines(table);
    config
}

pub(crate) fn parse_voice_presets(
    value: Option<toml::Value>,
    warnings: &mut Vec<String>,
) -> Vec<VoicePreset> {
    let Some(value) = value else {
        return Vec::new();
    };
    let toml::Value::Array(rows) = value else {
        warnings.push("voice_presets 不是数组，已忽略".to_owned());
        return Vec::new();
    };

    let mut presets: Vec<VoicePreset> = Vec::new();
    for (index, row) in rows.into_iter().enumerate() {
        let Some(mut row) = row.as_table().cloned() else {
            warnings.push(format!("第 {} 条变声预设不是表格，已丢弃", index + 1));
            continue;
        };
        let id = row
            .remove("id")
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        let name = row
            .remove("name")
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();

        if !id.starts_with("user:") {
            warnings.push(format!("变声预设 ID「{id}」缺少 user: 前缀，已丢弃"));
            continue;
        }
        if name.trim().is_empty() {
            warnings.push(format!("用户预设「{id}」名称为空，已丢弃"));
            continue;
        }
        if presets.iter().any(|preset| preset.id == id) {
            warnings.push(format!("用户预设 ID「{id}」重复，已丢弃后出现的条目"));
            continue;
        }

        let mut params = VoiceParamsConfig::neutral();
        if let Some(value) = take_number(&mut row, "pitch_semitones") {
            params.pitch_semitones = value;
        }
        if let Some(value) = take_number(&mut row, "formant_semitones") {
            params.formant_semitones = value;
        }
        if let Some(value) = take_bool(&mut row, "gate_enabled") {
            params.gate_enabled = value;
        }
        if let Some(value) = take_number(&mut row, "gate_db") {
            params.gate_db = value;
        }
        if let Some(value) = take_bool(&mut row, "eq_low_enabled") {
            params.eq_low_enabled = value;
        }
        if let Some(value) = take_number(&mut row, "eq_low_db") {
            params.eq_low_db = value;
        }
        if let Some(value) = take_bool(&mut row, "eq_mid_enabled") {
            params.eq_mid_enabled = value;
        }
        if let Some(value) = take_number(&mut row, "eq_mid_db") {
            params.eq_mid_db = value;
        }
        if let Some(value) = take_bool(&mut row, "eq_high_enabled") {
            params.eq_high_enabled = value;
        }
        if let Some(value) = take_number(&mut row, "eq_high_db") {
            params.eq_high_db = value;
        }
        if let Some(value) = take_bool(&mut row, "drive_enabled") {
            params.drive_enabled = value;
        }
        if let Some(value) = take_number(&mut row, "drive") {
            params.drive = value;
        }
        if let Some(value) = take_bool(&mut row, "robot_enabled") {
            params.robot_enabled = value;
        }
        if let Some(value) = take_number(&mut row, "robot_hz") {
            params.robot_hz = value;
        }
        if let Some(value) = take_number(&mut row, "robot_mix") {
            params.robot_mix = value;
        }

        let issues = params.validate();
        if !issues.is_empty() {
            warnings.push(format!(
                "用户预设「{name}」参数越界，已钳制：{}",
                issues.join("；")
            ));
        }
        presets.push(VoicePreset {
            id,
            name,
            params: params.sanitized(),
            unknown_lines: toml_lines(row),
        });
    }
    presets
}

/// 把 `[voice]` 与用户预设写入文档。内置预设由代码提供，不重复落盘。
pub(crate) fn write_voice(
    document: &mut toml::Table,
    voice: &VoiceConfig,
    presets: &[VoicePreset],
) {
    let mut table = toml::Table::new();
    table.insert("enabled".into(), toml::Value::Boolean(voice.enabled));
    table.insert(
        "active_preset".into(),
        toml::Value::String(voice.active_preset.clone()),
    );
    table.insert(
        "bypass_hotkey".into(),
        toml::Value::String(voice.bypass_hotkey.clone()),
    );
    table.insert(
        "next_preset_hotkey".into(),
        toml::Value::String(voice.next_preset_hotkey.clone()),
    );
    table.insert(
        "prev_preset_hotkey".into(),
        toml::Value::String(voice.prev_preset_hotkey.clone()),
    );
    merge_unknown(&mut table, &voice.unknown_lines);
    document.insert("voice".into(), toml::Value::Table(table));

    let rows: Vec<_> = presets
        .iter()
        .filter(|preset| !preset.id.starts_with("builtin:"))
        .map(preset_to_table)
        .collect();
    if !rows.is_empty() {
        document.insert("voice_presets".into(), toml::Value::Array(rows));
    }
}

fn preset_to_table(preset: &VoicePreset) -> toml::Value {
    let params = preset.params;
    let mut row = toml::Table::new();
    row.insert("id".into(), toml::Value::String(preset.id.clone()));
    row.insert("name".into(), toml::Value::String(preset.name.clone()));
    for (key, value) in [
        ("pitch_semitones", params.pitch_semitones),
        ("formant_semitones", params.formant_semitones),
        ("gate_db", params.gate_db),
        ("eq_low_db", params.eq_low_db),
        ("eq_mid_db", params.eq_mid_db),
        ("eq_high_db", params.eq_high_db),
        ("drive", params.drive),
        ("robot_hz", params.robot_hz),
        ("robot_mix", params.robot_mix),
    ] {
        row.insert(key.into(), toml::Value::Float(f64::from(value)));
    }
    for (key, value) in [
        ("gate_enabled", params.gate_enabled),
        ("eq_low_enabled", params.eq_low_enabled),
        ("eq_mid_enabled", params.eq_mid_enabled),
        ("eq_high_enabled", params.eq_high_enabled),
        ("drive_enabled", params.drive_enabled),
        ("robot_enabled", params.robot_enabled),
    ] {
        row.insert(key.into(), toml::Value::Boolean(value));
    }
    merge_unknown(&mut row, &preset.unknown_lines);
    toml::Value::Table(row)
}

fn take_number(table: &mut toml::Table, key: &str) -> Option<f32> {
    match table.remove(key)? {
        toml::Value::Float(value) => Some(value as f32),
        toml::Value::Integer(value) => Some(value as f32),
        toml::Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

fn take_bool(table: &mut toml::Table, key: &str) -> Option<bool> {
    table.remove(key).and_then(|value| value.as_bool())
}

fn toml_lines(table: toml::Table) -> Vec<String> {
    if table.is_empty() {
        return Vec::new();
    }
    toml::to_string(&table)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn merge_unknown(table: &mut toml::Table, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let Ok(extra) = lines.join("\n").parse::<toml::Table>() else {
        return;
    };
    for (key, value) in extra {
        if !table.contains_key(&key) {
            table.insert(key, value);
        }
    }
}
