//! 变声参数、后端接口与实现。

use anyhow::Result;

pub mod backend;
pub mod chain;
pub mod control;
pub mod effects;
mod metrics;
pub mod params;
pub mod preview;
pub mod runtime;

#[cfg(feature = "voice")]
pub mod signalsmith;

pub use backend::VoiceBackend;
pub use chain::VoiceChain;
pub use control::{VoiceControl, VoiceParamBuffer};
pub use effects::EffectChain;
pub use metrics::VoiceMetricsSnapshot;
pub use params::VoiceParams;
pub use preview::{
    PreviewError, VoicePreviewBuffer, VoicePreviewClip, VoicePreviewControl, VoicePreviewSource,
    VoicePreviewStart,
};
pub use runtime::{VoicePhase, VoiceRuntime, VoiceSetup};

/// 按输出采样率创建变声后端的工厂。
///
/// 会话在构造时用实际输出采样率调用一次，后端实例随后随采集处理移动到
/// 音频线程；`Send` 约束保证这条移动路径安全。
pub type VoiceBackendFactory = Box<dyn Fn(u32) -> Result<Box<dyn VoiceBackend + Send>> + Send>;

#[cfg(feature = "voice")]
pub use signalsmith::{backend_factory, SignalsmithBackend};
