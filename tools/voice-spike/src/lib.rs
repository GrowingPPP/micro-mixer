//! 变声器技术验证：独立测量工程，不进入生产 workspace。

pub mod metrics;
pub mod signal;

#[global_allocator]
static GLOBAL: metrics::CountingAllocator = metrics::CountingAllocator;
