//! 变声运行状态：启用、预热、湿声与淡出之间的共享阶段与代次。
//!
//! 控制线程调用请求方法，采集回调按阶段推进；两侧只通过无锁原子量通信。
//! 阶段与代次打包在同一个原子量里，完成回调可以一次性校验代次和阶段，
//! 旧代次的预热或淡出结果不会覆盖新请求。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::{VoiceBackend, VoiceParams};

/// 变声运行阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VoicePhase {
    /// 变声未启用，采集侧只写干声。
    Off = 0,
    /// 湿声预热中；干声继续写环，湿声只在链内排出并丢弃。
    Warmup = 1,
    /// 已切换为湿声。
    Wet = 2,
    /// 正在淡回干声。
    FadingOut = 3,
}

impl VoicePhase {
    fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::Warmup,
            2 => Self::Wet,
            3 => Self::FadingOut,
            _ => Self::Off,
        }
    }
}

const PHASE_BITS: u32 = 8;
const PHASE_MASK: u64 = (1 << PHASE_BITS) - 1;

#[inline]
fn pack(generation: u64, phase: VoicePhase) -> u64 {
    (generation << PHASE_BITS) | phase as u64
}

#[inline]
fn unpack(raw: u64) -> (u64, VoicePhase) {
    (
        raw >> PHASE_BITS,
        VoicePhase::from_raw((raw & PHASE_MASK) as u8),
    )
}

/// 采集侧与渲染侧共享的变声状态。
///
/// 请求方法可在任意线程调用；采集回调通过 [`VoiceRuntime::phase`] 读取阶段，
/// 并在完成预热或淡出时带上代次提交，避免旧回调改变新状态。
#[derive(Debug)]
pub struct VoiceRuntime {
    state: AtomicU64,
}

impl Default for VoiceRuntime {
    fn default() -> Self {
        Self {
            state: AtomicU64::new(pack(0, VoicePhase::Off)),
        }
    }
}

impl VoiceRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// 请求启用。关闭或淡出状态下进入预热；预热或湿声状态下不变。
    pub fn request_enable(&self) {
        loop {
            let raw = self.state.load(Ordering::Acquire);
            let (generation, phase) = unpack(raw);
            match phase {
                VoicePhase::Warmup | VoicePhase::Wet => return,
                VoicePhase::Off | VoicePhase::FadingOut => {
                    let next = pack(generation + 1, VoicePhase::Warmup);
                    if self
                        .state
                        .compare_exchange_weak(raw, next, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        return;
                    }
                }
            }
        }
    }

    /// 请求关闭。预热中直接回到干声；湿声中先淡出再回到干声。
    pub fn request_disable(&self) {
        self.request_off_or_fade();
    }

    /// 请求临时旁路。与关闭共用淡出路径，只改变运行状态。
    pub fn request_bypass(&self) {
        self.request_off_or_fade();
    }

    fn request_off_or_fade(&self) {
        loop {
            let raw = self.state.load(Ordering::Acquire);
            let (generation, phase) = unpack(raw);
            let next_phase = match phase {
                VoicePhase::Off => return,
                VoicePhase::Warmup => VoicePhase::Off,
                VoicePhase::Wet => VoicePhase::FadingOut,
                VoicePhase::FadingOut => VoicePhase::FadingOut,
            };
            let next = pack(generation + 1, next_phase);
            if self
                .state
                .compare_exchange_weak(raw, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    /// 当前阶段。
    pub fn phase(&self) -> VoicePhase {
        unpack(self.state.load(Ordering::Acquire)).1
    }

    /// 当前代次；每次有效的启用、关闭或旁路请求都会推进。
    pub fn generation(&self) -> u64 {
        unpack(self.state.load(Ordering::Acquire)).0
    }

    /// 一次性读取阶段与代次，供音频回调在块边界同步状态。
    pub(crate) fn snapshot(&self) -> (VoicePhase, u64) {
        let (generation, phase) = unpack(self.state.load(Ordering::Acquire));
        (phase, generation)
    }

    /// 只允许发起预热的那一代把阶段推进到湿声。
    pub(crate) fn finish_warmup(&self, generation: u64) -> bool {
        let expected = pack(generation, VoicePhase::Warmup);
        let next = pack(generation, VoicePhase::Wet);
        self.state
            .compare_exchange(expected, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// 把当前的淡出阶段推进到关闭；新请求已改写阶段时返回 false。
    pub(crate) fn finish_fade_out(&self) -> bool {
        loop {
            let raw = self.state.load(Ordering::Acquire);
            let (generation, phase) = unpack(raw);
            if phase != VoicePhase::FadingOut {
                return false;
            }
            let next = pack(generation, VoicePhase::Off);
            if self
                .state
                .compare_exchange_weak(raw, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }
}

/// 变声会话的构造输入。
///
/// `block_ms` 决定 [`super::VoiceChain`] 的固定适配块；`backend` 由宿主在
/// 非实时线程创建并注入，测试可使用可控延迟替身。后端的 `Send` 约束保证
/// 引擎可以随采集处理移动到音频线程。`params` 在构造时写入后端。
pub struct VoiceSetup {
    pub params: VoiceParams,
    pub block_ms: f32,
    pub runtime: Arc<VoiceRuntime>,
    pub backend: Box<dyn VoiceBackend + Send>,
}

impl VoiceSetup {
    pub fn new(
        params: VoiceParams,
        block_ms: f32,
        runtime: Arc<VoiceRuntime>,
        backend: Box<dyn VoiceBackend + Send>,
    ) -> Self {
        Self {
            params,
            block_ms,
            runtime,
            backend,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 启用与关闭推进阶段和代次() {
        let runtime = VoiceRuntime::new();
        assert_eq!(runtime.phase(), VoicePhase::Off);
        assert_eq!(runtime.generation(), 0);

        runtime.request_enable();
        assert_eq!(runtime.phase(), VoicePhase::Warmup);
        let warming = runtime.generation();
        runtime.request_enable();
        assert_eq!(runtime.generation(), warming, "重复启用不应推进代次");

        assert!(runtime.finish_warmup(warming));
        assert_eq!(runtime.phase(), VoicePhase::Wet);

        runtime.request_disable();
        assert_eq!(runtime.phase(), VoicePhase::FadingOut);
        let fading = runtime.generation();
        assert!(fading > warming);
        assert!(runtime.finish_fade_out());
        assert_eq!(runtime.phase(), VoicePhase::Off);
        assert_eq!(runtime.generation(), fading);
    }

    #[test]
    fn 旧代次不能完成新一次预热() {
        let runtime = VoiceRuntime::new();
        runtime.request_enable();
        let stale = runtime.generation();
        runtime.request_disable();
        runtime.request_enable();
        let current = runtime.generation();

        assert!(!runtime.finish_warmup(stale), "旧代次不得切到湿声");
        assert_eq!(runtime.phase(), VoicePhase::Warmup);
        assert!(runtime.finish_warmup(current));
        assert_eq!(runtime.phase(), VoicePhase::Wet);
    }

    #[test]
    fn 旧淡出完成回调不能关闭新代次() {
        let runtime = VoiceRuntime::new();
        runtime.request_enable();
        let warming = runtime.generation();
        assert!(runtime.finish_warmup(warming));
        runtime.request_disable();
        assert_eq!(runtime.phase(), VoicePhase::FadingOut);

        runtime.request_enable();
        assert_eq!(runtime.phase(), VoicePhase::Warmup);
        assert!(!runtime.finish_fade_out(), "新代次已预热时不得回到关闭");
        assert_eq!(runtime.phase(), VoicePhase::Warmup);
    }

    #[test]
    fn 临时旁路与关闭共用淡出路径() {
        let runtime = VoiceRuntime::new();
        runtime.request_enable();
        let warming = runtime.generation();
        assert!(runtime.finish_warmup(warming));

        runtime.request_bypass();
        assert_eq!(runtime.phase(), VoicePhase::FadingOut);
        assert!(runtime.finish_fade_out());
        assert_eq!(runtime.phase(), VoicePhase::Off);
    }
}
