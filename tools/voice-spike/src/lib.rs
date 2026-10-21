//! 变声器技术验证：独立测量工程，不进入生产 workspace。

pub mod adapter;
pub mod backend;
pub mod bench;
pub mod metrics;
pub mod pitch_shift_backend;
pub mod signal;
pub mod signalsmith_backend;

#[global_allocator]
static GLOBAL: metrics::CountingAllocator = metrics::CountingAllocator;
