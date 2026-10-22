//! 变声参数、后端接口与实现。

pub mod backend;
pub mod chain;
pub mod params;

#[cfg(feature = "voice-signalsmith")]
pub mod signalsmith;

pub use backend::VoiceBackend;
pub use chain::VoiceChain;
pub use params::VoiceParams;

#[cfg(feature = "voice-signalsmith")]
pub use signalsmith::SignalsmithBackend;
