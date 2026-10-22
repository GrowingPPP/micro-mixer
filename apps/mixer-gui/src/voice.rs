//! 宿主变声状态、命令与持久化。
//!
//! 状态变更在内部锁里串行执行；音频回调只通过 [`VoiceControl`] 的原子快照
//! 读取参数，不会接触这里的锁。启用与激活预设有独立写入门，并由单个合并
//! 写入线程落盘：连续操作只保留最新待写快照，旧保存结果不能覆盖新状态。

#[cfg(feature = "voice")]
use std::sync::{Arc, Condvar, Mutex};

use audio_core::voice::VoiceParams;
use serde::{Deserialize, Serialize};

#[cfg(feature = "voice")]
use std::path::{Path, PathBuf};

#[cfg(feature = "voice")]
use anyhow::Result;

#[cfg(feature = "voice")]
use app_config::{builtin_voice_presets, Config, VoiceParamsConfig, VoicePreset};

#[cfg(feature = "voice")]
use audio_core::voice::{VoiceControl, VoicePhase};

#[cfg(feature = "voice")]
use audio_session::{signalsmith_voice_backend_factory, VoiceOptions};

/// 会话缺省的人声算法块时长（毫秒）。
#[cfg(feature = "voice")]
pub(crate) const DEFAULT_VOICE_BLOCK_MS: f32 = 10.0;

/// 与音频引擎的声明的上限保持一致，避免把非法值送到实时线程触发断言。
#[cfg(feature = "voice")]
const MAX_VOICE_BLOCK_MS: f32 = 40.0;

/// 完整变声参数的命令与状态形状。
///
/// 字段名与 `audio-core::VoiceParams` 一一对应。命令入口先转成音频参数并
/// 调用 `sanitized()`，越界与非有限值不会进入音频线程。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct VoiceParamsDto {
    pub pitch_semitones: f32,
    pub formant_semitones: f32,
    pub gate_enabled: bool,
    pub gate_db: f32,
    pub eq_low_enabled: bool,
    pub eq_low_db: f32,
    pub eq_mid_enabled: bool,
    pub eq_mid_db: f32,
    pub eq_high_enabled: bool,
    pub eq_high_db: f32,
    pub drive_enabled: bool,
    pub drive: f32,
    pub robot_enabled: bool,
    pub robot_hz: f32,
    pub robot_mix: f32,
}

impl Default for VoiceParamsDto {
    fn default() -> Self {
        Self::from(VoiceParams::neutral())
    }
}

impl From<VoiceParams> for VoiceParamsDto {
    fn from(params: VoiceParams) -> Self {
        Self {
            pitch_semitones: params.pitch_semitones,
            formant_semitones: params.formant_semitones,
            gate_enabled: params.gate_enabled,
            gate_db: params.gate_db,
            eq_low_enabled: params.eq_low_enabled,
            eq_low_db: params.eq_low_db,
            eq_mid_enabled: params.eq_mid_enabled,
            eq_mid_db: params.eq_mid_db,
            eq_high_enabled: params.eq_high_enabled,
            eq_high_db: params.eq_high_db,
            drive_enabled: params.drive_enabled,
            drive: params.drive,
            robot_enabled: params.robot_enabled,
            robot_hz: params.robot_hz,
            robot_mix: params.robot_mix,
        }
    }
}

impl From<VoiceParamsDto> for VoiceParams {
    fn from(params: VoiceParamsDto) -> Self {
        Self {
            pitch_semitones: params.pitch_semitones,
            formant_semitones: params.formant_semitones,
            gate_enabled: params.gate_enabled,
            gate_db: params.gate_db,
            eq_low_enabled: params.eq_low_enabled,
            eq_low_db: params.eq_low_db,
            eq_mid_enabled: params.eq_mid_enabled,
            eq_mid_db: params.eq_mid_db,
            eq_high_enabled: params.eq_high_enabled,
            eq_high_db: params.eq_high_db,
            drive_enabled: params.drive_enabled,
            drive: params.drive,
            robot_enabled: params.robot_enabled,
            robot_hz: params.robot_hz,
            robot_mix: params.robot_mix,
        }
    }
}

/// 预设列表条目；内置预设只读，用户预设由后续命令维护。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct VoicePresetDto {
    pub id: String,
    pub name: String,
    pub builtin: bool,
    pub params: VoiceParamsDto,
}

/// 试听状态占位；录制与回放接入后由后续任务补全。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct VoicePreviewDto {
    pub state: String,
    pub elapsed_ms: u64,
    pub reason: Option<String>,
}

impl VoicePreviewDto {
    fn idle() -> Self {
        Self {
            state: "idle".to_owned(),
            elapsed_ms: 0,
            reason: None,
        }
    }
}

/// 250 ms 轮询使用的紧凑变声状态。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct VoiceStatusDto {
    pub enabled: bool,
    pub bypassed: bool,
    pub phase: String,
    pub preset_id: String,
    pub preset_name: String,
    pub modified: bool,
    pub draft_available: bool,
    pub params: VoiceParamsDto,
    pub preview: VoicePreviewDto,
    pub save_error: Option<String>,
}

impl VoiceStatusDto {
    /// 未编译变声后端时的只读状态；命令会返回明确错误。
    #[cfg(not(feature = "voice"))]
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            bypassed: false,
            phase: "off".to_owned(),
            preset_id: "builtin:female".to_owned(),
            preset_name: "女声".to_owned(),
            modified: false,
            draft_available: false,
            params: VoiceParamsDto::default(),
            preview: VoicePreviewDto::idle(),
            save_error: None,
        }
    }
}

/// `StartConfig` 携带的会话初始变声值。
///
/// 缺省字段回退到宿主当前配置；该结构不参与启动指纹，参数变化不重建会话。
// 未编译变声后端时保留反序列化形状，字段不会进入运行路径。
#[cfg_attr(not(feature = "voice"), allow(dead_code))]
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct VoiceStartConfig {
    pub enabled: bool,
    #[serde(default)]
    pub params: Option<VoiceParamsDto>,
    #[serde(default)]
    pub block_ms: Option<f32>,
}

#[cfg(feature = "voice")]
fn phase_name(phase: VoicePhase) -> &'static str {
    match phase {
        VoicePhase::Off => "off",
        VoicePhase::Warmup => "warmup",
        VoicePhase::Wet => "wet",
        VoicePhase::FadingOut => "fading_out",
    }
}

#[cfg(feature = "voice")]
fn params_from_config(params: VoiceParamsConfig) -> VoiceParams {
    VoiceParams {
        pitch_semitones: params.pitch_semitones,
        formant_semitones: params.formant_semitones,
        gate_enabled: params.gate_enabled,
        gate_db: params.gate_db,
        eq_low_enabled: params.eq_low_enabled,
        eq_low_db: params.eq_low_db,
        eq_mid_enabled: params.eq_mid_enabled,
        eq_mid_db: params.eq_mid_db,
        eq_high_enabled: params.eq_high_enabled,
        eq_high_db: params.eq_high_db,
        drive_enabled: params.drive_enabled,
        drive: params.drive,
        robot_enabled: params.robot_enabled,
        robot_hz: params.robot_hz,
        robot_mix: params.robot_mix,
    }
}

#[cfg(feature = "voice")]
fn preset_dto(preset: &VoicePreset) -> VoicePresetDto {
    VoicePresetDto {
        id: preset.id.clone(),
        name: preset.name.clone(),
        builtin: preset.id.starts_with("builtin:"),
        params: params_from_config(preset.params).into(),
    }
}

/// 一次待落盘的启用状态与激活预设。
#[cfg(feature = "voice")]
#[derive(Debug, Clone)]
struct VoiceWrite {
    generation: u64,
    enabled: bool,
    preset_id: String,
}

#[cfg(feature = "voice")]
type VoiceSaveFn = dyn Fn(&Path, &VoiceWrite) -> Result<()> + Send + Sync;

#[cfg(feature = "voice")]
struct VoiceState {
    enabled: bool,
    bypassed: bool,
    preset_id: String,
    preset_name: String,
    /// 当前预设自身的参数，用于判断是否存在未保存修改。
    saved_params: VoiceParams,
    params: VoiceParams,
    user_presets: Vec<VoicePreset>,
    draft_available: bool,
    preview: VoicePreviewDto,
    save_error: Option<String>,
    control: Option<VoiceControl>,
    /// 用户命令与运行状态变更的代次；异步保存结果据此丢弃过期回复。
    operation: u64,
    save_generation: u64,
    pending_write: Option<VoiceWrite>,
    writer_active: bool,
}

#[cfg(feature = "voice")]
struct VoiceShared {
    state: Mutex<VoiceState>,
    writes_finished: Condvar,
    config_path: PathBuf,
    save: Arc<VoiceSaveFn>,
}

/// 宿主变声状态与命令入口。
#[cfg(feature = "voice")]
pub(crate) struct VoiceService {
    shared: Arc<VoiceShared>,
}

#[cfg(feature = "voice")]
impl Default for VoiceService {
    fn default() -> Self {
        Self::new(&Config::load())
    }
}

#[cfg(feature = "voice")]
impl VoiceService {
    pub(crate) fn new(config: &Config) -> Self {
        Self::with_path(config, Config::path())
    }

    #[cfg(test)]
    pub(crate) fn new_with_path(config: &Config, path: PathBuf) -> Self {
        Self::with_path(config, path)
    }

    #[cfg(test)]
    fn new_with_save(config: &Config, path: PathBuf, save: Arc<VoiceSaveFn>) -> Self {
        Self::with_shared(config, path, save)
    }

    fn with_path(config: &Config, path: PathBuf) -> Self {
        Self::with_shared(config, path, Arc::new(save_voice_snapshot))
    }

    fn with_shared(config: &Config, path: PathBuf, save: Arc<VoiceSaveFn>) -> Self {
        let (preset_id, enabled) = config.voice.effective_active_preset(&config.voice_presets);
        let preset = config.find_voice_preset(&preset_id);
        let params = preset.as_ref().map_or_else(VoiceParams::neutral, |preset| {
            params_from_config(preset.params)
        });
        let preset_name = preset
            .map(|preset| preset.name)
            .unwrap_or_else(|| preset_id.clone());
        Self {
            shared: Arc::new(VoiceShared {
                state: Mutex::new(VoiceState {
                    enabled,
                    bypassed: false,
                    preset_id,
                    preset_name,
                    saved_params: params,
                    params,
                    user_presets: config.voice_presets.clone(),
                    draft_available: false,
                    preview: VoicePreviewDto::idle(),
                    save_error: None,
                    control: None,
                    operation: 0,
                    save_generation: 0,
                    pending_write: None,
                    writer_active: false,
                }),
                writes_finished: Condvar::new(),
                config_path: path,
                save,
            }),
        }
    }

    pub(crate) fn status(&self) -> VoiceStatusDto {
        let state = self.lock();
        VoiceStatusDto {
            enabled: state.enabled,
            bypassed: state.bypassed,
            phase: state
                .control
                .as_ref()
                .map_or("off", |control| phase_name(control.phase()))
                .to_owned(),
            preset_id: state.preset_id.clone(),
            preset_name: state.preset_name.clone(),
            modified: state.params != state.saved_params,
            draft_available: state.draft_available,
            params: state.params.into(),
            preview: state.preview.clone(),
            save_error: state.save_error.clone(),
        }
    }

    pub(crate) fn presets(&self) -> Vec<VoicePresetDto> {
        let user_presets = self.lock().user_presets.clone();
        let mut presets: Vec<_> = builtin_voice_presets().iter().map(preset_dto).collect();
        let mut users: Vec<_> = user_presets.iter().map(preset_dto).collect();
        users.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.id.cmp(&right.id))
        });
        presets.append(&mut users);
        presets
    }

    /// 新会话发布后绑定控制句柄；会话重建会清除临时旁路。
    pub(crate) fn attach(&self, control: VoiceControl) {
        let mut state = self.lock();
        state.bypassed = false;
        state.control = Some(control.clone());
        control.set_params(state.params);
        if state.enabled {
            control.request_enable();
        } else {
            control.request_disable();
        }
    }

    /// 会话释放后丢弃旧句柄，旧控制不能再影响之后的命令。
    pub(crate) fn detach(&self) {
        let mut state = self.lock();
        state.control = None;
        state.bypassed = false;
    }

    /// 构造会话启动选项；关闭状态下仍保留人声链，供后续启用与试听复用。
    pub(crate) fn prepare_options(&self, block_ms: f32) -> Result<VoiceOptions> {
        validate_block_ms(block_ms)?;
        let state = self.lock();
        Ok(VoiceOptions {
            enabled: state.enabled && !state.bypassed,
            params: state.params,
            block_ms,
            backend: signalsmith_voice_backend_factory(block_ms),
        })
    }

    /// 采纳 `StartConfig` 的初始值，返回实际使用的块时长。
    ///
    /// `StartConfig` 只影响当前运行状态，不写配置；持久化仍由显式命令负责。
    pub(crate) fn apply_start_config(&self, start: &VoiceStartConfig) -> Result<f32> {
        let block_ms = start.block_ms.unwrap_or(DEFAULT_VOICE_BLOCK_MS);
        validate_block_ms(block_ms)?;
        let mut state = self.lock();
        state.operation = state.operation.wrapping_add(1);
        state.enabled = start.enabled;
        if let Some(params) = start.params {
            state.params = VoiceParams::from(params).sanitized();
        }
        Self::apply_control(&state);
        Ok(block_ms)
    }

    pub(crate) fn set_enabled(&self, enabled: bool) -> Result<VoiceStatusDto> {
        {
            let mut state = self.lock();
            state.enabled = enabled;
            state.bypassed = false;
            state.operation = state.operation.wrapping_add(1);
            Self::apply_control(&state);
            self.queue_write(&mut state);
        }
        Ok(self.status())
    }

    pub(crate) fn set_bypass(&self, bypass: bool) -> Result<VoiceStatusDto> {
        {
            let mut state = self.lock();
            state.bypassed = bypass;
            state.operation = state.operation.wrapping_add(1);
            Self::apply_control(&state);
        }
        Ok(self.status())
    }

    pub(crate) fn set_params(&self, params: VoiceParamsDto) -> Result<VoiceStatusDto> {
        let params = VoiceParams::from(params).sanitized();
        {
            let mut state = self.lock();
            state.params = params;
            state.operation = state.operation.wrapping_add(1);
            if let Some(control) = &state.control {
                control.set_params(params);
            }
        }
        Ok(self.status())
    }

    pub(crate) fn select_preset(&self, id: &str) -> Result<VoiceStatusDto> {
        let preset = builtin_voice_presets()
            .into_iter()
            .find(|preset| preset.id == id)
            .or_else(|| {
                self.lock()
                    .user_presets
                    .iter()
                    .find(|preset| preset.id == id)
                    .cloned()
            })
            .ok_or_else(|| anyhow::anyhow!("找不到变声预设「{id}」"))?;
        let params = params_from_config(preset.params);
        {
            let mut state = self.lock();
            state.preset_id = preset.id;
            state.preset_name = preset.name;
            state.saved_params = params;
            state.params = params;
            state.operation = state.operation.wrapping_add(1);
            if let Some(control) = &state.control {
                control.set_params(params);
            }
            // 选择预设只更新参数与激活项，不改变启用状态。
            self.queue_write(&mut state);
        }
        Ok(self.status())
    }

    /// 自检、校准与设备恢复开始前临时旁路；只改运行状态，不写配置。
    pub(crate) fn suspend(&self) -> VoiceSuspendState {
        let mut state = self.lock();
        let saved = VoiceSuspendState {
            enabled: state.enabled,
            bypassed: state.bypassed,
        };
        state.bypassed = true;
        state.operation = state.operation.wrapping_add(1);
        if let Some(control) = &state.control {
            control.request_disable();
        }
        saved
    }

    /// 恢复暂停前的启用与旁路状态。
    pub(crate) fn restore(&self, saved: VoiceSuspendState) {
        let mut state = self.lock();
        state.enabled = saved.enabled;
        state.bypassed = saved.bypassed;
        state.operation = state.operation.wrapping_add(1);
        Self::apply_control(&state);
    }

    fn apply_control(state: &VoiceState) {
        let Some(control) = &state.control else {
            return;
        };
        control.set_params(state.params);
        if state.enabled && !state.bypassed {
            control.request_enable();
        } else {
            control.request_disable();
        }
    }

    fn queue_write(&self, state: &mut VoiceState) {
        state.save_generation = state.save_generation.wrapping_add(1);
        state.pending_write = Some(VoiceWrite {
            generation: state.save_generation,
            enabled: state.enabled,
            preset_id: state.preset_id.clone(),
        });
        if state.writer_active {
            return;
        }
        state.writer_active = true;
        let shared = self.shared.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("voice-config".into())
            .spawn(move || run_writer(shared))
        {
            state.writer_active = false;
            state.pending_write = None;
            state.save_error = Some(format!("启动变声配置保存线程失败: {error}"));
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VoiceState> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 等待当前及已排队的状态写入完成；只用于回归测试。
    #[cfg(test)]
    pub(crate) fn flush_saves(&self) {
        let mut state = self.lock();
        while state.writer_active || state.pending_write.is_some() {
            state = self
                .shared
                .writes_finished
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// 当前操作代次；只用于回归测试验证旧结果不会回写。
    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.lock().operation
    }
}

/// 暂停前的启用与旁路状态，由调用方原样传回 [`VoiceService::restore`]。
#[cfg(feature = "voice")]
#[derive(Debug, Clone, Copy)]
pub(crate) struct VoiceSuspendState {
    enabled: bool,
    bypassed: bool,
}

#[cfg(feature = "voice")]
fn validate_block_ms(block_ms: f32) -> Result<()> {
    anyhow::ensure!(
        block_ms.is_finite() && block_ms > 0.0 && block_ms <= MAX_VOICE_BLOCK_MS,
        "变声块时长必须为 (0, 40] ms 内的有限值，实际 {block_ms}"
    );
    Ok(())
}

#[cfg(feature = "voice")]
fn run_writer(shared: Arc<VoiceShared>) {
    loop {
        let write = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match state.pending_write.take() {
                Some(write) => write,
                None => {
                    state.writer_active = false;
                    shared.writes_finished.notify_all();
                    return;
                }
            }
        };
        let result = (shared.save)(&shared.config_path, &write);
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if write.generation == state.save_generation {
            state.save_error = result
                .err()
                .map(|error| format!("保存变声配置失败: {error:#}"));
        } else if let Err(error) = result {
            log::warn!(
                "过期的变声配置保存失败（代次 {}）: {error:#}",
                write.generation
            );
        }
        shared.writes_finished.notify_all();
    }
}

/// 在进程级写入门内重读磁盘配置，只修改变声启用与激活预设后原子保存。
#[cfg(feature = "voice")]
fn save_voice_snapshot(path: &Path, write: &VoiceWrite) -> Result<()> {
    crate::host::with_config_write(|| {
        let mut config = Config::load_from(path);
        config.voice.enabled = write.enabled;
        config.voice.active_preset = write.preset_id.clone();
        config.save_to(path)
    })
}

#[cfg(all(test, feature = "voice"))]
#[path = "voice_tests.rs"]
mod tests;
