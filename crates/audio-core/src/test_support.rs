//! 测试专用的分配计数设施。
//!
//! 只统计当前线程经 Rust 全局分配器发生的分配，因此每个测试的计数互不影响。

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct TrackingAllocator;

thread_local! {
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
}

// SAFETY: 仅在原分配器之上记录当前线程的分配次数，并原样转发分配与释放。
unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| {
            if let Some(n) = count.get() {
                count.set(Some(n + 1));
            }
        });
        // SAFETY: 原样转发调用方提供的合法分配布局。
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: 使用与 System.alloc 相同的布局释放其返回的指针。
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static TEST_ALLOCATOR: TrackingAllocator = TrackingAllocator;

/// 统计 `f` 执行期间当前线程的分配次数。
pub(crate) fn count_allocations(f: impl FnOnce()) -> usize {
    struct Reset;

    impl Drop for Reset {
        fn drop(&mut self) {
            ALLOCATIONS.with(|count| count.set(None));
        }
    }

    ALLOCATIONS.with(|count| count.set(Some(0)));
    let reset = Reset;
    f();
    let allocations = ALLOCATIONS.with(|count| count.get().unwrap());
    drop(reset);
    allocations
}
