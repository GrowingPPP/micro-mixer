//! 应用配置 —— `~/.config/mixer/config.toml`
//!
//! 目的：让日常使用只需要一条命令。设备名、音效目录、增益这些不该每次重敲。
//!
//! 故意不引入 serde/toml：这里只需要一个扁平的 key = value 文件，
//! 手写解析器约 60 行、零依赖、构建快，也更容易给出友好的报错。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

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
    /// 每个音效的音量（dB），与 sound_files 一一对应
    pub sound_gains: Vec<f32>,
    /// 每个音效的**显示名**（与 sound_files 一一对应；空串表示用文件名）
    pub sound_names: Vec<String>,
    /// 每个音效的**标签**（与 sound_files 一一对应；一条音的多个标签用逗号分隔）
    pub sound_tags: Vec<String>,
    /// 监听：触发的音效同时放给自己听（默认开）
    pub monitor: bool,
    /// 监听输出到哪个设备。空 = 系统默认输出。
    /// 用途：默认输出可能是虚拟声卡或别的东西，用户希望监听走耳机。
    pub monitor_device: Option<String>,
    /// 热键列表（与音效槽位一一对应）
    pub hotkeys: Vec<String>,
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
            sound_gains: Vec::new(),
            sound_names: Vec::new(),
            sound_tags: Vec::new(),
            // 默认**开启**监听：没有它，用户在游戏里按键后自己听不到任何声音，
            // 只能盲猜队友有没有收到 —— 这是实打实的可用性问题。
            monitor: true,
            monitor_device: None,
            hotkeys: Vec::new(),
        }
    }
}

impl Config {
    /// 配置文件路径。遵循 `XDG_CONFIG_HOME`。
    pub fn path() -> PathBuf {
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("mixer").join("config.toml")
    }

    /// 读取配置。文件不存在时返回默认值（不是错误）。
    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("创建配置目录失败: {}", dir.display()))?;
        }
        std::fs::write(&path, self.to_text())
            .with_context(|| format!("写入配置失败: {}", path.display()))?;
        Ok(path)
    }

    // ── 解析

    pub fn parse(text: &str) -> Self {
        let mut c = Config::default();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let key = k.trim();
            let val = v.trim();
            match key {
                "input_device" => c.input_device = parse_str(val),
                "output_device" => c.output_device = parse_str(val),
                "buffer_ms" => c.buffer_ms = val.parse().ok(),
                "mic_gain_db" => c.mic_gain_db = val.parse().ok(),
                "sfx_gain_db" => c.sfx_gain_db = val.parse().ok(),
                "sound_dirs" => c.sound_dirs = parse_list(val).into_iter().map(expand).collect(),
                "sound_names" => c.sound_names = parse_list(val),
                "sound_tags" => c.sound_tags = parse_list(val),
                "sound_files" => {
                    c.sound_files = parse_list(val).into_iter().map(expand).collect()
                }
                "monitor" => c.monitor = parse_bool(val),
                "monitor_device" => c.monitor_device = parse_str(val),
            "sound_gains" => {
                    c.sound_gains = parse_list(val)
                        .iter()
                        .map(|x| x.parse::<f32>().unwrap_or(0.0))
                        .collect()
                }
                "hotkeys" => c.hotkeys = parse_list(val),
                _ => {} // 未知键忽略，保持向前兼容
            }
        }
        c
    }

    pub fn to_text(&self) -> String {
        let mut s = String::from("# mixer 配置 —— 由 `mixer config set` 生成\n\n");
        if let Some(v) = &self.input_device {
            s += &format!("input_device = \"{v}\"\n");
        }
        if let Some(v) = &self.output_device {
            s += &format!("output_device = \"{v}\"\n");
        }
        if let Some(v) = self.buffer_ms {
            s += &format!("buffer_ms = {v}\n");
        }
        if let Some(v) = self.mic_gain_db {
            s += &format!("mic_gain_db = {v}\n");
        }
        if let Some(v) = self.sfx_gain_db {
            s += &format!("sfx_gain_db = {v}\n");
        }
        if !self.sound_dirs.is_empty() {
            let items: Vec<String> = self
                .sound_dirs
                .iter()
                .map(|p| format!("\"{}\"", p.display()))
                .collect();
            s += &format!("sound_dirs = [{}]\n", items.join(", "));
        }
        s += &format!("monitor = {}\n", self.monitor);
        if let Some(d) = &self.monitor_device {
            s += &format!("monitor_device = \"{d}\"\n");
        }
        if !self.sound_files.is_empty() {
            let items: Vec<String> = self
                .sound_files
                .iter()
                .map(|p| format!("\"{}\"", p.display()))
                .collect();
            s += &format!("sound_files = [{}]\n", items.join(", "));
        }
        if !self.sound_gains.is_empty() {
            let items: Vec<String> = self.sound_gains.iter().map(|g| format!("\"{g}\"")).collect();
            s += &format!("sound_gains = [{}]\n", items.join(", "));
        }
        if !self.sound_names.is_empty() {
            let items: Vec<String> = self
                .sound_names
                .iter()
                .map(|n| format!("\"{}\"", n.replace('"', "")))
                .collect();
            s += &format!("sound_names = [{}]\n", items.join(", "));
        }
        if !self.sound_tags.is_empty() {
            let items: Vec<String> = self
                .sound_tags
                .iter()
                .map(|t| format!("\"{}\"", t.replace('"', "")))
                .collect();
            s += &format!("sound_tags = [{}]\n", items.join(", "));
        }
        if !self.hotkeys.is_empty() {
            let items: Vec<String> = self.hotkeys.iter().map(|h| format!("\"{h}\"")).collect();
            s += &format!("hotkeys = [{}]\n", items.join(", "));
        }
        s
    }

    /// 用 `key value` 设置一项。返回是否成功。
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let v = value.trim();
        match key {
            "input_device" => {
                self.input_device = if v.is_empty() || v == "none" {
                    None
                } else {
                    Some(v.to_string())
                }
            }
            "output_device" => {
                self.output_device = if v.is_empty() || v == "none" {
                    None
                } else {
                    Some(v.to_string())
                }
            }
            "buffer_ms" => self.buffer_ms = Some(v.parse().context("buffer_ms 需要是整数")?),
            "mic_gain_db" => self.mic_gain_db = Some(v.parse().context("mic_gain_db 需要是数字")?),
            "sfx_gain_db" => self.sfx_gain_db = Some(v.parse().context("sfx_gain_db 需要是数字")?),
            "sound_dirs" => {
                self.sound_dirs = v
                    .split(',')
                    .map(|x| x.trim())
                    .filter(|x| !x.is_empty())
                    .map(|x| expand(x.to_string()))
                    .collect()
            }
            "sound_files" => {
                self.sound_files = v
                    .split(',')
                    .map(|x| x.trim())
                    .filter(|x| !x.is_empty())
                    .map(|x| expand(x.to_string()))
                    .collect()
            }
            "monitor" => self.monitor = parse_bool(v),
            "monitor_device" => self.monitor_device = parse_str(v),
            "sound_names" => {
                self.sound_names = v
                    .split('\n')
                    .map(|x| x.trim().to_string())
                    .collect()
            }
            "sound_tags" => {
                self.sound_tags = v
                    .split('\n')
                    .map(|x| x.trim().to_string())
                    .collect()
            }
            "sound_gains" => {
                self.sound_gains = v
                    .split(',')
                    .map(|x| x.trim())
                    .filter(|x| !x.is_empty())
                    .filter_map(|x| x.parse::<f32>().ok())
                    .collect()
            }
            "hotkeys" => {
                self.hotkeys = v
                    .split(',')
                    .map(|x| x.trim())
                    .filter(|x| !x.is_empty())
                    .map(|x| x.to_string())
                    .collect()
            }
            other => anyhow::bail!(
                "未知配置项「{other}」。可用：input_device / output_device / buffer_ms / \
                 mic_gain_db / sfx_gain_db / sound_dirs / sound_files / sound_gains / \
             monitor / hotkeys"
            ),
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
                    .map(|t| {
                        // 存的时候用 `|` 分隔（见 set_sound_items），但手写配置时
                        // 用户更可能打逗号 —— 两种都认。
                        t.split(['|', ',', ';'])
                            .map(|x| x.trim().to_string())
                            .filter(|x| !x.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect()
    }

    /// 用一组音效条目覆盖配置里的并行数组。
    pub fn set_sound_items(&mut self, items: &[SoundItem]) {
        self.sound_files = items.iter().map(|i| i.path.clone()).collect();
        self.sound_gains = items.iter().map(|i| i.gain_db).collect();
        self.sound_names = items.iter().map(|i| i.name.clone()).collect();
        // ⚠️ 必须用 `|` 而不是逗号：配置里的列表本身就是逗号分隔的
        //    （`parse_list` 按 `,` 切），标签里再放逗号会把**整个并行数组错位** ——
        //    一个带 2 个标签的音效，第二个标签会被当成下一个音效的标签。
        self.sound_tags = items
            .iter()
            .map(|i| {
                i.tags
                    .iter()
                    .map(|t| t.replace(['|', ',', ';'], " "))
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect();
    }

    /// 补齐音量表长度（新增音效默认 0 dB）。
    pub fn normalized_gains(&self, count: usize) -> Vec<f32> {
        let mut g = self.sound_gains.clone();
        g.resize(count, 0.0);
        g
    }
}

fn parse_str(v: &str) -> Option<String> {
    let t = v.trim().trim_matches('"').trim_matches('\'');
    if t.is_empty() || t == "none" {
        None
    } else {
        Some(t.to_string())
    }
}

/// 解析 `["a", "b"]` 或 `a, b` 两种写法
/// 解析布尔值。缺失或无法识别时按 `true`（`monitor` 键的缺省即开启）。
fn parse_bool(v: &str) -> bool {
    match v.trim().to_ascii_lowercase().as_str() {
        "false" | "0" | "no" | "off" => false,
        _ => true,
    }
}

fn parse_list(v: &str) -> Vec<String> {
    let inner = v.trim().trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .map(|x| x.trim().trim_matches('"').trim_matches('\''))
        .filter(|x| !x.is_empty())
        .map(|x| x.to_string())
        .collect()
}

/// 展开开头的 `~`
fn expand(p: String) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn 未知键与畸形行应被忽略而不是报错() {
        let c = Config::parse("future_key = 1\n这是一行垃圾\nbuffer_ms = 25\n");
        assert_eq!(c.buffer_ms, Some(25));
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
            sound_gains: vec![0.0, -6.0],
            sound_names: Vec::new(),
            sound_tags: Vec::new(),
            monitor: false,
            monitor_device: None,
            hotkeys: vec!["F1".into(), "F2".into()],
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
        let mut c = Config::default();
        c.sound_dirs = vec![PathBuf::from("/nonexistent_dir_xyz")];
        c.sound_files = vec![PathBuf::from("/tmp/whatever.mp3")];
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
    fn 音效库条目应能从并行数组组合出来并写回() {
        let mut c = Config::default();
        c.sound_files = vec![
            PathBuf::from("/a/笑声.mp3"),
            PathBuf::from("/a/b.wav"),
        ];
        c.sound_gains = vec![-3.0];
        c.sound_names = vec!["大笑".into()];
        c.sound_tags = vec!["笑声|常用".into()];

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
        let mut c = Config::default();
        c.sound_files = vec![PathBuf::from("/a.mp3")];
        c.sound_names = vec!["带空格 的名字".into()];
        c.sound_tags = vec!["标签一|标签二".into()];
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
    fn 波浪号应展开为家目录() {
        std::env::set_var("HOME", "/tmp/fakehome");
        let c = Config::parse(r#"sound_dirs = ["~/Sounds"]"#);
        assert_eq!(c.sound_dirs[0], PathBuf::from("/tmp/fakehome/Sounds"));
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
        assert_eq!(names, vec!["a.mp3", "b.wav", "c.WAV", "d.flac"],
                   "应只保留音频、忽略大小写、按名排序");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 不存在的目录应报错而不是静默返回空() {
        assert!(scan_dir(Path::new("/nonexistent/xyz")).is_err());
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
