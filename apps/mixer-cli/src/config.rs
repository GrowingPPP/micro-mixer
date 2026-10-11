//! 配置持久化 —— `~/.config/mixer/config.toml`
//!
//! 目的：让日常使用只需要一条命令。设备名、音效目录、增益这些不该每次重敲。
//!
//! 故意不引入 serde/toml：这里只需要一个扁平的 key = value 文件，
//! 手写解析器约 60 行、零依赖、构建快，也更容易给出友好的报错。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    /// 真实麦克风（省略则用系统默认）
    pub input_device: Option<String>,
    /// 虚拟声卡输出设备（省略则自动探测）
    pub output_device: Option<String>,
    pub buffer_ms: Option<u32>,
    pub mic_gain_db: Option<f32>,
    pub sfx_gain_db: Option<f32>,
    /// 音效目录列表
    pub sound_dirs: Vec<PathBuf>,
    /// 热键列表（与音效槽位一一对应）
    pub hotkeys: Vec<String>,
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
                 mic_gain_db / sfx_gain_db / sound_dirs / hotkeys"
            ),
        }
        Ok(())
    }

    /// 合并音效目录里所有音频文件，按文件名排序。
    pub fn collect_sounds(&self) -> Result<Vec<PathBuf>> {
        crate::collect_audio_files_from(&self.sound_dirs)
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
    fn 波浪号应展开为家目录() {
        std::env::set_var("HOME", "/tmp/fakehome");
        let c = Config::parse(r#"sound_dirs = ["~/Sounds"]"#);
        assert_eq!(c.sound_dirs[0], PathBuf::from("/tmp/fakehome/Sounds"));
    }
}
