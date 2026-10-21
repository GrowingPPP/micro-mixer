use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

pub struct CountingAllocator;

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static MEASURING: AtomicBool = AtomicBool::new(false);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if MEASURING.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if MEASURING.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

pub fn measure_allocations<F: FnOnce()>(f: F) -> usize {
    ALLOC_COUNT.store(0, Ordering::SeqCst);
    MEASURING.store(true, Ordering::SeqCst);
    f();
    MEASURING.store(false, Ordering::SeqCst);
    ALLOC_COUNT.load(Ordering::SeqCst)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TimingStats {
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

pub fn timing_stats(samples: &[Duration]) -> TimingStats {
    if samples.is_empty() {
        return TimingStats::default();
    }
    let mut values: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    values.sort_by(|a, b| a.total_cmp(b));
    let p50 = values[(values.len() - 1) * 50 / 100];
    let p99 = values[(values.len() - 1) * 99 / 100];
    TimingStats {
        p50_ms: p50,
        p99_ms: p99,
        max_ms: *values.last().unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn percentiles_are_ordered_and_include_max() {
        let values = [
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(3),
            Duration::from_millis(4),
        ];
        let stats = timing_stats(&values);
        assert!(stats.p50_ms >= 2.0 && stats.p50_ms <= 3.0);
        assert!(stats.p99_ms >= stats.p50_ms);
        assert_eq!(stats.max_ms, 4.0);
    }

    #[test]
    fn allocation_counter_sees_a_vec_growth() {
        let allocations = measure_allocations(|| {
            let mut values = Vec::new();
            for i in 0..1_000 {
                values.push(i);
            }
            std::hint::black_box(values);
        });
        assert!(allocations > 0, "allocations={allocations}");
    }

    #[test]
    fn allocation_counter_reports_zero_for_pure_math() {
        let allocations = measure_allocations(|| {
            let mut sum = 0.0f32;
            for i in 0..10_000 {
                sum += (i as f32).sin();
            }
            std::hint::black_box(sum);
        });
        assert_eq!(allocations, 0);
    }
}
