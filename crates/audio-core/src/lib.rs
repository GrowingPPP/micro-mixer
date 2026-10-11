//! # audio-core
//!
//! 平台无关的音频核心：设备抽象、混音引擎、重采样、限幅、解码。
//!
//! 设计要点见 `docs/技术设计-音频架构与驱动.md`：
//! - **纯环回架构** —— 驱动只做环回，混音全部在 App 内完成
//! - **实时安全** —— 音频回调路径零分配、零加锁、零 IO
//! - **平台无关** —— macOS 与 Windows 的后端各自实现 [`device::AudioBackend`]，
//!   引擎与 UI 完全复用

pub mod decode;
pub mod device;
pub mod engine;
pub mod limiter;
pub mod resample;

pub use device::{AudioBackend, AudioStream, DeviceId, DeviceInfo, Direction, StreamFormat};
pub use engine::{Command, EngineParts, EngineStats, PlaybackControl, SfxSlot, StatsSnapshot};
