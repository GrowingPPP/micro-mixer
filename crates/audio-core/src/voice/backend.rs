//! 变声后端接口。

use super::VoiceParams;

/// 单声道变声后端。
///
/// 实现位于音频回调路径，`process`、`reset` 与 `set_params` 都必须
/// 不分配、不加锁、不做 IO。
pub trait VoiceBackend {
    /// 处理输入并写入输出，返回实际处理的帧数。
    ///
    /// 输入与输出按帧对齐；输出较短时只处理可容纳的帧数。
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize;

    /// 后端引入的算法延迟（帧）。
    fn latency_frames(&self) -> usize;

    /// 清空内部状态，保留当前参数与配置。
    fn reset(&mut self);

    /// 更新变声参数，不重建后端。
    fn set_params(&mut self, params: VoiceParams);
}
