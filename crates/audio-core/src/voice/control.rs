//! 人声参数快照与控制句柄。
//!
//! 控制侧把整份 [`VoiceParams`] 写入预分配的原子字段，并递增版本号标记提交；
//! 音频侧在块边界读取版本与整份快照，版本不一致时沿用上一份完整快照。
//! 读取路径无锁、无分配，任意数量的链可以共享同一快照。

use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::{VoiceParams, VoicePhase, VoiceRuntime};

/// 快照的原子字段。
///
/// 浮点数按位存储。读取方在版本号包围的区间内复制整份字段；版本变化时
/// 丢弃这次结果，因此不会接受半套参数。
struct SnapshotFields {
    pitch_semitones: AtomicU32,
    formant_semitones: AtomicU32,
    gate_enabled: AtomicBool,
    gate_db: AtomicU32,
    eq_low_enabled: AtomicBool,
    eq_low_db: AtomicU32,
    eq_mid_enabled: AtomicBool,
    eq_mid_db: AtomicU32,
    eq_high_enabled: AtomicBool,
    eq_high_db: AtomicU32,
    drive_enabled: AtomicBool,
    drive: AtomicU32,
    robot_enabled: AtomicBool,
    robot_hz: AtomicU32,
    robot_mix: AtomicU32,
}

impl SnapshotFields {
    fn new(params: VoiceParams) -> Self {
        Self {
            pitch_semitones: AtomicU32::new(params.pitch_semitones.to_bits()),
            formant_semitones: AtomicU32::new(params.formant_semitones.to_bits()),
            gate_enabled: AtomicBool::new(params.gate_enabled),
            gate_db: AtomicU32::new(params.gate_db.to_bits()),
            eq_low_enabled: AtomicBool::new(params.eq_low_enabled),
            eq_low_db: AtomicU32::new(params.eq_low_db.to_bits()),
            eq_mid_enabled: AtomicBool::new(params.eq_mid_enabled),
            eq_mid_db: AtomicU32::new(params.eq_mid_db.to_bits()),
            eq_high_enabled: AtomicBool::new(params.eq_high_enabled),
            eq_high_db: AtomicU32::new(params.eq_high_db.to_bits()),
            drive_enabled: AtomicBool::new(params.drive_enabled),
            drive: AtomicU32::new(params.drive.to_bits()),
            robot_enabled: AtomicBool::new(params.robot_enabled),
            robot_hz: AtomicU32::new(params.robot_hz.to_bits()),
            robot_mix: AtomicU32::new(params.robot_mix.to_bits()),
        }
    }

    fn write(&self, params: VoiceParams) {
        self.pitch_semitones
            .store(params.pitch_semitones.to_bits(), Ordering::Relaxed);
        self.formant_semitones
            .store(params.formant_semitones.to_bits(), Ordering::Relaxed);
        self.gate_enabled
            .store(params.gate_enabled, Ordering::Relaxed);
        self.gate_db
            .store(params.gate_db.to_bits(), Ordering::Relaxed);
        self.eq_low_enabled
            .store(params.eq_low_enabled, Ordering::Relaxed);
        self.eq_low_db
            .store(params.eq_low_db.to_bits(), Ordering::Relaxed);
        self.eq_mid_enabled
            .store(params.eq_mid_enabled, Ordering::Relaxed);
        self.eq_mid_db
            .store(params.eq_mid_db.to_bits(), Ordering::Relaxed);
        self.eq_high_enabled
            .store(params.eq_high_enabled, Ordering::Relaxed);
        self.eq_high_db
            .store(params.eq_high_db.to_bits(), Ordering::Relaxed);
        self.drive_enabled
            .store(params.drive_enabled, Ordering::Relaxed);
        self.drive.store(params.drive.to_bits(), Ordering::Relaxed);
        self.robot_enabled
            .store(params.robot_enabled, Ordering::Relaxed);
        self.robot_hz
            .store(params.robot_hz.to_bits(), Ordering::Relaxed);
        self.robot_mix
            .store(params.robot_mix.to_bits(), Ordering::Relaxed);
    }

    fn read(&self) -> VoiceParams {
        VoiceParams {
            pitch_semitones: f32::from_bits(self.pitch_semitones.load(Ordering::Relaxed)),
            formant_semitones: f32::from_bits(self.formant_semitones.load(Ordering::Relaxed)),
            gate_enabled: self.gate_enabled.load(Ordering::Relaxed),
            gate_db: f32::from_bits(self.gate_db.load(Ordering::Relaxed)),
            eq_low_enabled: self.eq_low_enabled.load(Ordering::Relaxed),
            eq_low_db: f32::from_bits(self.eq_low_db.load(Ordering::Relaxed)),
            eq_mid_enabled: self.eq_mid_enabled.load(Ordering::Relaxed),
            eq_mid_db: f32::from_bits(self.eq_mid_db.load(Ordering::Relaxed)),
            eq_high_enabled: self.eq_high_enabled.load(Ordering::Relaxed),
            eq_high_db: f32::from_bits(self.eq_high_db.load(Ordering::Relaxed)),
            drive_enabled: self.drive_enabled.load(Ordering::Relaxed),
            drive: f32::from_bits(self.drive.load(Ordering::Relaxed)),
            robot_enabled: self.robot_enabled.load(Ordering::Relaxed),
            robot_hz: f32::from_bits(self.robot_hz.load(Ordering::Relaxed)),
            robot_mix: f32::from_bits(self.robot_mix.load(Ordering::Relaxed)),
        }
    }
}

/// 无锁参数快照。
///
/// 版本号为奇数表示一次提交正在进行。读取方只有在读取前后版本号一致且为
/// 偶数时才接受整份字段，否则沿用调用方给出的上一份完整快照。控制侧写入由
/// 互斥量串行化；音频读取与任意数量的链共享快照都不加锁。
pub struct VoiceParamBuffer {
    fields: SnapshotFields,
    version: AtomicU64,
    writer: Mutex<()>,
}

impl VoiceParamBuffer {
    /// 创建包含初始快照的缓冲区；字段在此时一次分配。
    pub fn new(params: VoiceParams) -> Arc<Self> {
        Arc::new(Self {
            fields: SnapshotFields::new(params),
            version: AtomicU64::new(0),
            writer: Mutex::new(()),
        })
    }

    /// 发布一份完整快照，下一次块边界可见。
    ///
    /// 控制侧写入由内部互斥量串行化；音频侧不参与加锁。
    pub fn publish(&self, params: VoiceParams) {
        let _guard = self.lock_writer();
        self.version.fetch_add(1, Ordering::Relaxed);
        // 奇数版本必须先于字段写入可见，否则读者可能把新字段与旧版本配成一套。
        fence(Ordering::Release);
        self.fields.write(params);
        // 字段写入必须先于偶数版本可见，读者看到偶数版本时才接受整份字段。
        fence(Ordering::Release);
        self.version.fetch_add(1, Ordering::Release);
    }

    /// 最近一次完整快照，供状态 DTO 与调试读取。
    pub fn current(&self) -> VoiceParams {
        let _guard = self.lock_writer();
        self.fields.read()
    }

    /// 读取当前快照；发布进行中或字段在读取期间变化时返回 `previous`。
    ///
    /// 音频侧每个算法块调用一次。返回的字段来自同一次发布。
    pub(crate) fn read(&self, previous: VoiceParams) -> VoiceParams {
        let start = self.version.load(Ordering::Acquire);
        if start & 1 != 0 {
            return previous;
        }
        let params = self.fields.read();
        // 字段读取必须在第二次版本检查前完成，避免迟到的字段读取混入下一次提交。
        fence(Ordering::Acquire);
        let end = self.version.load(Ordering::Acquire);
        if start == end {
            params
        } else {
            previous
        }
    }

    fn lock_writer(&self) -> MutexGuard<'_, ()> {
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 宿主侧人声控制句柄。
///
/// 克隆共享同一运行状态与参数缓冲区；所有方法都在控制线程调用。
#[derive(Clone)]
pub struct VoiceControl {
    runtime: Arc<VoiceRuntime>,
    param_buffer: Arc<VoiceParamBuffer>,
}

impl VoiceControl {
    /// 创建独立运行状态，并发布初始参数。
    pub fn new(params: VoiceParams) -> Self {
        Self::with_runtime(Arc::new(VoiceRuntime::new()), params)
    }

    /// 复用已有运行状态，并发布初始参数。
    pub fn with_runtime(runtime: Arc<VoiceRuntime>, params: VoiceParams) -> Self {
        Self {
            runtime,
            param_buffer: VoiceParamBuffer::new(params),
        }
    }

    /// 发布完整参数快照；下一次算法块边界生效。
    pub fn set_params(&self, params: VoiceParams) {
        self.param_buffer.publish(params);
    }

    /// 最近一次完整参数快照。
    pub fn params(&self) -> VoiceParams {
        self.param_buffer.current()
    }

    /// 请求启用变声。
    pub fn request_enable(&self) {
        self.runtime.request_enable();
    }

    /// 请求关闭变声。
    pub fn request_disable(&self) {
        self.runtime.request_disable();
    }

    /// 请求临时旁路。
    pub fn request_bypass(&self) {
        self.runtime.request_bypass();
    }

    /// 当前变声阶段。
    pub fn phase(&self) -> VoicePhase {
        self.runtime.phase()
    }

    /// 当前运行代次。
    pub fn generation(&self) -> u64 {
        self.runtime.generation()
    }

    /// 渲染侧报告该代次的路径限幅影子预热已完成。
    pub fn mark_render_ready(&self, generation: u64) {
        self.runtime.mark_render_ready(generation);
    }

    pub(crate) fn runtime(&self) -> Arc<VoiceRuntime> {
        Arc::clone(&self.runtime)
    }

    pub(crate) fn param_buffer(&self) -> &Arc<VoiceParamBuffer> {
        &self.param_buffer
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::test_support::count_allocations;

    fn 预设甲() -> VoiceParams {
        VoiceParams {
            pitch_semitones: 7.0,
            formant_semitones: -5.0,
            gate_enabled: true,
            gate_db: -30.0,
            eq_low_enabled: true,
            eq_low_db: 3.0,
            eq_mid_enabled: true,
            eq_mid_db: -2.0,
            eq_high_enabled: true,
            eq_high_db: 4.0,
            drive_enabled: true,
            drive: 0.4,
            robot_enabled: true,
            robot_hz: 80.0,
            robot_mix: 0.6,
        }
    }

    fn 预设乙() -> VoiceParams {
        VoiceParams {
            pitch_semitones: -9.0,
            formant_semitones: 6.0,
            gate_enabled: false,
            gate_db: -55.0,
            eq_low_enabled: false,
            eq_low_db: -5.0,
            eq_mid_enabled: false,
            eq_mid_db: 6.0,
            eq_high_enabled: false,
            eq_high_db: -7.0,
            drive_enabled: false,
            drive: 0.9,
            robot_enabled: false,
            robot_hz: 900.0,
            robot_mix: 0.1,
        }
    }

    #[test]
    fn 参数快照在块边界一致() {
        let buffer = VoiceParamBuffer::new(VoiceParams::neutral());
        assert_eq!(buffer.read(VoiceParams::neutral()), VoiceParams::neutral());

        buffer.publish(预设甲());
        buffer.publish(预设乙());
        let first = buffer.read(VoiceParams::neutral());
        assert!(
            first == 预设甲() || first == 预设乙(),
            "连续发布后必须读到其中一份完整快照，实际 {first:?}"
        );

        buffer.publish(预设甲());
        let second = buffer.read(first);
        assert_eq!(second, 预设甲(), "新的块边界应看到最近一次发布");
        assert_eq!(buffer.current(), 预设甲());
    }

    #[test]
    fn 发布参数不分配且音频读取零分配() {
        let buffer = VoiceParamBuffer::new(VoiceParams::neutral());
        // 平台互斥量首次加锁会做一次性初始化；构造链时读取初始快照已完成这一步。
        assert_eq!(buffer.current(), VoiceParams::neutral());
        let mut read = VoiceParams::neutral();

        assert_eq!(
            count_allocations(|| buffer.publish(预设甲())),
            0,
            "控制侧发布快照不得分配"
        );
        assert_eq!(
            count_allocations(|| {
                read = buffer.read(read);
            }),
            0,
            "音频侧读取快照不得分配"
        );
        assert_eq!(read, 预设甲());
        assert_eq!(
            count_allocations(|| {
                let _ = buffer.current();
            }),
            0,
            "控制侧读取最近快照不得分配"
        );
    }

    #[test]
    fn 多链共享快照互不影响() {
        let buffer = VoiceParamBuffer::new(VoiceParams::neutral());
        buffer.publish(预设甲());
        assert_eq!(buffer.read(VoiceParams::neutral()), 预设甲());

        buffer.publish(预设乙());
        // 模拟新旧两条链交错读取同一缓冲区。
        assert_eq!(buffer.read(VoiceParams::neutral()), 预设乙());
        assert_eq!(buffer.read(预设甲()), 预设乙());

        buffer.publish(预设甲());
        assert_eq!(buffer.read(VoiceParams::neutral()), 预设甲());
        assert_eq!(buffer.current(), 预设甲());
    }

    #[test]
    fn 控制句柄共享阶段与参数() {
        let runtime = Arc::new(VoiceRuntime::new());
        let control = VoiceControl::with_runtime(runtime.clone(), VoiceParams::neutral());
        let clone = control.clone();

        control.set_params(预设甲());
        assert_eq!(clone.params(), 预设甲());

        clone.request_enable();
        assert_eq!(control.phase(), VoicePhase::Warmup);
        let warming = control.generation();
        control.mark_render_ready(warming);
        assert_eq!(runtime.phase(), VoicePhase::Warmup, "就绪标记不直接切湿声");
        assert!(runtime.finish_warmup(warming));
        assert_eq!(control.phase(), VoicePhase::Wet);

        clone.request_bypass();
        assert_eq!(control.phase(), VoicePhase::FadingOut);
        let fading = control.generation();
        assert!(fading > warming);
        clone.request_disable();
        assert!(control.generation() > fading);
    }
}
