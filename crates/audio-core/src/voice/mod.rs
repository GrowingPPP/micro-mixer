//! 变声参数、后端接口与实现。

pub mod backend;
pub mod chain;
pub mod effects;
pub mod params;
pub mod runtime;

#[cfg(feature = "voice")]
pub mod signalsmith;

pub use backend::VoiceBackend;
pub use chain::VoiceChain;
pub use effects::EffectChain;
pub use params::VoiceParams;
pub use runtime::{VoicePhase, VoiceRuntime, VoiceSetup};

#[cfg(feature = "voice")]
pub use signalsmith::SignalsmithBackend;
