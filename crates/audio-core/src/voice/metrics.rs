//! 人声链指标：延迟、块长、处理耗时与丢帧计数。
//!
//! 控制侧通过 [`super::VoiceControl`] 读取快照；音频侧只做定宽原子写入，
//! 不分配、不加锁、不做 IO。

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// 一次读取的人声链指标。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VoiceMetricsSnapshot {
    /// 算法延迟：后端延迟加一个固定块。
    pub latency_frames: usize,
    /// 构造链时使用的输出采样率；兼容入口为 0。
    pub sample_rate: u32,
    /// 固定算法块长。
    pub block_frames: usize,
    /// 最近一次 `process` 的耗时，单位纳秒。
    pub process_ns_last: u64,
    /// 已观察到的最大 `process` 耗时，单位纳秒。
    pub process_ns_max: u64,
    /// 湿声 FIFO 因容量不足累计丢弃的帧数。
    pub overflow_frames: u64,
    /// 湿声暂时排空时由采集侧补齐的帧数。
    pub gap_frames: u64,
}

/// 人声链共享的指标槽。
///
/// 字段在构造时一次分配；音频侧写入只做原子操作，控制侧读取只做原子加载。
/// 快照各字段不要求来自同一瞬间。
#[derive(Default)]
pub(crate) struct VoiceMetrics {
    latency_frames: AtomicUsize,
    sample_rate: AtomicU32,
    block_frames: AtomicUsize,
    process_ns_last: AtomicU64,
    process_ns_max: AtomicU64,
    overflow_frames: AtomicU64,
    gap_frames: AtomicU64,
}

impl VoiceMetrics {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 写入构造时确定的静态配置；链创建后不再变化。
    pub(crate) fn configure(&self, sample_rate: u32, block_frames: usize, latency_frames: usize) {
        self.sample_rate.store(sample_rate, Ordering::Relaxed);
        self.block_frames.store(block_frames, Ordering::Relaxed);
        self.latency_frames.store(latency_frames, Ordering::Relaxed);
    }

    /// 记录一次 `process` 的耗时，并推进最大值。
    ///
    /// 先推进最大值再写最近值，任意时刻读取都满足最大值不小于最近值。
    pub(crate) fn record_process(&self, ns: u64) {
        self.process_ns_max.fetch_max(ns, Ordering::Relaxed);
        self.process_ns_last.store(ns, Ordering::Relaxed);
    }

    /// 累加每次处理的湿声 FIFO 丢弃增量。
    pub(crate) fn record_overflow(&self, frames: u64) {
        if frames != 0 {
            self.overflow_frames.fetch_add(frames, Ordering::Relaxed);
        }
    }

    /// 累加湿声排空时补齐的帧数。
    ///
    /// 长会话累计值必须饱和而非回绕，避免异常计数把缺口伪装成零。
    pub(crate) fn record_gap(&self, frames: u64) {
        if frames == 0 {
            return;
        }
        let mut current = self.gap_frames.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_add(frames);
            match self.gap_frames.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// 读取当前指标；只做原子加载。
    pub(crate) fn snapshot(&self) -> VoiceMetricsSnapshot {
        VoiceMetricsSnapshot {
            latency_frames: self.latency_frames.load(Ordering::Relaxed),
            sample_rate: self.sample_rate.load(Ordering::Relaxed),
            block_frames: self.block_frames.load(Ordering::Relaxed),
            process_ns_last: self.process_ns_last.load(Ordering::Relaxed),
            process_ns_max: self.process_ns_max.load(Ordering::Relaxed),
            overflow_frames: self.overflow_frames.load(Ordering::Relaxed),
            gap_frames: self.gap_frames.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 指标快照记录配置与累计计数() {
        let metrics = VoiceMetrics::new();
        metrics.configure(48_000, 480, 960);
        metrics.record_process(120);
        metrics.record_process(80);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.sample_rate, 48_000);
        assert_eq!(snapshot.block_frames, 480);
        assert_eq!(snapshot.latency_frames, 960);
        assert_eq!(snapshot.process_ns_last, 80);
        assert_eq!(snapshot.process_ns_max, 120, "最大值不回退");

        metrics.record_overflow(3);
        metrics.record_overflow(4);
        metrics.record_gap(5);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.overflow_frames, 7);
        assert_eq!(snapshot.gap_frames, 5);
    }

    #[test]
    fn 缺帧计数饱和不回绕() {
        let metrics = VoiceMetrics::new();
        metrics.record_gap(u64::MAX);
        metrics.record_gap(1);

        assert_eq!(
            metrics.snapshot().gap_frames,
            u64::MAX,
            "长时间会话的累计缺帧不得回绕"
        );
    }
}
