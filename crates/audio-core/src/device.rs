//! 设备抽象层 —— 平台无关。
//!
//! macOS（CoreAudio）与 Windows（WASAPI）各自实现 [`AudioBackend`]，
//! 上层的混音引擎、UI、配置逻辑完全复用。
//!
//! **实时约束**：`CaptureCallback` / `RenderCallback` 运行在音频实时线程上，
//! 实现里禁止分配内存、加锁、做文件 IO。

use anyhow::Result;

/// 设备标识。
///
/// 用设备名作为标识：虚拟声卡的名字是固定的（`VB-Cable`、`XXX Mic`），
/// 而 cpal 不提供稳定的设备 ID。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId(String);

impl DeviceId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Input,
    Output,
}

impl Direction {
    pub fn label(&self) -> &'static str {
        match self {
            Direction::Input => "输入",
            Direction::Output => "输出",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub name: String,
    pub direction: Direction,
    pub is_default: bool,
    pub max_channels: u16,
    pub default_sample_rate: u32,
}

/// 内部统一格式：**32-bit float，交错存储**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl StreamFormat {
    pub const fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate,
            channels,
        }
    }

    pub const fn stereo(sample_rate: u32) -> Self {
        Self::new(sample_rate, 2)
    }

    pub fn ch(&self) -> usize {
        self.channels.max(1) as usize
    }

    pub fn frames(&self, samples: usize) -> usize {
        samples / self.ch()
    }
}

impl std::fmt::Display for StreamFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}Hz/{}ch", self.sample_rate, self.channels)
    }
}

/// 采集回调。`data` 是与设备格式一致的交错 f32。
pub type CaptureCallback = Box<dyn FnMut(&[f32]) + Send>;

/// 渲染回调。必须把 `out` 全部填满。
pub type RenderCallback = Box<dyn FnMut(&mut [f32]) + Send>;

/// 一个已打开的音频流。**Drop 即停止。**
///
/// 注意：**故意不要求 `Send`** —— cpal 的 macOS 后端里 `Stream` 不是 `Send`
/// （内部含 `NotSendSyncAcrossAllPlatforms` 标记）。
/// 因此流必须留在创建它的那个线程上；上层用"专用音频线程 + channel"的模式持有它。
pub trait AudioStream {
    fn format(&self) -> StreamFormat;
    fn device_name(&self) -> &str;
}

/// 平台音频后端。
pub trait AudioBackend: Send + Sync {
    /// 后端名字，用于日志（`coreaudio` / `wasapi`）。
    fn backend_name(&self) -> &'static str;

    /// 枚举所有输入与输出设备。同名设备只出现一次（虚拟声卡通常两侧同名）。
    fn devices(&self) -> Result<Vec<DeviceInfo>>;

    fn default_input(&self) -> Option<DeviceId>;
    fn default_output(&self) -> Option<DeviceId>;

    /// 询问"如果用 `prefer` 打开这个设备，最终会协商成什么格式"。
    ///
    /// 必须在打开流**之前**调用 —— 引擎需要知道真实格式才能预分配缓冲。
    fn input_format(&self, id: &DeviceId, prefer: Option<StreamFormat>) -> Result<StreamFormat>;
    fn output_format(&self, id: &DeviceId, prefer: Option<StreamFormat>) -> Result<StreamFormat>;

    /// 打开采集流。`prefer` 为期望格式，后端会挑选最接近的可用配置。
    fn open_input(
        &self,
        id: &DeviceId,
        prefer: Option<StreamFormat>,
        cb: CaptureCallback,
    ) -> Result<Box<dyn AudioStream>>;

    /// 打开渲染流。
    fn open_output(
        &self,
        id: &DeviceId,
        prefer: Option<StreamFormat>,
        cb: RenderCallback,
    ) -> Result<Box<dyn AudioStream>>;

    /// 按名字模糊查找设备（大小写不敏感的包含匹配）。
    fn find(&self, needle: &str) -> Result<Option<DeviceInfo>> {
        let needle = needle.to_lowercase();
        Ok(self
            .devices()?
            .into_iter()
            .find(|d| d.name.to_lowercase().contains(&needle)))
    }

    /// 找一个虚拟声卡的输出设备名 —— 按优先级尝试已知的虚拟声卡关键字。
    fn guess_virtual_output(&self) -> Option<DeviceId> {
        let outs: Vec<_> = self
            .devices()
            .ok()?
            .into_iter()
            .filter(|d| d.direction == Direction::Output)
            .collect();
        // 按关键字顺序找：装了多个虚拟声卡时靠前的优先
        for k in VIRTUAL_KEYS {
            if let Some(d) = outs.iter().find(|d| d.name.to_lowercase().contains(k)) {
                return Some(d.id.clone());
            }
        }
        None
    }
}


/// 已知虚拟声卡的名称关键字，**按优先级排列**。
///
/// 顺序有意义：机器上同时装了多个时，靠前的优先。
/// `virtual` / `虚拟` 放最后，因为它们最宽泛，可能误伤真实设备名。
pub const VIRTUAL_KEYS: &[&str] = &[
    "blackhole",
    "vb-cable",
    "cable input",
    "cable output",
    "cable",
    "loopback",
    "soundflower",
    "virtual audio",
    "virtual",
    "虚拟",
];

/// 这个名字看起来是不是虚拟声卡。
pub fn is_virtual_name(name: &str) -> bool {
    let n = name.to_lowercase();
    VIRTUAL_KEYS.iter().any(|k| n.contains(k))
}


#[cfg(test)]
mod virtual_name_tests {
    use super::is_virtual_name;

    #[test]
    fn 常见虚拟声卡都应被认出() {
        for n in [
            "BlackHole 2ch",
            "BlackHole 16ch",
            "VB-CABLE Input",
            "CABLE Output (VB-Audio)",
            "Loopback Audio",
            "Soundflower (2ch)",
            "豆包会议记录虚拟设备",
            "Virtual Audio Device",
        ] {
            assert!(is_virtual_name(n), "「{n}」应被认成虚拟声卡");
        }
    }

    #[test]
    fn 真实设备不应被误判() {
        for n in [
            "JBL TUNE 310C USB-C",
            "MacBook Pro Speakers",
            "MacBook Pro Microphone",
            "Omi Recorder Audio Driver",
            "Mi Monitor",
            "AirPods Pro",
        ] {
            assert!(!is_virtual_name(n), "「{n}」是真实设备，不该被当成虚拟声卡");
        }
    }
}
