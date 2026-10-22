//! 变声参数、后端接口与实现。

pub mod backend;
pub mod chain;
pub mod params;
pub mod runtime;

#[cfg(feature = "voice-signalsmith")]
pub mod signalsmith;

pub use backend::VoiceBackend;
pub use chain::VoiceChain;
pub use params::VoiceParams;
pub use runtime::{VoicePhase, VoiceRuntime, VoiceSetup};

#[cfg(feature = "voice-signalsmith")]
pub use signalsmith::SignalsmithBackend;
