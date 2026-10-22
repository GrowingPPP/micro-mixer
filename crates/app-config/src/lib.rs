//! 应用配置 —— `~/.config/mixer/config.toml`
//!
//! 目的：让日常使用只需要一条命令。设备名、音效目录、增益这些不该每次重敲。
//!
//! 使用 TOML 解析器处理字符串转义、空数组和多行配置；旧版扁平配置仍可读取。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

mod voice;
use voice::VoiceLoadNotes;
pub use voice::{builtin_voice_presets, VoiceConfig, VoiceParamsConfig, VoicePreset};

/// 音效库里的一个条目。
///
/// 配置里以**并行数组**的形式存放（`sound_files` / `sound_gains` /
/// `sound_names` / `sound_tags`），这里把它们组合成便于使用的视图。
#[derive(Debug, Clone, PartialEq)]
pub struct SoundItem {
    pub path: PathBuf,
    /// 显示名。没有自定义名时取文件名（去扩展名）。
    pub name: String,
    pub gain_db: f32,
    pub tags: Vec<String>,
}

/// 持久化音效记录；稳定标识使过滤无效文件和排序不会改变条目身份。
#[derive(Debug, Clone, PartialEq)]
pub struct SoundEntry {
    pub id: String,
    pub path: PathBuf,
    pub name: String,
    pub gain_db: f32,
    pub tags: Vec<String>,
    pub hotkey: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// 真实麦克风（省略则用系统默认）
    pub input_device: Option<String>,
    /// 虚拟声卡输出设备（省略则自动探测）
    pub output_device: Option<String>,
    pub buffer_ms: Option<u32>,
    pub mic_gain_db: Option<f32>,
    pub sfx_gain_db: Option<f32>,
    /// 音效目录列表（批量导入用）
    pub sound_dirs: Vec<PathBuf>,
    /// 显式音效文件列表 —— **非空时优先于 sound_dirs**
    pub sound_files: Vec<PathBuf>,
    /// 结构化配置中的持久标识。旧配置通过路径生成确定性标识。
    pub sound_ids: Vec<String>,
    /// 每个音效的音量（dB），与 sound_files 一一对应
    pub sound_gains: Vec<f32>,
    /// 每个音效的**显示名**（与 sound_files 一一对应；空串表示用文件名）
    pub sound_names: Vec<String>,
    /// 每个音效的**标签**（与 sound_files 一一对应；一条音的多个标签用逗号分隔）
    pub sound_tags: Vec<String>,
    /// 监听：触发的音效同时放给自己听（默认开）
    pub monitor: bool,
    /// 循环当前音效；旧配置没有此项时默认关闭。
    pub looping: bool,
    /// 监听输出到哪个设备。空 = 系统默认输出。
    /// 用途：默认输出可能是虚拟声卡或别的东西，用户希望监听走耳机。
    pub monitor_device: Option<String>,
    /// 热键列表（与音效槽位一一对应）
    pub hotkeys: Vec<String>,

    /// `[voice]` 节；旧配置没有该节时使用默认值（关闭变声）。
    pub voice: VoiceConfig,
    /// 用户变声预设；内置预设由代码提供，不存放在这里。
    pub voice_presets: Vec<VoicePreset>,
    /// 加载变声配置时产生的非阻断警告（越界钳制、条目丢弃、预设回退）。
    pub voice_warnings: Vec<String>,
    /// `voice_warnings` 中会在重写配置时被永久删除的内容（被拒绝的预设条目、
    /// 类型错误的字段、被整体忽略的配置节）。
    ///
    /// 非空时 [`Config::load_for_write_from`] 拒绝写入：保存一次就会把这些手写内容带走。
    pub voice_rejected: Vec<String>,

    /// 解析时不认识的行，原样留着。
    ///
    /// 为什么要留：这是个**用户可以直接编辑**的文本文件。早先保存时只写自己认识的键，
    /// 结果是用户手加的配置（或未来版本才有的键）在下次界面保存时**被静默抹掉** ——
    /// 表现是"我的设置莫名其妙没了"，极难查。
    ///
    /// 留着它们，保存就是**无损往返**的。注释不保留（每次重新生成）。
    #[doc(hidden)]
    pub unknown_lines: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            buffer_ms: None,
            mic_gain_db: None,
            sfx_gain_db: None,
            sound_dirs: Vec::new(),
            sound_files: Vec::new(),
            sound_ids: Vec::new(),
            sound_gains: Vec::new(),
            sound_names: Vec::new(),
            sound_tags: Vec::new(),
            // 默认**开启**监听：没有它，用户在游戏里按键后自己听不到任何声音，
            // 只能盲猜对方有没有收到 —— 这是实打实的可用性问题。
            monitor: true,
            looping: false,
            monitor_device: None,
            hotkeys: Vec::new(),
            voice: VoiceConfig::default(),
            voice_presets: Vec::new(),
            voice_warnings: Vec::new(),
            voice_rejected: Vec::new(),
            unknown_lines: Vec::new(),
        }
    }
}

/// 一次宽松读取的结果：配置本身，加上需要告知用户的问题。
#[derive(Debug, Clone)]
pub struct LoadReport {
    pub config: Config,
    /// 面向用户的中文说明；调用方应写入日志并在界面或终端显示。
    pub notices: Vec<String>,
    /// 为 true 时 [`Config::load_for_write_from`] 会拒绝同一文件：保存将失败，
    /// 直到用户修复文件。
    pub write_blocked: bool,
}

/// 目标平台。
///
/// ## 为什么做成运行时值，而不是到处写 `#[cfg]`
///
/// 平台相关的**路径逻辑**在 macOS 上完全不可见 —— 交叉编译只能证明它
/// **能编译**，证明不了它**算得对**。而配置路径算错的表现是
/// "设置莫名其妙丢了"，属于最难查的一类。
///
/// 把"平台判断"和"读环境变量"都变成参数之后，Windows 的分支就能在 macOS 上
/// 跑单元测试了（见文件末尾那组测试）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Unix,
}

impl Platform {
    /// 当前编译目标。
    pub const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// 环境变量读取器。生产代码传 `std::env::var_os`，测试传假的。
pub type EnvFn<'a> = &'a dyn Fn(&str) -> Option<std::ffi::OsString>;

fn non_empty(v: Option<std::ffi::OsString>) -> Option<PathBuf> {
    v.filter(|s| !s.is_empty()).map(PathBuf::from)
}

/// 用户主目录。**Windows 上没有 `HOME`**，要用 `USERPROFILE`。
pub fn home_dir() -> Option<PathBuf> {
    home_dir_on(Platform::current(), &|k| std::env::var_os(k))
}

fn home_dir_on(p: Platform, env: EnvFn) -> Option<PathBuf> {
    match p {
        Platform::Windows => non_empty(env("USERPROFILE")),
        Platform::Unix => non_empty(env("HOME")),
    }
}

/// 配置的根目录。按平台选约定位置。
///
/// - **Windows**：`%APPDATA%`（Roaming，设置跟着用户走）
/// - **macOS / Linux**：`XDG_CONFIG_HOME`，回退 `~/.config`
///
/// ⚠️ Windows 上**没有** `HOME` 也没有 `XDG_CONFIG_HOME` ——
/// 早先的实现在那儿会一路回退到 `"."`，也就是**当前工作目录**。
/// GUI 应用的 CWD 是不可预测的（可能是 `C:\Windows\System32`），
/// 结果是用户每次改完设置、下次打开就没了。这是个必须在移植时修掉的坑。
pub fn base_dir() -> PathBuf {
    base_dir_on(
        Platform::current(),
        &|k| std::env::var_os(k),
        &std::env::temp_dir(),
    )
}

fn base_dir_on(p: Platform, env: EnvFn, tmp: &std::path::Path) -> PathBuf {
    match p {
        Platform::Windows => {
            if let Some(v) = non_empty(env("APPDATA")) {
                return v;
            }
            // 极少数情况下 APPDATA 没设（服务账户等）——退回用户目录下的标准位置
            if let Some(v) = non_empty(env("USERPROFILE")) {
                return v.join("AppData").join("Roaming");
            }
        }
        Platform::Unix => {
            if let Some(v) = non_empty(env("XDG_CONFIG_HOME")) {
                return v;
            }
            if let Some(v) = non_empty(env("HOME")) {
                return v.join(".config");
            }
        }
    }

    // 都拿不到 —— 用一个**固定**的临时位置，而不是 "."。
    // "." 会随启动方式漂移，用户会看到"设置时好时坏"这种最难查的现象。
    tmp.join("mixer-config")
}

impl Config {
    /// 配置文件路径。
    ///
    /// - **macOS / Linux**：遵循 `XDG_CONFIG_HOME`，回退到 `~/.config`
    /// - **Windows**：`%APPDATA%`（Roaming，设置跟着用户走）
    ///
    /// ⚠️ Windows 上**没有** `HOME` 也没有 `XDG_CONFIG_HOME` ——
    /// 早先的实现在那儿会一路回退到 `"."`，也就是**当前工作目录**。
    /// GUI 应用的 CWD 是不可预测的（可能是 `C:\Windows\System32`），
    /// 结果是用户每次改完设置、下次打开就没了。这是个必须在移植时修掉的坑。
    pub fn path() -> PathBuf {
        base_dir().join("mixer").join("config.toml")
    }

    /// 宽松读取配置，只用于展示与启动参数。文件不存在或无法读取时返回默认值。
    ///
    /// 读出的配置**不能**直接改完再写回：无效 TOML 与读取失败会退化成部分内容或默认值，
    /// 保存后原文件里的音效库与用户预设随之丢失。会写回的流程使用
    /// [`Config::load_for_write`]。需要告知用户读取问题时使用 [`Config::load_report`]。
    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(_) => Self::default(),
        }
    }

    /// 读取配置并返回需要告知用户的问题。读取本身与 [`Config::load`] 一样宽松。
    pub fn load_report() -> LoadReport {
        Self::load_report_from(&Self::path())
    }

    pub fn load_report_from(path: &Path) -> LoadReport {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return LoadReport {
                    config: Self::default(),
                    notices: Vec::new(),
                    write_blocked: false,
                };
            }
            Err(error) => {
                return LoadReport {
                    config: Self::default(),
                    notices: vec![format!(
                        "读取配置文件失败（{}）：{error}。本次按默认设置运行；\
                         修复前保存配置会被拒绝，原文件不会被覆盖",
                        path.display()
                    )],
                    write_blocked: true,
                };
            }
        };
        match text.parse::<toml::Table>() {
            Ok(table) => {
                let config = Self::from_table(table);
                let mut notices = config.voice_warnings.clone();
                let write_blocked = !config.voice_rejected.is_empty();
                if write_blocked {
                    notices.push(format!(
                        "配置文件 {} 中被丢弃的变声条目仍保留在文件里；\
                         手动修复它们之前，保存配置会被拒绝，避免把它们永久删除",
                        path.display()
                    ));
                }
                LoadReport {
                    config,
                    notices,
                    write_blocked,
                }
            }
            Err(error) => LoadReport {
                config: Self::parse(&text),
                notices: vec![format!(
                    "配置文件 {} 不是有效 TOML（{}）。只读取了能识别的设置；\
                     修复前保存配置会被拒绝，原文件不会被覆盖",
                    path.display(),
                    describe_toml_error(&text, &error)
                )],
                write_blocked: true,
            },
        }
    }

    /// 写入前的严格读取：读出的配置可以改完直接 [`Config::save_to`]。
    ///
    /// 只有文件不存在才使用默认值。以下情况返回错误，调用方应原样报告并放弃写入，
    /// 磁盘上的文件保持不动：
    /// - 读取失败（权限、编码等）；
    /// - 不是有效 TOML：宽松回退只认旧版扁平键，重写会把 `[[sounds]]`、`[voice]`、
    ///   `[[voice_presets]]` 降级成注释；
    /// - 含有加载时被拒绝的变声条目（见 [`Config::voice_rejected`]）：它们不进内存，
    ///   写回就等于永久删除。
    ///
    /// 钳制与激活预设回退只是规范化数值，条目仍在，不阻止写入。
    pub fn load_for_write() -> Result<Self> {
        Self::load_for_write_from(&Self::path())
    }

    pub fn load_for_write_from(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("读取配置文件失败: {}", path.display()));
            }
        };
        let table = text.parse::<toml::Table>().map_err(|error| {
            anyhow::anyhow!(
                "配置文件 {} 不是有效 TOML（{}），已拒绝写入以免覆盖原内容；请先修复该文件",
                path.display(),
                describe_toml_error(&text, &error)
            )
        })?;
        let config = Self::from_table(table);
        if !config.voice_rejected.is_empty() {
            anyhow::bail!(
                "配置文件 {} 含有无法保留的变声条目，保存会把它们永久删除，已拒绝写入：{}。\
                 请先在文件中修复这些条目",
                path.display(),
                config.voice_rejected.join("；")
            );
        }
        Ok(config)
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = Self::path();
        self.save_to(&path)?;
        Ok(path)
    }

    /// 保存到指定路径，写入过程原子替换目标文件。
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("创建配置目录失败: {}", dir.display()))?;
        }
        write_atomic(path, &self.to_text())
    }

    // ── 解析

    /// 严格解析 TOML，供调用方在载入失败时向用户报告错误。
    pub fn try_parse(text: &str) -> Result<Self> {
        let table = text.parse::<toml::Table>().context("配置不是有效 TOML")?;
        Ok(Self::from_table(table))
    }

    pub fn parse(text: &str) -> Self {
        if let Ok(c) = Self::try_parse(text) {
            return c;
        }
        // 兼容早期写入的未加引号热键以及无效的独立行。所有有效值仍由 TOML
        // 解析，不能再把带逗号的引号字符串拆成不同音效。
        let mut table = toml::Table::new();
        let mut unknown = Vec::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if !KNOWN_KEYS.contains(&key) {
                unknown.push(raw.trim_end().to_owned());
                continue;
            }
            if let Ok(row) = line.parse::<toml::Table>() {
                table.extend(row);
            } else if matches!(
                key,
                "sound_files" | "sound_dirs" | "sound_names" | "sound_tags" | "hotkeys"
            ) {
                table.insert(
                    key.into(),
                    toml::Value::Array(
                        legacy_list(value)
                            .into_iter()
                            .map(toml::Value::String)
                            .collect(),
                    ),
                );
            } else if matches!(key, "input_device" | "output_device" | "monitor_device") {
                table.insert(
                    key.into(),
                    toml::Value::String(value.trim().trim_matches(['\"', '\'']).to_owned()),
                );
            }
        }
        let mut c = Self::from_table(table);
        c.unknown_lines = unknown;
        c
    }

    fn from_table(mut t: toml::Table) -> Self {
        let mut notes = VoiceLoadNotes::default();
        let voice = voice::parse_voice(t.remove("voice"), &mut notes);
        let voice_presets = voice::parse_voice_presets(t.remove("voice_presets"), &mut notes);
        let mut c = Self {
            voice,
            voice_presets,
            voice_warnings: notes.warnings,
            voice_rejected: notes.rejected,
            input_device: take_string(&mut t, "input_device"),
            output_device: take_string(&mut t, "output_device"),
            monitor_device: take_string(&mut t, "monitor_device"),
            buffer_ms: t
                .remove("buffer_ms")
                .and_then(|v| v.as_integer().and_then(|i| u32::try_from(i).ok())),
            mic_gain_db: take_number(&mut t, "mic_gain_db"),
            sfx_gain_db: take_number(&mut t, "sfx_gain_db"),
            monitor: t
                .remove("monitor")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            looping: t
                .remove("looping")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            sound_files: take_list(&mut t, "sound_files")
                .into_iter()
                .map(expand)
                .collect(),
            sound_dirs: take_list(&mut t, "sound_dirs")
                .into_iter()
                .map(expand)
                .collect(),
            sound_names: take_list(&mut t, "sound_names"),
            sound_tags: take_list(&mut t, "sound_tags"),
            sound_ids: take_list(&mut t, "sound_ids"),
            hotkeys: take_list(&mut t, "hotkeys"),
            sound_gains: t
                .remove("sound_gains")
                .and_then(|v| {
                    v.as_array().map(|a| {
                        a.iter()
                            .map(number_value)
                            .map(|v| v.unwrap_or(0.0))
                            .collect()
                    })
                })
                .unwrap_or_default(),
            ..Self::default()
        };
        if let Some(toml::Value::Array(rows)) = t.remove("sounds") {
            let entries: Vec<_> = rows
                .into_iter()
                .filter_map(|row| {
                    let mut row = row.as_table()?.clone();
                    let path = expand(take_string(&mut row, "path")?);
                    Some(SoundEntry {
                        id: take_string(&mut row, "id").unwrap_or_default(),
                        path,
                        name: take_string(&mut row, "name").unwrap_or_default(),
                        gain_db: take_number(&mut row, "gain_db").unwrap_or(0.0),
                        tags: take_list(&mut row, "tags"),
                        hotkey: take_string(&mut row, "hotkey").unwrap_or_default(),
                    })
                })
                .collect();
            c.set_sound_entries(&entries);
        }
        let (active_preset, enabled) = c.voice.effective_active_preset(&c.voice_presets);
        if active_preset != c.voice.active_preset {
            c.voice_warnings.push(format!(
                "变声激活预设「{}」不存在，已回退到「{active_preset}」并关闭变声",
                c.voice.active_preset
            ));
            c.voice.active_preset = active_preset;
            c.voice.enabled = enabled;
        }
        c.unknown_lines = toml::to_string(&t)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        c
    }

    pub fn to_text(&self) -> String {
        let mut t = toml::Table::new();
        for (key, value) in [
            ("input_device", &self.input_device),
            ("output_device", &self.output_device),
            ("monitor_device", &self.monitor_device),
        ] {
            if let Some(v) = value {
                t.insert(key.into(), toml::Value::String(v.clone()));
            }
        }
        if let Some(v) = self.buffer_ms {
            t.insert("buffer_ms".into(), toml::Value::Integer(v.into()));
        }
        for (key, value) in [
            ("mic_gain_db", self.mic_gain_db),
            ("sfx_gain_db", self.sfx_gain_db),
        ] {
            if let Some(v) = value {
                t.insert(key.into(), toml::Value::Float(v.into()));
            }
        }
        t.insert("monitor".into(), toml::Value::Boolean(self.monitor));
        t.insert("looping".into(), toml::Value::Boolean(self.looping));
        insert_list(
            &mut t,
            "sound_dirs",
            self.sound_dirs
                .iter()
                .map(|p| p.to_string_lossy().into_owned()),
        );
        if self.sound_ids.is_empty() {
            insert_list(
                &mut t,
                "sound_files",
                self.sound_files
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned()),
            );
            insert_list(&mut t, "sound_names", self.sound_names.clone());
            insert_list(&mut t, "sound_tags", self.sound_tags.clone());
            insert_list(&mut t, "hotkeys", self.hotkeys.clone());
            if !self.sound_gains.is_empty() {
                t.insert(
                    "sound_gains".into(),
                    toml::Value::Array(
                        self.sound_gains
                            .iter()
                            .map(|g| toml::Value::Float((*g).into()))
                            .collect(),
                    ),
                );
            }
        } else {
            let rows = self
                .sound_entries()
                .into_iter()
                .map(|e| {
                    let mut row = toml::Table::new();
                    row.insert("id".into(), toml::Value::String(e.id));
                    row.insert(
                        "path".into(),
                        toml::Value::String(e.path.to_string_lossy().into_owned()),
                    );
                    row.insert("name".into(), toml::Value::String(e.name));
                    row.insert("gain_db".into(), toml::Value::Float(e.gain_db.into()));
                    row.insert("hotkey".into(), toml::Value::String(e.hotkey));
                    row.insert(
                        "tags".into(),
                        toml::Value::Array(e.tags.into_iter().map(toml::Value::String).collect()),
                    );
                    toml::Value::Table(row)
                })
                .collect();
            t.insert("sounds".into(), toml::Value::Array(rows));
        }
        voice::write_voice(&mut t, &self.voice, &self.voice_presets);
        // Unknown entries must join the document before serialization. Appending
        // them after [[sounds]] would attach them to the final sound record.
        let mut notes = Vec::new();
        let unknown = self
            .unknown_lines
            .iter()
            .map(|line| {
                if let Some((key, value)) = line.split_once('=') {
                    let key = key.trim();
                    if !key.starts_with(['"', '\''])
                        && !key
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
                    {
                        notes.push(format!("# 旧版原文: {line}"));
                        return format!("{} = {value}", toml::Value::String(key.to_owned()));
                    }
                }
                line.clone()
            })
            .collect::<Vec<_>>()
            .join("\n");
        match unknown.parse::<toml::Table>() {
            Ok(unknown) => {
                for (key, value) in unknown {
                    if !t.contains_key(&key) {
                        t.insert(key, value);
                    }
                }
            }
            Err(_) => notes.extend(
                self.unknown_lines
                    .iter()
                    .map(|line| format!("# 无法解析的旧配置: {line}")),
            ),
        }
        let mut text = String::from("# mixer 配置\n\n");
        if !notes.is_empty() {
            text.push_str(&notes.join("\n"));
            text.push('\n');
        }
        text.push_str(&toml::to_string(&t).expect("TOML values are serializable"));
        text
    }

    /// 读取一项配置的文本表示。与 `set` 共用点路径解析，未知项返回错误。
    ///
    /// 未设置的设备与数值输出空行，便于调用方判断"未配置"；列表按逗号分隔，
    /// 与 `set` 的输入格式一致。布尔值输出 `true` / `false`。
    pub fn get(&self, key: &str) -> Result<String> {
        let value = match ConfigKey::parse(key)? {
            ConfigKey::InputDevice => self.input_device.clone().unwrap_or_default(),
            ConfigKey::OutputDevice => self.output_device.clone().unwrap_or_default(),
            ConfigKey::MonitorDevice => self.monitor_device.clone().unwrap_or_default(),
            ConfigKey::BufferMs => self.buffer_ms.map(|v| v.to_string()).unwrap_or_default(),
            ConfigKey::MicGainDb => self.mic_gain_db.map(|v| v.to_string()).unwrap_or_default(),
            ConfigKey::SfxGainDb => self.sfx_gain_db.map(|v| v.to_string()).unwrap_or_default(),
            ConfigKey::Monitor => self.monitor.to_string(),
            ConfigKey::Looping => self.looping.to_string(),
            ConfigKey::SoundDirs => join_paths(&self.sound_dirs),
            ConfigKey::SoundFiles => join_paths(&self.sound_files),
            ConfigKey::SoundNames => self.sound_names.join("\n"),
            ConfigKey::SoundTags => self.sound_tags.join("\n"),
            ConfigKey::SoundGains => self
                .sound_gains
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(","),
            ConfigKey::Hotkeys => self.hotkeys.join(","),
            ConfigKey::VoiceEnabled => self.voice.enabled.to_string(),
            ConfigKey::VoiceActivePreset => self.voice.active_preset.clone(),
            ConfigKey::VoiceBypassHotkey => self.voice.bypass_hotkey.clone(),
            ConfigKey::VoiceNextPresetHotkey => self.voice.next_preset_hotkey.clone(),
            ConfigKey::VoicePrevPresetHotkey => self.voice.prev_preset_hotkey.clone(),
        };
        Ok(value)
    }

    /// 用 `key value` 设置一项。返回是否成功。
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let v = value.trim();
        match ConfigKey::parse(key)? {
            ConfigKey::InputDevice => {
                self.input_device = if v.is_empty() || v == "none" {
                    None
                } else {
                    Some(v.to_string())
                }
            }
            ConfigKey::OutputDevice => {
                self.output_device = if v.is_empty() || v == "none" {
                    None
                } else {
                    Some(v.to_string())
                }
            }
            ConfigKey::BufferMs => {
                self.buffer_ms = Some(v.parse().context("buffer_ms 需要是整数")?)
            }
            ConfigKey::MicGainDb => {
                self.mic_gain_db = Some(v.parse().context("mic_gain_db 需要是数字")?)
            }
            ConfigKey::SfxGainDb => {
                self.sfx_gain_db = Some(v.parse().context("sfx_gain_db 需要是数字")?)
            }
            ConfigKey::SoundDirs => {
                self.sound_dirs = v
                    .split(',')
                    .map(|x| x.trim())
                    .filter(|x| !x.is_empty())
                    .map(|x| expand(x.to_string()))
                    .collect()
            }
            ConfigKey::SoundFiles => {
                self.sound_files = v
                    .split(',')
                    .map(|x| x.trim())
                    .filter(|x| !x.is_empty())
                    .map(|x| expand(x.to_string()))
                    .collect()
            }
            ConfigKey::Monitor => self.monitor = parse_bool(v),
            ConfigKey::Looping => {
                self.looping = v.parse().context("looping 需要是 true 或 false")?
            }
            ConfigKey::MonitorDevice => self.monitor_device = parse_str(v),
            ConfigKey::SoundNames => {
                self.sound_names = v.split('\n').map(|x| x.trim().to_string()).collect()
            }
            ConfigKey::SoundTags => {
                self.sound_tags = v.split('\n').map(|x| x.trim().to_string()).collect()
            }
            ConfigKey::SoundGains => {
                self.sound_gains = v
                    .split(',')
                    .map(|x| x.trim())
                    .filter(|x| !x.is_empty())
                    .filter_map(|x| x.parse::<f32>().ok())
                    .collect()
            }
            ConfigKey::Hotkeys => {
                self.hotkeys = if v.is_empty() {
                    Vec::new()
                } else {
                    v.split(',')
                        .map(|x| x.trim())
                        .map(|x| x.to_string())
                        .collect()
                }
            }
            ConfigKey::VoiceEnabled => {
                self.voice.enabled = v.parse().context("voice.enabled 需要是 true 或 false")?
            }
            ConfigKey::VoiceActivePreset => {
                if self.find_voice_preset(v).is_none() {
                    anyhow::bail!("未知的变声预设「{v}」");
                }
                self.voice.active_preset = v.to_owned();
            }
            ConfigKey::VoiceBypassHotkey => self.voice.bypass_hotkey = v.to_owned(),
            ConfigKey::VoiceNextPresetHotkey => self.voice.next_preset_hotkey = v.to_owned(),
            ConfigKey::VoicePrevPresetHotkey => self.voice.prev_preset_hotkey = v.to_owned(),
        }
        Ok(())
    }

    /// 当前生效的音效列表：**显式文件列表优先**，否则扫描目录。
    pub fn collect_sounds(&self) -> Result<Vec<PathBuf>> {
        if !self.sound_files.is_empty() {
            return Ok(self.sound_files.clone());
        }
        scan_dirs(&self.sound_dirs)
    }

    /// 把并行数组补齐到 `count` 长度，返回统一视图。
    ///
    /// 配置里是**并行数组**（文件名/音量/显示名/标签各一条），
    /// 任何一处长度不一致都要能安全补齐 —— 用户手改配置、或新加了音效都会遇到。
    pub fn sound_items(&self) -> Vec<SoundItem> {
        let n = self.sound_files.len();
        let g = self.normalized_gains(n);
        (0..n)
            .map(|i| SoundItem {
                path: self.sound_files[i].clone(),
                gain_db: g[i],
                name: self
                    .sound_names
                    .get(i)
                    .cloned()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| {
                        self.sound_files[i]
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| "未命名".into())
                    }),
                tags: self
                    .sound_tags
                    .get(i)
                    .map(|t| parse_tags(t))
                    .unwrap_or_default(),
            })
            .collect()
    }

    /// 将旧配置映射为包含标识和热键的完整条目。
    pub fn sound_entries(&self) -> Vec<SoundEntry> {
        self.sound_items()
            .into_iter()
            .enumerate()
            .map(|(i, item)| SoundEntry {
                id: self
                    .sound_ids
                    .get(i)
                    .filter(|id| !id.is_empty())
                    .cloned()
                    .unwrap_or_else(|| {
                        stable_sound_id(
                            &item.path,
                            self.sound_files[..i]
                                .iter()
                                .filter(|p| **p == item.path)
                                .count(),
                        )
                    }),
                path: item.path,
                name: item.name,
                gain_db: item.gain_db,
                tags: item.tags,
                hotkey: self.hotkeys.get(i).cloned().unwrap_or_default(),
            })
            .collect()
    }

    pub fn set_sound_entries(&mut self, entries: &[SoundEntry]) {
        self.sound_files = entries.iter().map(|e| e.path.clone()).collect();
        self.sound_ids = entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                if e.id.is_empty() {
                    stable_sound_id(
                        &e.path,
                        entries[..i].iter().filter(|p| p.path == e.path).count(),
                    )
                } else {
                    e.id.clone()
                }
            })
            .collect();
        self.sound_names = entries.iter().map(|e| e.name.clone()).collect();
        self.sound_gains = entries.iter().map(|e| e.gain_db).collect();
        // TOML in the compatibility array preserves punctuation inside individual tags.
        self.sound_tags = entries
            .iter()
            .map(|e| {
                toml::Value::Array(e.tags.iter().cloned().map(toml::Value::String).collect())
                    .to_string()
            })
            .collect();
        self.hotkeys = entries.iter().map(|e| e.hotkey.clone()).collect();
    }

    /// 用一组音效条目覆盖配置里的并行数组。
    pub fn set_sound_items(&mut self, items: &[SoundItem]) {
        let previous = self.sound_entries();
        let entries: Vec<_> = items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                // Match each duplicate by occurrence; moving or removing other files preserves IDs.
                let occurrence = items[..i].iter().filter(|e| e.path == item.path).count();
                let old = previous
                    .iter()
                    .filter(|e| e.path == item.path)
                    .nth(occurrence);
                SoundEntry {
                    id: old
                        .map(|e| e.id.clone())
                        .unwrap_or_else(|| stable_sound_id(&item.path, occurrence)),
                    path: item.path.clone(),
                    name: item.name.clone(),
                    gain_db: item.gain_db,
                    tags: item.tags.clone(),
                    hotkey: old.map(|e| e.hotkey.clone()).unwrap_or_default(),
                }
            })
            .collect();
        self.set_sound_entries(&entries);
    }

    /// 补齐音量表长度（新增音效默认 0 dB）。
    pub fn normalized_gains(&self, count: usize) -> Vec<f32> {
        let mut g = self.sound_gains.clone();
        g.resize(count, 0.0);
        g
    }
}

/// `config get/set` 支持的点路径。
///
/// 键名解析与错误文案只在这里维护一份，避免 `get` 与 `set` 对同一项给出不同判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigKey {
    InputDevice,
    OutputDevice,
    MonitorDevice,
    BufferMs,
    MicGainDb,
    SfxGainDb,
    Monitor,
    Looping,
    SoundDirs,
    SoundFiles,
    SoundNames,
    SoundTags,
    SoundGains,
    Hotkeys,
    VoiceEnabled,
    VoiceActivePreset,
    VoiceBypassHotkey,
    VoiceNextPresetHotkey,
    VoicePrevPresetHotkey,
}

const CONFIG_KEY_LIST: &str = "input_device / output_device / monitor_device / buffer_ms / \
     mic_gain_db / sfx_gain_db / sound_dirs / sound_files / sound_names / sound_tags / \
     sound_gains / monitor / looping / hotkeys / voice.enabled / voice.active_preset / \
     voice.bypass_hotkey / voice.next_preset_hotkey / voice.prev_preset_hotkey";

impl ConfigKey {
    fn parse(key: &str) -> Result<Self> {
        Ok(match key {
            "input_device" => Self::InputDevice,
            "output_device" => Self::OutputDevice,
            "monitor_device" => Self::MonitorDevice,
            "buffer_ms" => Self::BufferMs,
            "mic_gain_db" => Self::MicGainDb,
            "sfx_gain_db" => Self::SfxGainDb,
            "monitor" => Self::Monitor,
            "looping" => Self::Looping,
            "sound_dirs" => Self::SoundDirs,
            "sound_files" => Self::SoundFiles,
            "sound_names" => Self::SoundNames,
            "sound_tags" => Self::SoundTags,
            "sound_gains" => Self::SoundGains,
            "hotkeys" => Self::Hotkeys,
            "voice.enabled" => Self::VoiceEnabled,
            "voice.active_preset" => Self::VoiceActivePreset,
            "voice.bypass_hotkey" => Self::VoiceBypassHotkey,
            "voice.next_preset_hotkey" => Self::VoiceNextPresetHotkey,
            "voice.prev_preset_hotkey" => Self::VoicePrevPresetHotkey,
            other => anyhow::bail!("未知配置项「{other}」。可用：{CONFIG_KEY_LIST}"),
        })
    }
}

/// 把 TOML 解析错误整理成「第 N 行：原因」，避免带出多行源码片段。
fn describe_toml_error(text: &str, error: &toml::de::Error) -> String {
    match error.span() {
        Some(span) => {
            let upto = span.start.min(text.len());
            let line = text.as_bytes()[..upto]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count()
                + 1;
            format!("第 {line} 行：{}", error.message())
        }
        None => error.message().to_owned(),
    }
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join(",")
}

/// 原子写入配置文本：同目录临时文件写完并同步后，重命名覆盖目标。
///
/// 临时文件必须与目标同目录，重命名才不会跨文件系统。任一步失败都删除临时文件，
/// 目标路径上的旧配置保持原样。
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("配置路径缺少文件名: {}", path.display()))?;
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let temp_path = dir.join(format!(
        ".{}.{}.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        nanos,
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));

    let result = (|| -> Result<()> {
        let mut file = std::fs::File::create(&temp_path)
            .with_context(|| format!("创建临时配置文件失败: {}", temp_path.display()))?;
        file.write_all(text.as_bytes())
            .with_context(|| format!("写入临时配置文件失败: {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("同步临时配置文件失败: {}", temp_path.display()))?;
        drop(file);
        std::fs::rename(&temp_path, path)
            .with_context(|| format!("替换配置文件失败: {}", path.display()))
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

fn parse_str(v: &str) -> Option<String> {
    let t = v.trim().trim_matches('"').trim_matches('\'');
    if t.is_empty() || t == "none" {
        None
    } else {
        Some(t.to_string())
    }
}

/// 解析布尔值。缺失或无法识别时按 `true`（`monitor` 键的缺省即开启）。
///
/// 认这几种"假"的写法：`false` / `0` / `no` / `off`（大小写不敏感）。
fn parse_bool(v: &str) -> bool {
    !matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "false" | "0" | "no" | "off"
    )
}

const KNOWN_KEYS: &[&str] = &[
    "input_device",
    "output_device",
    "monitor_device",
    "buffer_ms",
    "mic_gain_db",
    "sfx_gain_db",
    "monitor",
    "looping",
    "sound_files",
    "sound_dirs",
    "sound_names",
    "sound_tags",
    "sound_ids",
    "sound_gains",
    "hotkeys",
    "sounds",
];

fn take_string(t: &mut toml::Table, key: &str) -> Option<String> {
    t.remove(key)
        .and_then(|v| v.as_str().map(str::to_owned))
        .filter(|v| !v.is_empty() && v != "none")
}
fn number_value(v: &toml::Value) -> Option<f32> {
    v.as_float()
        .map(|n| n as f32)
        .or_else(|| v.as_integer().map(|n| n as f32))
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}
fn take_number(t: &mut toml::Table, key: &str) -> Option<f32> {
    t.remove(key).as_ref().and_then(number_value)
}
fn take_list(t: &mut toml::Table, key: &str) -> Vec<String> {
    t.remove(key)
        .and_then(|v| {
            v.as_array().map(|a| {
                a.iter()
                    .map(|v| v.as_str().unwrap_or_default().to_owned())
                    .collect()
            })
        })
        .unwrap_or_default()
}
fn insert_list(t: &mut toml::Table, key: &str, values: impl IntoIterator<Item = String>) {
    let list: Vec<_> = values.into_iter().map(toml::Value::String).collect();
    if !list.is_empty() {
        t.insert(key.into(), toml::Value::Array(list));
    }
}
fn parse_tags(tags: &str) -> Vec<String> {
    if let Ok(mut t) = format!("tags = {tags}").parse::<toml::Table>() {
        if t.get("tags").is_some_and(|v| v.is_array()) {
            return take_list(&mut t, "tags");
        }
    }
    tags.split(['|', ',', ';'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}
/// Deterministic FNV identifier for migration; stored IDs survive later file renames.
pub fn stable_sound_id(path: &Path, occurrence: usize) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in path
        .to_string_lossy()
        .bytes()
        .chain(occurrence.to_le_bytes())
    {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    format!("sound-{hash:016x}")
}
fn legacy_list(value: &str) -> Vec<String> {
    let inner = value.trim().trim_start_matches('[').trim_end_matches(']');
    let mut quote = None;
    let mut part = String::new();
    let mut parts = Vec::new();
    for ch in inner.chars() {
        if matches!(ch, '\"' | '\'') && (quote.is_none() || quote == Some(ch)) {
            quote = if quote.is_some() { None } else { Some(ch) };
        } else if ch == ',' && quote.is_none() {
            parts.push(part.trim().to_owned());
            part.clear();
        } else {
            part.push(ch);
        }
    }
    if !part.is_empty() || !parts.is_empty() {
        parts.push(part.trim().to_owned());
    }
    parts
}

/// 展开 `~/`，与 GUI 目录输入共用平台的主目录规则。
fn expand(p: String) -> PathBuf {
    expand_on(&p, Platform::current(), &|key| std::env::var_os(key))
}

fn expand_on(p: &str, platform: Platform, env: EnvFn) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = home_dir_on(platform, env) {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

/// 路径解析的单元测试。
///
/// **这组测试的价值在于：它们在 macOS 上跑，验证的却是 Windows 的分支。**
/// 交叉编译只能证明那些代码能编译；这里证明它算得对。
/// 动机是实际踩到的坑：Windows 上没有 `HOME`/`XDG_CONFIG_HOME`，
/// 旧实现一路回退到 `"."`（当前工作目录），用户的设置会莫名其妙丢失。
#[cfg(test)]
mod path_tests {
    // 测试名用中文描述 + 保留环境变量的大写写法（APPDATA / XDG / HOME / LOCALAPPDATA…）。
    // 改成 snake_case 要把缩写降成小写，可读性反而更差 —— 这里 lint 是错的。
    #![allow(non_snake_case)]
    use super::*;
    use std::ffi::OsString;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn windows_配置路径优先用_APPDATA() {
        let env = env_of(&[
            ("APPDATA", "/APPDATA"),
            ("USERPROFILE", "/USERPROFILE"),
            ("HOME", "/HOME"),
            ("XDG_CONFIG_HOME", "/XDG"),
        ]);
        let got = base_dir_on(Platform::Windows, &env, std::path::Path::new("/TMP"));
        assert_eq!(got, PathBuf::from("/APPDATA"), "Windows 上必须优先 APPDATA");
    }

    #[test]
    fn windows_没有_APPDATA_时退回_USERPROFILE() {
        let env = env_of(&[("USERPROFILE", "/USERPROFILE"), ("HOME", "/HOME")]);
        let got = base_dir_on(Platform::Windows, &env, std::path::Path::new("/TMP"));
        assert_eq!(got, PathBuf::from("/USERPROFILE/AppData/Roaming"));
    }

    #[test]
    fn windows_环境变量全空时_绝不能落到当前目录() {
        // 这一条是整个模块存在的理由：旧实现会返回 "."（当前工作目录），
        // GUI 的 CWD 不可预测，用户会看到"设置时好时坏"。
        let env = env_of(&[]);
        let got = base_dir_on(Platform::Windows, &env, std::path::Path::new("/TMP"));
        assert_eq!(got, PathBuf::from("/TMP/mixer-config"));
        assert_ne!(got, PathBuf::from("."));
        assert_ne!(got, PathBuf::from(""));
    }

    #[test]
    fn unix_配置路径优先用_XDG() {
        let env = env_of(&[("XDG_CONFIG_HOME", "/XDG"), ("HOME", "/HOME")]);
        assert_eq!(
            base_dir_on(Platform::Unix, &env, std::path::Path::new("/TMP")),
            PathBuf::from("/XDG")
        );
    }

    #[test]
    fn unix_没有_XDG_时用_HOME_下的_config() {
        let env = env_of(&[("HOME", "/HOME")]);
        assert_eq!(
            base_dir_on(Platform::Unix, &env, std::path::Path::new("/TMP")),
            PathBuf::from("/HOME/.config")
        );
    }

    #[test]
    fn 空字符串等同于没设置() {
        // 有些环境会把变量设成空串。当成"有值"的话路径会变成相对路径。
        let env = env_of(&[("APPDATA", ""), ("USERPROFILE", "/USERPROFILE")]);
        assert_eq!(
            base_dir_on(Platform::Windows, &env, std::path::Path::new("/TMP")),
            PathBuf::from("/USERPROFILE/AppData/Roaming")
        );
    }

    #[test]
    fn 主目录在_windows_上认_USERPROFILE_而不是_HOME() {
        let env = env_of(&[("USERPROFILE", "/USERPROFILE"), ("HOME", "/HOME")]);
        assert_eq!(
            home_dir_on(Platform::Windows, &env),
            Some(PathBuf::from("/USERPROFILE"))
        );
        // 反过来：Windows 上**没有** USERPROFILE 就是 None，不能去读 HOME
        let env2 = env_of(&[("HOME", "/HOME")]);
        assert_eq!(home_dir_on(Platform::Windows, &env2), None);
    }

    #[test]
    fn macos_目录别名使用_HOME_并保留中文空格和逗号() {
        let env = env_of(&[
            ("HOME", "/Users/测试 用户"),
            ("USERPROFILE", "/wrong-profile"),
        ]);
        assert_eq!(
            expand_on("~/音频 素材,一", Platform::Unix, &env),
            PathBuf::from("/Users/测试 用户").join("音频 素材,一")
        );
    }

    #[test]
    fn windows_目录别名使用_USERPROFILE_不依赖_HOME() {
        let env = env_of(&[("USERPROFILE", r"C:\Users\测试 用户")]);
        let expected = PathBuf::from(r"C:\Users\测试 用户").join("音频 素材,一");
        assert_eq!(
            expand_on("~/音频 素材,一", Platform::Windows, &env),
            expected
        );

        // Git Bash 等环境可能也设置 HOME，仍与 GUI 一致使用 USERPROFILE。
        let env = env_of(&[
            ("USERPROFILE", r"D:\Users\测试 用户"),
            ("HOME", "/wrong-home"),
        ]);
        assert_eq!(
            expand_on("~/Sounds", Platform::Windows, &env),
            PathBuf::from(r"D:\Users\测试 用户").join("Sounds")
        );
    }

    #[test]
    fn 没有平台主目录时保留目录别名() {
        for (platform, pairs) in [
            (Platform::Windows, vec![]),
            (Platform::Windows, vec![("USERPROFILE", "")]),
            (Platform::Windows, vec![("HOME", "/wrong-home")]),
            (Platform::Unix, vec![]),
            (Platform::Unix, vec![("HOME", "")]),
            (Platform::Unix, vec![("USERPROFILE", "/wrong-profile")]),
        ] {
            assert_eq!(
                expand_on("~/Sounds", platform, &env_of(&pairs)),
                PathBuf::from("~/Sounds"),
                "不能改成依赖当前工作目录的 Sounds：{platform:?} {pairs:?}"
            );
        }
    }

    #[test]
    fn 主目录展开不改写绝对路径或普通相对路径() {
        let env = env_of(&[("HOME", "/home"), ("USERPROFILE", r"C:\Users\测试")]);
        for platform in [Platform::Unix, Platform::Windows] {
            for path in [
                "/Volumes/音频 素材/声音,一.wav",
                r"D:\音频 素材\声音,一.wav",
                r"\\server\share\音频 素材\声音.wav",
                "relative/sound.wav",
                "~other/Sounds",
            ] {
                assert_eq!(expand_on(path, platform, &env), PathBuf::from(path));
            }
        }
    }

    #[test]
    fn 当前平台能被正确识别() {
        let p = Platform::current();
        if cfg!(target_os = "windows") {
            assert_eq!(p, Platform::Windows);
        } else {
            assert_eq!(p, Platform::Unix);
        }
        // 配置文件的**相对结构**两个平台必须一致（根目录不同，但下面这截相同）
        let full = Config::path();
        assert!(
            full.ends_with("mixer/config.toml"),
            "配置文件路径结构不对：{}",
            full.display()
        );
        // 而且必须真的是绝对路径 —— 相对路径会随启动方式漂移
        assert!(
            full.is_absolute(),
            "配置文件路径必须是绝对的，实际是：{}",
            full.display()
        );
    }
}

#[cfg(test)]
mod tests {
    // 测试名保留 ID 等缩写，行为描述比机械转成 snake_case 更重要。
    #![allow(non_snake_case)]

    use super::*;

    #[test]
    fn toml_preserves_empty_tags_punctuation_and_windows_paths() {
        let c = Config {
            sound_files: vec![
                PathBuf::from(r"C:\Users\Doe, John\a\sound.wav"),
                PathBuf::from("/b.wav"),
            ],
            sound_names: vec!["嘿,你好吗 \"quoted\"".into(), "B".into()],
            sound_tags: vec![String::new(), "B-only".into()],
            ..Default::default()
        };
        let text = c.to_text();
        assert!(Config::try_parse(&text).is_ok());
        let round = Config::parse(&text);
        assert_eq!(round, c);
        assert!(round.sound_items()[0].tags.is_empty());
        assert_eq!(round.sound_items()[1].tags, ["B-only"]);
    }

    #[test]
    fn structured_sound_ids_survive_reordering_and_tag_punctuation() {
        let mut c = Config::default();
        c.set_sound_entries(&[
            SoundEntry {
                id: "first".into(),
                path: "a.wav".into(),
                name: "A".into(),
                gain_db: -3.0,
                tags: vec!["one,two".into(), "quoted \"tag\"".into()],
                hotkey: "F1".into(),
            },
            SoundEntry {
                id: "second".into(),
                path: "b.wav".into(),
                name: "B".into(),
                gain_db: 2.0,
                tags: vec![],
                hotkey: "F2".into(),
            },
        ]);
        let expected = c.sound_entries();
        let round = Config::try_parse(&c.to_text()).unwrap();
        assert_eq!(round.sound_entries(), expected);
        let mut items = round.sound_items();
        items.reverse();
        c.set_sound_items(&items);
        assert_eq!(c.sound_entries()[0].id, "second");
        assert_eq!(c.sound_entries()[0].hotkey, "F2");
        assert_eq!(c.sound_entries()[1].tags, expected[0].tags);
    }

    #[test]
    fn canonical_sounds_do_not_capture_unknown_top_level_keys_or_nested_tables() {
        let text = r#"
future_scalar = "keep,me"
future_array = ["a", "b"]
[[sounds]]
id = "stable-id"
path = 'C:\Users\Doe, John\sound.wav'
name = "Good"
gain_db = -9.0
tags = []
hotkey = "F2"
[future.nested]
flag = true
[[future.rows]]
value = "first"
[[future.rows]]
value = "second"
"#;
        let config = Config::try_parse(text).unwrap();
        let serialized = config.to_text();
        let document = serialized.parse::<toml::Table>().unwrap();
        assert_eq!(document["future_scalar"].as_str(), Some("keep,me"));
        assert_eq!(document["future_array"].as_array().unwrap().len(), 2);
        assert_eq!(document["future"]["rows"].as_array().unwrap().len(), 2);
        assert_eq!(document["future"]["nested"]["flag"].as_bool(), Some(true));
        let sound = &document["sounds"].as_array().unwrap()[0];
        assert!(sound.get("future_scalar").is_none());
        let round = Config::try_parse(&serialized).unwrap();
        assert_eq!(round.sound_entries(), config.sound_entries());
        assert_eq!(round.to_text().parse::<toml::Table>().unwrap(), document);
    }

    #[test]
    fn legacy_unicode_unknown_keys_become_valid_toml_without_losing_their_value() {
        let mut config = Config::parse("手写的键 = \"别弄丢我\"\nfuture_scalar = 123\n");
        config.set_sound_entries(&[SoundEntry {
            id: "id".into(),
            path: "x.wav".into(),
            name: "X".into(),
            gain_db: 0.0,
            tags: vec![],
            hotkey: String::new(),
        }]);
        let serialized = config.to_text();
        let document = serialized.parse::<toml::Table>().unwrap();
        assert_eq!(document["手写的键"].as_str(), Some("别弄丢我"));
        assert_eq!(document["future_scalar"].as_integer(), Some(123));
        assert_eq!(
            Config::try_parse(&serialized).unwrap().sound_entries(),
            config.sound_entries()
        );
    }

    #[test]
    fn multiline_toml_lists_and_unknown_tables_survive() {
        let text = "sound_files = [\n  'a,b.wav',\n  'c.wav',\n]\nsound_tags = ['', 'B-only']\n[future]\nvalue = ['keep,me']\n";
        let c = Config::try_parse(text).unwrap();
        assert_eq!(c.sound_files.len(), 2);
        let round = Config::try_parse(&c.to_text()).unwrap();
        assert_eq!(round.sound_tags, ["", "B-only"]);
        assert!(round.to_text().contains("keep,me"));
    }

    #[test]
    fn 解析基本键值对() {
        let c = Config::parse(
            r#"
# 注释
input_device = "麦克风"
output_device = "VB-Cable"
buffer_ms = 30
mic_gain_db = -2.5
sfx_gain_db = -9
"#,
        );
        assert_eq!(c.input_device.as_deref(), Some("麦克风"));
        assert_eq!(c.output_device.as_deref(), Some("VB-Cable"));
        assert_eq!(c.buffer_ms, Some(30));
        assert_eq!(c.mic_gain_db, Some(-2.5));
        assert_eq!(c.sfx_gain_db, Some(-9.0));
    }

    #[test]
    fn 解析数组两种写法() {
        let c = Config::parse(r#"hotkeys = ["F1", "Cmd+Shift+A"]"#);
        assert_eq!(c.hotkeys, vec!["F1", "Cmd+Shift+A"]);

        let c2 = Config::parse("hotkeys = F1, F2 , F3");
        assert_eq!(c2.hotkeys, vec!["F1", "F2", "F3"]);
    }

    #[test]
    fn 单引号与空值都能处理() {
        let c = Config::parse("input_device = 'x'\noutput_device = \"\"\n");
        assert_eq!(c.input_device.as_deref(), Some("x"));
        assert_eq!(c.output_device, None, "空字符串应视为未设置");
    }

    #[test]
    fn 未知键不报错且被保留_畸形行直接丢弃() {
        let c = Config::parse("future_key = 1\n这是一行垃圾\nbuffer_ms = 25\n");
        // 已知项照常解析
        assert_eq!(c.buffer_ms, Some(25));
        // 不认识的**键值行**保留下来（保存时写回去，避免抹掉用户手改的内容）
        assert_eq!(c.unknown_lines, vec!["future_key = 1".to_string()]);
        // 连 `=` 都没有的行是垃圾，丢掉 —— 保留它没有意义
        let out = c.to_text();
        assert!(out.contains("future_key = 1"));
        assert!(!out.contains("这是一行垃圾"));
    }

    #[test]
    fn 往返序列化保持一致() {
        let c = Config {
            input_device: Some("mic".into()),
            output_device: Some("cable".into()),
            buffer_ms: Some(20),
            mic_gain_db: Some(0.0),
            sfx_gain_db: Some(-6.0),
            sound_dirs: vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")],
            sound_files: vec![PathBuf::from("/tmp/x.mp3")],
            sound_ids: Vec::new(),
            sound_gains: vec![0.0, -6.0],
            sound_names: Vec::new(),
            sound_tags: Vec::new(),
            monitor: false,
            looping: false,
            monitor_device: None,
            hotkeys: vec!["F1".into(), "F2".into()],
            voice: VoiceConfig::default(),
            voice_presets: Vec::new(),
            voice_warnings: Vec::new(),
            voice_rejected: Vec::new(),
            unknown_lines: Vec::new(),
        };
        let round = Config::parse(&c.to_text());
        assert_eq!(round, c, "序列化再解析应完全一致");
    }

    #[test]
    fn set_能改各项且校验类型() {
        let mut c = Config::default();
        c.set("output_device", "VB-Cable").unwrap();
        assert_eq!(c.output_device.as_deref(), Some("VB-Cable"));
        c.set("output_device", "none").unwrap();
        assert_eq!(c.output_device, None);
        c.set("buffer_ms", "40").unwrap();
        assert_eq!(c.buffer_ms, Some(40));
        assert!(c.set("buffer_ms", "abc").is_err(), "非法整数应报错");
        assert!(c.set("不存在的项", "1").is_err(), "未知键应报错");
    }

    #[test]
    fn 显式文件列表与音量应能往返() {
        let c = Config::parse(
            "sound_files = [\"/a.mp3\", \"/b.wav\"]\nsound_gains = [\"0\", \"-6.5\"]\n",
        );
        assert_eq!(c.sound_files.len(), 2);
        assert_eq!(c.sound_gains, vec![0.0, -6.5]);
        let round = Config::parse(&c.to_text());
        assert_eq!(round.sound_files, c.sound_files);
        assert_eq!(round.sound_gains, c.sound_gains);
    }

    #[test]
    fn 显式文件列表优先于目录() {
        let c = Config {
            sound_dirs: vec![PathBuf::from("/nonexistent_dir_xyz")],
            sound_files: vec![PathBuf::from("/tmp/whatever.mp3")],
            ..Default::default()
        };
        let got = c.collect_sounds().unwrap();
        assert_eq!(got, c.sound_files, "有显式列表时不应去扫目录");
    }

    #[test]
    fn 音量表应自动补齐() {
        let c = Config::parse("sound_gains = [\"-3\"]");
        assert_eq!(c.normalized_gains(3), vec![-3.0, 0.0, 0.0]);
        assert_eq!(c.normalized_gains(1), vec![-3.0]);
    }

    #[test]
    fn 监听开关应能往返且默认开启() {
        assert!(Config::default().monitor, "默认应开启监听");
        assert!(!Config::parse("monitor = false").monitor);
        assert!(Config::parse("monitor = true").monitor);
        // 没写这个键时（老配置文件）也应保持开启
        assert!(Config::parse("buffer_ms = 20").monitor);
        let c = Config::parse("monitor = false");
        assert!(!Config::parse(&c.to_text()).monitor, "往返后应保持关闭");
    }

    #[test]
    fn looping_is_disabled_for_old_or_missing_configuration() {
        assert!(!Config::default().looping);
        for text in [
            "",
            "buffer_ms = 20\nsound_files = ['once.wav']",
            "hotkeys = F1, F2\nsound_files = ['once.wav']",
        ] {
            let config = Config::parse(text);
            assert!(!config.looping, "old config unexpectedly loops: {text}");
            let serialized = config.to_text();
            let document = serialized.parse::<toml::Table>().unwrap();
            assert_eq!(document["looping"].as_bool(), Some(false));
            assert!(!Config::try_parse(&serialized).unwrap().looping);
        }
    }

    #[test]
    fn looping_true_and_explicit_false_are_serialized_and_round_trip() {
        for enabled in [true, false] {
            let config =
                Config::try_parse(&format!("looping = {enabled}\nmonitor = false")).unwrap();
            assert_eq!(config.looping, enabled);
            assert!(config.unknown_lines.is_empty());
            let serialized = config.to_text();
            let document = serialized.parse::<toml::Table>().unwrap();
            assert_eq!(document["looping"].as_bool(), Some(enabled));
            assert_eq!(Config::try_parse(&serialized).unwrap(), config);
        }
    }

    #[test]
    fn looping_setter_rejects_invalid_values_without_changing_saved_state() {
        let mut config = Config::default();
        config.set("looping", " true ").unwrap();
        assert!(config.looping);
        for invalid in ["", "yes", "on", "1", "TRUE", "not-a-bool"] {
            assert!(config.set("looping", invalid).is_err());
            assert!(config.looping, "invalid value changed the preceding choice");
        }
        config.set("looping", "false").unwrap();
        assert!(!config.looping);
        let document = config.to_text().parse::<toml::Table>().unwrap();
        assert_eq!(document["looping"].as_bool(), Some(false));
        assert!(!Config::try_parse(&config.to_text()).unwrap().looping);
    }

    #[test]
    fn looping_updates_preserve_legacy_sounds_and_unknown_configuration() {
        let mut config = Config::parse(
            "looping = true\nhotkeys = F1, F2\nsound_files = ['a,b.wav', 'b.wav']\n\
             sound_gains = [-3, 2]\nsound_names = ['A', 'B']\nfuture_scalar = 'keep,me'\n",
        );
        assert!(
            config.looping,
            "legacy fallback must recognize the looping key"
        );
        assert_eq!(config.hotkeys, ["F1", "F2"]);
        let entries = config.sound_entries();
        config.set("looping", "false").unwrap();
        let text = config.to_text();
        let document = text.parse::<toml::Table>().unwrap();
        assert_eq!(document["looping"].as_bool(), Some(false));
        assert_eq!(document["future_scalar"].as_str(), Some("keep,me"));
        let round = Config::try_parse(&text).unwrap();
        assert!(!round.looping);
        assert_eq!(round.sound_entries(), entries);
        assert_eq!(round.to_text().parse::<toml::Table>().unwrap(), document);
    }

    #[test]
    fn looping_updates_preserve_structured_sound_ids_tags_and_unknown_tables() {
        let mut config = Config::try_parse(
            r#"
looping = false
future_scalar = "keep,me"
[[sounds]]
id = "stable-id"
path = 'C:\Users\Doe, John\sound.wav'
name = "Good"
gain_db = -9.0
tags = ["literal,tag", "another"]
hotkey = "F2"
[future.nested]
flag = true
[[future.rows]]
value = "first"
[[future.rows]]
value = "second"
"#,
        )
        .unwrap();
        let entries = config.sound_entries();
        for enabled in [true, false] {
            config
                .set("looping", if enabled { "true" } else { "false" })
                .unwrap();
            let serialized = config.to_text();
            let document = serialized.parse::<toml::Table>().unwrap();
            assert_eq!(document["looping"].as_bool(), Some(enabled));
            assert_eq!(document["future_scalar"].as_str(), Some("keep,me"));
            assert_eq!(document["future"]["nested"]["flag"].as_bool(), Some(true));
            assert_eq!(document["future"]["rows"].as_array().unwrap().len(), 2);
            assert!(document["sounds"][0].get("looping").is_none());
            let round = Config::try_parse(&serialized).unwrap();
            assert_eq!(round.looping, enabled);
            assert_eq!(round.sound_entries(), entries);
            assert_eq!(round.to_text().parse::<toml::Table>().unwrap(), document);
        }
    }

    #[test]
    fn 音效库条目应能从并行数组组合出来并写回() {
        let mut c = Config {
            sound_files: vec![PathBuf::from("/a/笑声.mp3"), PathBuf::from("/a/b.wav")],
            sound_gains: vec![-3.0],
            sound_names: vec!["大笑".into()],
            sound_tags: vec!["笑声|常用".into()],
            ..Default::default()
        };

        let items = c.sound_items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "大笑", "有自定义名就用它");
        assert_eq!(items[0].tags, vec!["笑声", "常用"]);
        assert_eq!(
            c.sound_tags,
            vec!["笑声|常用".to_string()],
            "多个标签必须写成一个列表项 —— 用逗号会让并行数组错位"
        );
        assert_eq!(items[0].gain_db, -3.0);
        assert_eq!(items[1].name, "b", "没自定义名就退回文件名");
        assert_eq!(items[1].gain_db, 0.0, "缺的音量补 0");
        assert!(items[1].tags.is_empty(), "缺的标签为空");

        // 写回后应完全一致
        c.set_sound_items(&items);
        let again = c.sound_items();
        assert_eq!(again, items, "往返后条目应保持一致");
    }

    #[test]
    fn 名称与标签应能往返序列化() {
        let c = Config {
            sound_files: vec![PathBuf::from("/a.mp3")],
            sound_names: vec!["带空格 的名字".into()],
            sound_tags: vec!["标签一|标签二".into()],
            ..Default::default()
        };
        let round = Config::parse(&c.to_text());
        assert_eq!(round.sound_names, c.sound_names);
        assert_eq!(round.sound_tags, c.sound_tags);
        assert_eq!(
            round.sound_files.len(),
            round.sound_tags.len(),
            "并行数组长度必须一致，否则标签会串到别的音效上"
        );
    }

    #[test]
    fn 配置目录别名应使用当前平台主目录() {
        // 只读真实环境，避免改 HOME 干扰同时运行的路径或配置测试。
        let expected = home_dir()
            .map(|home| home.join("Sounds"))
            .unwrap_or_else(|| PathBuf::from("~/Sounds"));
        let c = Config::parse(r#"sound_dirs = ["~/Sounds"]"#);
        assert_eq!(c.sound_dirs[0], expected);
        let c = Config::parse(r#"sound_files = ["~/Sounds"]"#);
        assert_eq!(c.sound_files[0], expected);
    }

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new(label: &str) -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("系统时钟早于 UNIX_EPOCH")
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "mixer-config-{label}-{}-{nanos}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn 保存后旧文件被完整替换() {
        let dir = TemporaryDirectory::new("replace");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "output_device = \"旧设备\"\nbuffer_ms = 10\n").unwrap();
        let config = Config {
            output_device: Some("新设备".into()),
            buffer_ms: Some(40),
            monitor: false,
            ..Default::default()
        };
        config.save_to(&path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("旧设备"), "旧值仍然存在：\n{saved}");
        let restored = Config::load_from(&path);
        assert_eq!(restored.output_device.as_deref(), Some("新设备"));
        assert_eq!(restored.buffer_ms, Some(40));
        assert!(!restored.monitor, "显式关闭的监听必须保存并恢复");
    }

    #[test]
    fn 保存失败保留旧文件() {
        let dir = TemporaryDirectory::new("failure");
        let target = dir.path().join("目标目录");
        std::fs::create_dir(&target).unwrap();
        let old_path = target.join("config.toml");
        const OLD: &str = "output_device = \"旧设备\"\n";
        std::fs::write(&old_path, OLD).unwrap();

        let config = Config {
            output_device: Some("新设备".into()),
            ..Default::default()
        };
        assert!(
            config.save_to(&target).is_err(),
            "目标路径是目录时保存必须失败"
        );
        assert_eq!(
            std::fs::read_to_string(&old_path).unwrap(),
            OLD,
            "保存失败不能改动旧文件"
        );
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            names,
            [std::ffi::OsString::from("目标目录")],
            "失败后残留临时文件"
        );
    }

    #[test]
    fn 未知键与音效条目在原子保存后仍保留() {
        let dir = TemporaryDirectory::new("unknown");
        let path = dir.path().join("config.toml");
        let text = r#"
future_scalar = "keep,me"
[[sounds]]
id = "stable-id"
path = 'C:\Users\Doe, John\sound.wav'
name = "Good"
gain_db = -9.0
tags = ["literal,tag", "another"]
hotkey = "F2"
"#;
        let original = Config::try_parse(text).unwrap();
        original.save_to(&path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        let document = saved.parse::<toml::Table>().unwrap();
        assert_eq!(document["future_scalar"].as_str(), Some("keep,me"));
        let restored = Config::try_parse(&saved).unwrap();
        assert!(
            restored
                .unknown_lines
                .iter()
                .any(|line| line.contains("future_scalar")),
            "未知键丢失：\n{saved}"
        );
        assert_eq!(restored.sound_entries(), original.sound_entries());
        assert_eq!(
            restored.sound_entries()[0].tags,
            ["literal,tag", "another"],
            "音效标签不能按标点拆分"
        );
    }

    #[test]
    fn 保存后不留下临时文件() {
        let dir = TemporaryDirectory::new("clean");
        let path = dir.path().join("config.toml");
        Config::default().save_to(&path).unwrap();
        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [std::ffi::OsString::from("config.toml")],
            "目录里有残留文件"
        );
    }

    #[test]
    fn 旧配置没有变声节时视为关闭() {
        let config = Config::try_parse("buffer_ms = 20\nmonitor = false\n").unwrap();
        assert!(!config.voice.enabled, "旧配置不能自动启用变声");
        assert_eq!(config.voice.active_preset, "builtin:female");
        assert!(config.voice.bypass_hotkey.is_empty());
        assert!(config.voice.next_preset_hotkey.is_empty());
        assert!(config.voice.prev_preset_hotkey.is_empty());
        assert!(config.voice_presets.is_empty());
        assert!(config.voice_warnings.is_empty());
    }

    #[test]
    fn 变声配置与用户预设完整往返() {
        let params = VoiceParamsConfig {
            pitch_semitones: 3.5,
            formant_semitones: -2.5,
            gate_enabled: true,
            gate_db: -48.0,
            eq_low_enabled: true,
            eq_low_db: -3.5,
            eq_mid_enabled: true,
            eq_mid_db: 1.5,
            eq_high_enabled: true,
            eq_high_db: 4.0,
            drive_enabled: true,
            drive: 0.4,
            robot_enabled: true,
            robot_hz: 123.0,
            robot_mix: 0.6,
        };
        let mut config = Config::default();
        config.voice.enabled = true;
        config.voice.active_preset = "user:mine-abc123".into();
        config.voice.bypass_hotkey = "Cmd+Shift+B".into();
        config.voice.next_preset_hotkey = "Cmd+Shift+Right".into();
        config.voice.prev_preset_hotkey = "Cmd+Shift+Left".into();
        config.voice_presets.push(VoicePreset {
            id: "user:mine-abc123".into(),
            name: "我的声音".into(),
            params,
            unknown_lines: Vec::new(),
        });

        let text = config.to_text();
        let round = Config::try_parse(&text).unwrap();
        assert_eq!(round, config, "变声配置与用户预设应无损往返");
        assert_eq!(
            round.find_voice_preset("user:mine-abc123"),
            Some(VoicePreset {
                id: "user:mine-abc123".into(),
                name: "我的声音".into(),
                params,
                unknown_lines: Vec::new(),
            })
        );

        let builtins = builtin_voice_presets();
        assert_eq!(
            builtins.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            [
                "builtin:female",
                "builtin:male",
                "builtin:child",
                "builtin:deep",
                "builtin:robot",
                "builtin:radio",
            ]
        );
        assert_eq!(
            builtins.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["女声", "男声", "小孩", "低沉", "机器人", "对讲机"]
        );
        let by_id = |id: &str| {
            builtins
                .iter()
                .find(|preset| preset.id == id)
                .unwrap()
                .params
        };
        let female = by_id("builtin:female");
        assert_eq!(
            (female.pitch_semitones, female.formant_semitones),
            (4.0, 3.0)
        );
        assert!(female.gate_enabled);
        assert_eq!(female.gate_db, -45.0);
        assert!(female.eq_high_enabled);
        assert_eq!(female.eq_high_db, 2.0);
        assert!(!female.eq_low_enabled && !female.eq_mid_enabled);
        assert!(!female.drive_enabled && !female.robot_enabled);
        let robot = by_id("builtin:robot");
        assert!(robot.drive_enabled);
        assert_eq!(robot.drive, 0.2);
        assert!(robot.robot_enabled);
        assert_eq!((robot.robot_hz, robot.robot_mix), (50.0, 0.8));
        let radio = by_id("builtin:radio");
        assert!(radio.gate_enabled);
        assert_eq!(radio.gate_db, -50.0);
        assert!(radio.eq_low_enabled && radio.eq_mid_enabled && radio.eq_high_enabled);
        assert_eq!(
            (radio.eq_low_db, radio.eq_mid_db, radio.eq_high_db),
            (-9.0, 4.0, -9.0)
        );
        assert!(radio.drive_enabled);
        assert_eq!(radio.drive, 0.35);
        assert!(!radio.robot_enabled);
    }

    #[test]
    fn 未知变声键与预设字段保留往返() {
        let text = r#"
[voice]
enabled = true
active_preset = "user:mine-abc123"
future_voice_key = "keep,me"

[[voice_presets]]
id = "user:mine-abc123"
name = "我的声音"
pitch_semitones = 2.0
future_preset_key = 7
"#;
        let config = Config::try_parse(text).unwrap();
        assert!(
            config
                .voice
                .unknown_lines
                .iter()
                .any(|line| line.contains("future_voice_key")),
            "未知变声键未保留：{:?}",
            config.voice.unknown_lines
        );
        assert!(
            config.voice_presets[0]
                .unknown_lines
                .iter()
                .any(|line| line.contains("future_preset_key")),
            "未知预设字段未保留：{:?}",
            config.voice_presets[0].unknown_lines
        );

        let serialized = config.to_text();
        let document = serialized.parse::<toml::Table>().unwrap();
        assert_eq!(
            document["voice"]["future_voice_key"].as_str(),
            Some("keep,me")
        );
        assert_eq!(
            document["voice_presets"][0]["future_preset_key"].as_integer(),
            Some(7)
        );
        let round = Config::try_parse(&serialized).unwrap();
        assert_eq!(round.voice.unknown_lines, config.voice.unknown_lines);
        assert_eq!(
            round.voice_presets[0].unknown_lines,
            config.voice_presets[0].unknown_lines
        );
        assert_eq!(
            round.to_text().parse::<toml::Table>().unwrap(),
            document,
            "未知键在第二次往返后不应漂移"
        );
    }

    #[test]
    fn 激活预设缺失时回退女声并关闭() {
        let config =
            Config::try_parse("[voice]\nenabled = true\nactive_preset = \"user:missing\"\n")
                .unwrap();
        assert_eq!(config.voice.active_preset, "builtin:female");
        assert!(!config.voice.enabled, "缺失预设时必须关闭变声");
        assert!(
            config
                .voice_warnings
                .iter()
                .any(|warning| warning.contains("user:missing")),
            "缺失预设应产生警告：{:?}",
            config.voice_warnings
        );
        assert_eq!(
            config.voice.effective_active_preset(&config.voice_presets),
            ("builtin:female".into(), false)
        );
    }

    #[test]
    fn 重复预设_ID_与空名称被拒绝() {
        let text = r#"
[[voice_presets]]
id = "user:first"
name = "第一个"
pitch_semitones = 1.0

[[voice_presets]]
id = "user:first"
name = "重复标识"

[[voice_presets]]
id = "user:blank"
name = "   "

[[voice_presets]]
id = "builtin:female"
name = "非法前缀"

[[voice_presets]]
id = "user:last"
name = "最后一个"
pitch_semitones = 9.0
"#;
        let config = Config::try_parse(text).unwrap();
        let ids: Vec<_> = config
            .voice_presets
            .iter()
            .map(|preset| preset.id.as_str())
            .collect();
        assert_eq!(ids, ["user:first", "user:last"]);
        assert!(
            config.voice_warnings.len() >= 3,
            "被丢弃的条目都应记录警告：{:?}",
            config.voice_warnings
        );
        assert!(config.find_voice_preset("user:first").is_some());
        assert!(config.find_voice_preset("user:last").is_some());
        assert!(config.find_voice_preset("user:blank").is_none());
    }

    #[test]
    fn 越界参数在加载时钳制并警告() {
        let text = r#"
[voice]
active_preset = "user:hot"

[[voice_presets]]
id = "user:hot"
name = "越界参数"
pitch_semitones = 30.0
formant_semitones = -20.0
gate_enabled = true
gate_db = 10.0
eq_low_enabled = true
eq_low_db = 20.0
eq_mid_enabled = true
eq_mid_db = -20.0
eq_high_enabled = true
eq_high_db = 13.0
drive_enabled = true
drive = 2.0
robot_enabled = true
robot_hz = 2000.0
robot_mix = -1.0
"#;
        let config = Config::try_parse(text).unwrap();
        let params = config.voice_presets[0].params;
        assert_eq!(params.pitch_semitones, 24.0);
        assert_eq!(params.formant_semitones, -12.0);
        assert_eq!(params.gate_db, 0.0);
        assert_eq!(params.eq_low_db, 12.0);
        assert_eq!(params.eq_mid_db, -12.0);
        assert_eq!(params.eq_high_db, 12.0);
        assert_eq!(params.drive, 1.0);
        assert_eq!(params.robot_hz, 1000.0);
        assert_eq!(params.robot_mix, 0.0);
        assert!(
            config
                .voice_warnings
                .iter()
                .any(|warning| warning.contains("越界参数")),
            "钳制应产生警告：{:?}",
            config.voice_warnings
        );

        let round = Config::try_parse(&config.to_text()).unwrap();
        assert_eq!(round.voice_presets, config.voice_presets);
        assert!(
            round.voice_warnings.is_empty(),
            "已钳制的值再次加载不应重复警告：{:?}",
            round.voice_warnings
        );
    }

    #[test]
    fn 用户预设_ID_稳定且不重复() {
        let config = Config::default();
        let first = config.new_user_preset_id("My Voice");
        let second = config.new_user_preset_id("My Voice");
        assert_ne!(first, second, "同名两次创建必须得到不同 ID");
        assert!(first.starts_with("user:"), "用户预设 ID 前缀错误：{first}");
        for id in [&first, &second] {
            let suffix = id.rsplit_once('-').expect("ID 应包含名称片段与后缀").1;
            assert_eq!(suffix.len(), 6, "随机后缀应为 6 位十六进制：{id}");
            assert!(
                suffix.chars().all(|ch| ch.is_ascii_hexdigit()),
                "随机后缀应为十六进制：{id}"
            );
        }
        assert!(config.new_user_preset_id("中文名称").starts_with("user:"));

        let mut preset = VoicePreset {
            id: first.clone(),
            name: "My Voice".into(),
            params: VoiceParamsConfig::neutral(),
            unknown_lines: Vec::new(),
        };
        preset.name = "重命名后".into();
        assert_eq!(preset.id, first, "重命名不能改变已有预设 ID");
    }

    #[test]
    fn 内置预设只读且不在配置中重复写入() {
        let mut config = Config {
            voice_presets: builtin_voice_presets(),
            ..Default::default()
        };
        config.voice_presets.push(VoicePreset {
            id: "user:mine-abc123".into(),
            name: "我的声音".into(),
            params: VoiceParamsConfig {
                pitch_semitones: 1.0,
                ..VoiceParamsConfig::neutral()
            },
            unknown_lines: Vec::new(),
        });

        let document = config.to_text().parse::<toml::Table>().unwrap();
        let saved = document["voice_presets"].as_array().unwrap();
        assert_eq!(saved.len(), 1, "只有用户预设应写入配置：{saved:?}");
        assert_eq!(saved[0]["id"].as_str(), Some("user:mine-abc123"));
        assert!(
            saved.iter().all(|row| !row["id"]
                .as_str()
                .unwrap_or_default()
                .starts_with("builtin:")),
            "内置预设不能写入配置：{saved:?}"
        );
    }

    #[test]
    fn 变声配置点路径写入与读取() {
        let mut config = Config::default();
        config.set("voice.enabled", "true").unwrap();
        assert!(config.voice.enabled);
        config.set("voice.enabled", "false").unwrap();
        assert!(!config.voice.enabled);
        assert!(config.set("voice.enabled", "yes").is_err());

        config.set("voice.active_preset", "builtin:male").unwrap();
        assert_eq!(config.voice.active_preset, "builtin:male");
        assert!(
            config.set("voice.active_preset", "user:missing").is_err(),
            "激活预设必须存在"
        );
        assert_eq!(config.voice.active_preset, "builtin:male");

        config.set("voice.bypass_hotkey", "Cmd+Shift+B").unwrap();
        config
            .set("voice.next_preset_hotkey", "Cmd+Shift+Right")
            .unwrap();
        config
            .set("voice.prev_preset_hotkey", "Cmd+Shift+Left")
            .unwrap();
        assert_eq!(config.voice.bypass_hotkey, "Cmd+Shift+B");
        assert_eq!(config.voice.next_preset_hotkey, "Cmd+Shift+Right");
        assert_eq!(config.voice.prev_preset_hotkey, "Cmd+Shift+Left");

        assert!(config.set("voice.unknown", "1").is_err());
        assert!(config.set("voice.presets.name", "1").is_err());

        let round = Config::try_parse(&config.to_text()).unwrap();
        assert_eq!(round.voice, config.voice);
    }

    #[test]
    fn 变声配置原子保存失败保留旧文件() {
        let dir = TemporaryDirectory::new("voice-failure");
        let target = dir.path().join("目标目录");
        std::fs::create_dir(&target).unwrap();
        let old_path = target.join("config.toml");
        const OLD: &str = "[voice]\nenabled = false\nactive_preset = \"builtin:female\"\n";
        std::fs::write(&old_path, OLD).unwrap();

        let mut config = Config::default();
        config.set("voice.enabled", "true").unwrap();
        config.set("voice.active_preset", "builtin:male").unwrap();
        assert!(
            config.save_to(&target).is_err(),
            "目标路径是目录时保存必须失败"
        );
        assert_eq!(
            std::fs::read_to_string(&old_path).unwrap(),
            OLD,
            "保存失败不能改动旧文件"
        );
    }

    // ── 写入前的严格读取

    /// 音效库、变声节与用户预设俱全，只有 Windows 路径里的一处非法反斜杠转义。
    const INVALID_TOML_WITH_LIBRARY_AND_VOICE: &str = concat!(
        "[[sounds]]\n",
        "id = \"s1\"\n",
        "path = \"C:\\Users\\Doe\\a.wav\"\n",
        "name = \"笑声\"\n",
        "\n",
        "[voice]\n",
        "enabled = true\n",
        "active_preset = \"user:mine-abc123\"\n",
        "\n",
        "[[voice_presets]]\n",
        "id = \"user:mine-abc123\"\n",
        "name = \"我的声音\"\n",
        "pitch_semitones = 3.0\n",
    );

    fn write_config(dir: &TemporaryDirectory, text: &str) -> PathBuf {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn 写入前读取遇到无效_TOML_拒绝且保留原文件() {
        let dir = TemporaryDirectory::new("strict-invalid");
        let path = write_config(&dir, INVALID_TOML_WITH_LIBRARY_AND_VOICE);

        let error = Config::load_for_write_from(&path).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("不是有效 TOML"), "{message}");
        assert!(message.contains("第 3 行"), "应指出出错行：{message}");
        assert!(message.contains("拒绝写入"), "{message}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            INVALID_TOML_WITH_LIBRARY_AND_VOICE,
            "拒绝写入时磁盘上的文件必须保持原样"
        );
    }

    #[test]
    fn 写入前读取文件不存在时使用默认配置() {
        let dir = TemporaryDirectory::new("strict-missing");
        let path = dir.path().join("尚未创建").join("config.toml");

        let config = Config::load_for_write_from(&path).unwrap();
        assert_eq!(config, Config::default());
        assert!(!path.exists(), "读取不应创建文件");
    }

    #[test]
    fn 写入前读取无法读取时报错() {
        let dir = TemporaryDirectory::new("strict-unreadable");
        // 目录无法按文件读取：这类错误不能被当成「没有配置」。
        let error = Config::load_for_write_from(dir.path()).unwrap_err();
        assert!(
            format!("{error:#}").contains("读取配置文件失败"),
            "{error:#}"
        );

        let bytes = vec![0xff, 0xfe, 0x00, b'['];
        let path = dir.path().join("config.toml");
        std::fs::write(&path, &bytes).unwrap();
        let error = Config::load_for_write_from(&path).unwrap_err();
        assert!(
            format!("{error:#}").contains("读取配置文件失败"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn 写入前读取遇到被拒绝的变声条目拒绝写入() {
        let cases = [
            (
                "缺少 user: 前缀",
                "[[voice_presets]]\nid = \"mine\"\nname = \"甲\"\n",
                "缺少 user: 前缀",
            ),
            (
                "名称为空",
                "[[voice_presets]]\nid = \"user:a\"\nname = \"  \"\n",
                "名称为空",
            ),
            (
                "重复 ID",
                "[[voice_presets]]\nid = \"user:a\"\nname = \"甲\"\n\
                 [[voice_presets]]\nid = \"user:a\"\nname = \"乙\"\n",
                "重复",
            ),
            ("条目不是表格", "voice_presets = [1]\n", "不是表格"),
            ("预设列表不是数组", "voice_presets = \"x\"\n", "不是数组"),
            ("voice 节不是表格", "voice = 3\n", "[voice]"),
            (
                "启用值类型错误",
                "[voice]\nenabled = \"yes\"\n",
                "voice.enabled",
            ),
            (
                "热键类型错误",
                "[voice]\nbypass_hotkey = 7\n",
                "voice.bypass_hotkey",
            ),
            (
                "预设数值字段类型错误",
                "[[voice_presets]]\nid = \"user:a\"\nname = \"甲\"\npitch_semitones = [1]\n",
                "pitch_semitones",
            ),
            (
                "预设开关字段类型错误",
                "[[voice_presets]]\nid = \"user:a\"\nname = \"甲\"\ngate_enabled = \"on\"\n",
                "gate_enabled",
            ),
        ];
        for (label, voice_text, reason) in cases {
            let dir = TemporaryDirectory::new("strict-rejected");
            // 顶层键必须出现在任何表头之前，否则会被归入前一个表。
            let text = format!("buffer_ms = 40\n{voice_text}\n[[sounds]]\npath = \"/a.wav\"\n");
            let path = write_config(&dir, &text);

            let error = Config::load_for_write_from(&path).unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("拒绝写入"), "{label}：{message}");
            assert!(message.contains(reason), "{label} 应说明原因：{message}");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                text,
                "{label}：拒绝写入时不得改动文件"
            );

            let report = Config::load_report_from(&path);
            assert!(report.write_blocked, "{label}");
            assert!(
                report.notices.iter().any(|notice| notice.contains(reason)),
                "{label} 的提示应包含原因：{:?}",
                report.notices
            );
            assert_eq!(
                report.config.sound_entries().len(),
                1,
                "{label}：读取仍应保留其余配置"
            );
        }
    }

    #[test]
    fn 写入前读取接受仅钳制或回退的警告() {
        let dir = TemporaryDirectory::new("strict-normalized");
        let path = write_config(
            &dir,
            "[voice]\nenabled = true\nactive_preset = \"user:gone\"\n\n\
             [[voice_presets]]\nid = \"user:hot\"\nname = \"越界\"\npitch_semitones = 99.0\n",
        );

        let config = Config::load_for_write_from(&path).unwrap();
        assert!(config.voice_rejected.is_empty());
        assert!(
            config.voice_warnings.len() >= 2,
            "钳制与回退仍应留下警告：{:?}",
            config.voice_warnings
        );
        assert_eq!(config.voice_presets[0].params.pitch_semitones, 24.0);
        assert_eq!(config.voice.active_preset, "builtin:female");
        assert!(!config.voice.enabled);

        let report = Config::load_report_from(&path);
        assert!(!report.write_blocked, "规范化数值不应阻止保存");
        assert_eq!(report.notices, config.voice_warnings);
    }

    #[test]
    fn 写入前读取成功后保存保留音效库用户预设与未知键() {
        let dir = TemporaryDirectory::new("strict-roundtrip");
        let path = write_config(
            &dir,
            r#"
future_scalar = "keep,me"

[[sounds]]
id = "s1"
path = "C:\\Users\\Doe\\a.wav"
name = "笑声"
hotkey = "F1"

[voice]
enabled = true
active_preset = "user:mine-abc123"

[[voice_presets]]
id = "user:mine-abc123"
name = "我的声音"
pitch_semitones = 3.0
"#,
        );

        let mut config = Config::load_for_write_from(&path).unwrap();
        config.voice.enabled = false;
        config.save_to(&path).unwrap();

        let saved = Config::load_for_write_from(&path).unwrap();
        assert!(!saved.voice.enabled);
        assert_eq!(saved.sound_entries(), config.sound_entries());
        assert_eq!(saved.voice_presets, config.voice_presets);
        assert_eq!(saved.voice_presets[0].params.pitch_semitones, 3.0);
        assert!(saved
            .unknown_lines
            .iter()
            .any(|line| line.contains("future_scalar")));
    }

    #[test]
    fn 加载报告在无效_TOML_时提示并阻止写入() {
        let dir = TemporaryDirectory::new("report-invalid");
        let text = format!("buffer_ms = 40\n{INVALID_TOML_WITH_LIBRARY_AND_VOICE}");
        let path = write_config(&dir, &text);

        let report = Config::load_report_from(&path);
        assert!(report.write_blocked);
        assert_eq!(report.notices.len(), 1, "{:?}", report.notices);
        assert!(report.notices[0].contains("不是有效 TOML"));
        assert!(report.notices[0].contains(&path.display().to_string()));
        assert_eq!(
            report.config.buffer_ms,
            Some(40),
            "宽松读取仍应保留能识别的设置"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn 加载报告在读取失败时用默认值并阻止写入() {
        let dir = TemporaryDirectory::new("report-unreadable");
        let report = Config::load_report_from(dir.path());
        assert!(report.write_blocked);
        assert_eq!(report.config, Config::default());
        assert!(report.notices[0].contains("读取配置文件失败"));
    }

    #[test]
    fn 加载报告在配置正常或缺失时没有提示() {
        let dir = TemporaryDirectory::new("report-clean");
        let missing = Config::load_report_from(&dir.path().join("config.toml"));
        assert!(missing.notices.is_empty() && !missing.write_blocked);

        let path = write_config(&dir, "buffer_ms = 40\n[voice]\nenabled = false\n");
        let report = Config::load_report_from(&path);
        assert!(report.notices.is_empty() && !report.write_blocked);
        assert_eq!(report.config.buffer_ms, Some(40));
    }

    #[test]
    fn 被拒绝的变声条目进入警告列表且被拒绝列表是其子集() {
        let config = Config::try_parse(
            "[[voice_presets]]\nid = \"user:a\"\nname = \"\"\n\n\
             [[voice_presets]]\nid = \"user:b\"\nname = \"越界\"\npitch_semitones = 99.0\n",
        )
        .unwrap();
        assert_eq!(
            config.voice_rejected.len(),
            1,
            "{:?}",
            config.voice_rejected
        );
        assert!(config
            .voice_rejected
            .iter()
            .all(|item| config.voice_warnings.contains(item)));
        assert_eq!(
            config.voice_warnings.len(),
            2,
            "被拒绝与被钳制的条目都应有提示"
        );
    }
}

// ─────────────────────────────────────────── 目录扫描

const AUDIO_EXT: &[&str] = &[
    "mp3", "wav", "ogg", "m4a", "flac", "aac", "aiff", "caf", "opus",
];

/// 扫描一个目录里的音频文件，按文件名排序。
pub fn scan_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        anyhow::bail!("不是文件夹: {}", dir.display());
    }
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("打不开文件夹: {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| AUDIO_EXT.contains(&e.to_lowercase().as_str()))
                    .unwrap_or(false)
        })
        .collect();
    out.sort();
    Ok(out)
}

/// 扫描多个目录并合并（排序去重）。无法读取的目录会被跳过而非中断。
pub fn scan_dirs(dirs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for d in dirs {
        match scan_dir(d) {
            Ok(mut v) => out.append(&mut v),
            Err(e) => eprintln!("  ⚠ 跳过目录 {}: {e}", d.display()),
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod scan_tests {
    use super::*;

    #[test]
    fn 非下载目录的中文空格逗号路径可保存恢复后扫描() {
        struct TemporaryDirectory(PathBuf);
        impl Drop for TemporaryDirectory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory = TemporaryDirectory(std::env::temp_dir().join(format!(
            "mixer-custom-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
        let source = directory.0.join("音频 目录,素材");
        std::fs::create_dir_all(source.join("子文件夹")).unwrap();
        let sound = source.join("声音, 一.WAV");
        std::fs::write(&sound, b"scan fixture").unwrap();
        std::fs::write(source.join("ignored.txt"), b"not audio").unwrap();
        std::fs::write(source.join("子文件夹/nested.wav"), b"nested").unwrap();

        let config = Config {
            sound_dirs: vec![source.clone()],
            ..Default::default()
        };
        let restored = Config::parse(&config.to_text());
        assert_eq!(restored.sound_dirs, vec![source]);
        assert_eq!(restored.collect_sounds().unwrap(), vec![sound]);
    }

    #[test]
    fn 只挑出音频文件并按名排序() {
        let dir = std::env::temp_dir().join(format!("mixer_scan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["b.wav", "a.mp3", "note.txt", "c.WAV", "d.flac"] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        let got = scan_dir(&dir).unwrap();
        let names: Vec<String> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["a.mp3", "b.wav", "c.WAV", "d.flac"],
            "应只保留音频、忽略大小写、按名排序"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 不存在的目录应报错而不是静默返回空() {
        assert!(scan_dir(Path::new("/nonexistent/xyz")).is_err());
    }

    #[test]
    fn 未知键在保存后仍然存在() {
        // 这是个用户可以直接编辑的文本文件。早先保存时只写自己认识的键，
        // 结果手加的内容和未来版本的键会被**静默抹掉** —— 丢数据比不认识一个键糟糕得多。
        let text = r#"
output_device = "豆包"
buffer_ms = 20
some_future_key = 123
手写的键 = "别弄丢我"
"#;
        let c = Config::parse(text);
        assert_eq!(c.unknown_lines.len(), 2, "两个未知键都该被记下来");

        let out = c.to_text();
        assert!(out.contains("some_future_key = 123"), "未知键丢了：\n{out}");
        assert!(
            out.contains("手写的键 = \"别弄丢我\""),
            "中文键丢了：\n{out}"
        );

        // 再解析一遍，未知键不能变多（否则每存一次就长一截）
        let again = Config::parse(&out);
        assert_eq!(again.unknown_lines.len(), 2, "往返之后未知键变多了");
    }

    #[test]
    fn 多目录合并会去重() {
        let d = std::env::temp_dir().join(format!("mixer_scan2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("x.wav"), b"x").unwrap();
        let got = scan_dirs(&[d.clone(), d.clone(), PathBuf::from("/nonexistent")]).unwrap();
        assert_eq!(got.len(), 1, "重复目录应去重，坏目录应跳过");
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod hotkey_identity_regressions {
    use super::*;
    #[test]
    fn setter_and_structured_roundtrip_preserve_empty_hotkey_positions() {
        let mut config = Config::default();
        config.set_sound_entries(&[
            SoundEntry {
                id: "first".into(),
                path: PathBuf::from("first.wav"),
                name: "first".into(),
                gain_db: 0.0,
                tags: vec![],
                hotkey: "".into(),
            },
            SoundEntry {
                id: "second".into(),
                path: PathBuf::from("second.wav"),
                name: "second".into(),
                gain_db: 0.0,
                tags: vec![],
                hotkey: "".into(),
            },
        ]);
        config.set("hotkeys", ",F2").unwrap();
        assert_eq!(config.hotkeys, ["", "F2"]);
        let round = Config::parse(&config.to_text());
        assert_eq!(round.sound_entries()[0].hotkey, "");
        assert_eq!(round.sound_entries()[1].hotkey, "F2");
        config.set("hotkeys", "").unwrap();
        assert!(Config::parse(&config.to_text())
            .sound_entries()
            .iter()
            .all(|e| e.hotkey.is_empty()));
    }
}
