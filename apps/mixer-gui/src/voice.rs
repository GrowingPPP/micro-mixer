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
    /// 状态快照的操作代次；异步回复低于当前代次时必须丢弃。
    pub generation: u64,
}

impl VoiceStatusDto {
    /// 回复是否早于当前状态。界面按同一代次规则丢弃旧的预设列表与轮询回复。
    #[cfg(all(test, feature = "voice"))]
    pub(crate) fn is_stale_reply(&self, generation: u64) -> bool {
        generation < self.generation
    }
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
            generation: 0,
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

/// 激活预设缺失或被删除时的回退目标，与配置层保持一致。
#[cfg(feature = "voice")]
const FALLBACK_PRESET_ID: &str = "builtin:female";

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
fn params_to_config(params: VoiceParams) -> VoiceParamsConfig {
    VoiceParamsConfig {
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

/// 人声效果热键的语义动作；索引映射在宿主轮询线程里完成。
#[cfg(feature = "voice")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VoiceHotkeyAction {
    ToggleBypass,
    NextPreset,
    PrevPreset,
}

/// 一次热键动作的结果：是否真的改变了状态，以及需要提示用户的说明。
#[cfg(feature = "voice")]
#[derive(Debug, Clone)]
pub(crate) struct VoiceHotkeyOutcome {
    /// 测试与界面状态断言使用；生产热键路径只消费 `notice`。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) status: VoiceStatusDto,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) acted: bool,
    pub(crate) notice: Option<String>,
}

/// 切换预设时留在宿主内存里的未保存修改；进程退出自然丢失。
#[cfg(feature = "voice")]
#[derive(Debug, Clone)]
struct VoiceDraft {
    source_preset_id: String,
    params: VoiceParams,
}

/// 保存失败后用于恢复「未保存」判定的基线。
#[cfg(feature = "voice")]
#[derive(Debug, Clone)]
struct PendingSaveRollback {
    preset_id: String,
    saved_params: VoiceParams,
}

/// 一次待落盘的完整变声状态。
///
/// 用户预设以服务内的权威列表整体替换配置里的 `[[voice_presets]]`，
/// 而 `[voice]` 只覆盖服务拥有的字段；写线程严格重读磁盘，
/// 因此并发修改的热键与其他配置不会被旧快照抹掉。
#[cfg(feature = "voice")]
#[derive(Debug, Clone)]
struct VoiceWrite {
    generation: u64,
    enabled: bool,
    preset_id: String,
    saved_params: VoiceParams,
    rollback: Option<PendingSaveRollback>,
    user_presets: Vec<VoicePreset>,
}

#[cfg(feature = "voice")]
type VoiceSaveFn = dyn Fn(&Path, &VoiceWrite) -> Result<()> + Send + Sync;

#[cfg(feature = "voice")]
struct VoiceState {
    enabled: bool,
    bypassed: bool,
    /// 自检、校准与设备恢复的嵌套暂停层数；只叠加，不覆盖用户状态。
    suspended: u32,
    preset_id: String,
    preset_name: String,
    /// 当前预设的保存基线；用户保存后立即前移，落盘失败时回滚。
    saved_params: VoiceParams,
    params: VoiceParams,
    /// `[voice]` 中由命令维护的三个人声效果热键；启动时用于绑定。
    bypass_hotkey: String,
    next_preset_hotkey: String,
    prev_preset_hotkey: String,
    user_presets: Vec<VoicePreset>,
    draft: Option<VoiceDraft>,
    pending_save_rollback: Option<PendingSaveRollback>,
    preview: VoicePreviewDto,
    save_error: Option<String>,
    control: Option<VoiceControl>,
    /// 用户命令与运行状态变更的代次；异步保存结果据此丢弃过期回复。
    operation: u64,
    save_generation: u64,
    pending_write: Option<VoiceWrite>,
    writer_active: bool,
    /// 手机遥控路由代次；与状态写入共用同一把锁，退休后旧闭包不能写入。
    remote_epoch: u64,
}

#[cfg(feature = "voice")]
impl VoiceState {
    /// 暂停和临时旁路都只会让运行态更保守，不会改写保存的启用值。
    fn effective_enabled(&self) -> bool {
        self.enabled && !self.bypassed && self.suspended == 0
    }

    fn effective_bypassed(&self) -> bool {
        self.bypassed || self.suspended > 0
    }
}

#[cfg(feature = "voice")]
struct VoiceShared {
    state: Mutex<VoiceState>,
    writes_finished: Condvar,
    config_path: PathBuf,
    save: Arc<VoiceSaveFn>,
    /// 回归测试在“代次检查完成、状态写入之前”暂停遥控命令。
    #[cfg(test)]
    remote_pause: Mutex<Option<RemoteWritePause>>,
}

/// 让测试确定性地构造“检查后、写入前”窗口；暂停期间状态锁仍由命令持有。
#[cfg(all(test, feature = "voice"))]
struct RemoteWritePause {
    checked: std::sync::mpsc::Sender<()>,
    resume: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

/// 宿主变声状态与命令入口。
#[cfg(feature = "voice")]
#[derive(Clone)]
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
                    suspended: 0,
                    preset_id,
                    preset_name,
                    saved_params: params,
                    params,
                    bypass_hotkey: config.voice.bypass_hotkey.clone(),
                    next_preset_hotkey: config.voice.next_preset_hotkey.clone(),
                    prev_preset_hotkey: config.voice.prev_preset_hotkey.clone(),
                    user_presets: config.voice_presets.clone(),
                    draft: None,
                    pending_save_rollback: None,
                    preview: VoicePreviewDto::idle(),
                    save_error: None,
                    control: None,
                    operation: 0,
                    save_generation: 0,
                    pending_write: None,
                    writer_active: false,
                    remote_epoch: 0,
                }),
                writes_finished: Condvar::new(),
                config_path: path,
                save,
                #[cfg(test)]
                remote_pause: Mutex::new(None),
            }),
        }
    }

    /// 构造手机遥控路由；调用后先前发布的路由立即退休。
    ///
    /// 路由跨音频重启存活：待机与重建只重新发布，释放会话时才退休。
    pub(crate) fn remote_route(&self) -> remote::VoiceRoute {
        let epoch = {
            let mut state = self.lock();
            state.remote_epoch = state.remote_epoch.wrapping_add(1);
            state.remote_epoch
        };
        let state = self.clone();
        let enable = self.clone();
        let preset = self.clone();
        remote::VoiceRoute {
            state: Arc::new(move || state.remote_state()),
            set_enabled: Arc::new(move |enabled| {
                enable.set_enabled_for_remote(enabled, epoch).is_ok()
            }),
            select_preset: Arc::new(move |id| preset.select_preset_for_remote(id, epoch).is_ok()),
        }
    }

    /// 让所有已发布的路由失效；用于释放会话或关闭遥控。
    ///
    /// 递增与状态写入使用同一把锁；退休返回后，旧闭包不可能再写入状态。
    pub(crate) fn retire_remote_route(&self) {
        let mut state = self.lock();
        state.remote_epoch = state.remote_epoch.wrapping_add(1);
    }

    fn set_enabled_for_remote(&self, enabled: bool, epoch: u64) -> Result<VoiceStatusDto> {
        {
            let mut state = self.lock();
            anyhow::ensure!(state.remote_epoch == epoch, "遥控路由已过期");
            #[cfg(test)]
            self.pause_remote_write();
            Self::apply_enabled(&mut state, enabled);
            self.queue_write(&mut state);
        }
        Ok(self.status())
    }

    #[cfg(test)]
    fn pause_remote_write(&self) {
        let pause = self
            .shared
            .remote_pause
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(pause) = pause {
            let _ = pause.checked.send(());
            let _ = pause
                .resume
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .recv();
        }
    }

    fn remote_state(&self) -> remote::VoiceRemoteState {
        let status = self.status();
        remote::VoiceRemoteState {
            enabled: status.enabled,
            bypassed: status.bypassed,
            preset_id: status.preset_id,
            preset_name: status.preset_name,
            presets: self
                .presets()
                .into_iter()
                .map(|preset| (preset.id, preset.name))
                .collect(),
        }
    }

    pub(crate) fn status(&self) -> VoiceStatusDto {
        let state = self.lock();
        VoiceStatusDto {
            // 开关显示保存的启用值；暂停与临时旁路通过 bypassed 单独表达。
            enabled: state.enabled,
            bypassed: state.effective_bypassed(),
            phase: state
                .control
                .as_ref()
                .map_or("off", |control| phase_name(control.phase()))
                .to_owned(),
            preset_id: state.preset_id.clone(),
            preset_name: state.preset_name.clone(),
            modified: state.params != state.saved_params,
            draft_available: state.draft.is_some(),
            params: state.params.into(),
            preview: state.preview.clone(),
            save_error: state.save_error.clone(),
            generation: state.operation,
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

    /// 当前配置的三个人声效果热键：旁路、下一个预设、上一个预设。
    pub(crate) fn hotkey_specs(&self) -> Vec<String> {
        let state = self.lock();
        vec![
            state.bypass_hotkey.clone(),
            state.next_preset_hotkey.clone(),
            state.prev_preset_hotkey.clone(),
        ]
    }

    /// 变声配置路径；无会话时用于读取音效热键做跨组校验。
    pub(crate) fn config_path(&self) -> &Path {
        &self.shared.config_path
    }

    /// 只写入 `[voice]` 的三个热键字段，保留磁盘上的其他配置与用户预设。
    ///
    /// 先落盘再更新内存：保存失败时下一次启动仍按旧热键绑定。
    pub(crate) fn save_hotkeys(&self, bypass: &str, next: &str, prev: &str) -> Result<()> {
        let values = [
            bypass.trim().to_owned(),
            next.trim().to_owned(),
            prev.trim().to_owned(),
        ];
        crate::host::with_config_write(|| {
            let mut config = crate::host::load_config_for_write(self.config_path())?;
            config.voice.bypass_hotkey.clone_from(&values[0]);
            config.voice.next_preset_hotkey.clone_from(&values[1]);
            config.voice.prev_preset_hotkey.clone_from(&values[2]);
            config.save_to(self.config_path())
        })?;
        let mut state = self.lock();
        state.bypass_hotkey.clone_from(&values[0]);
        state.next_preset_hotkey.clone_from(&values[1]);
        state.prev_preset_hotkey.clone_from(&values[2]);
        Ok(())
    }

    /// 按热键语义执行动作。
    ///
    /// 无会话或变声关闭时旁路只返回提示，不建立临时旁路、也不启动音频；
    /// 预设热键始终更新待用参数与配置，循环不改变启用状态。
    pub(crate) fn handle_hotkey(
        &self,
        action: VoiceHotkeyAction,
        session_active: bool,
    ) -> Result<VoiceHotkeyOutcome> {
        match action {
            VoiceHotkeyAction::ToggleBypass => {
                if !session_active {
                    return Ok(VoiceHotkeyOutcome {
                        status: self.status(),
                        acted: false,
                        notice: Some("音频未启动，旁路热键不生效".to_owned()),
                    });
                }
                let status = self.status();
                if !status.enabled {
                    return Ok(VoiceHotkeyOutcome {
                        status,
                        acted: false,
                        notice: Some("变声未开启，旁路热键不生效".to_owned()),
                    });
                }
                Ok(VoiceHotkeyOutcome {
                    status: self.set_bypass(!status.bypassed)?,
                    acted: true,
                    notice: None,
                })
            }
            VoiceHotkeyAction::NextPreset => Ok(VoiceHotkeyOutcome {
                status: self.cycle_preset(true)?,
                acted: true,
                notice: None,
            }),
            VoiceHotkeyAction::PrevPreset => Ok(VoiceHotkeyOutcome {
                status: self.cycle_preset(false)?,
                acted: true,
                notice: None,
            }),
        }
    }

    /// 按界面预设顺序循环：六个内置预设按定义顺序，随后用户预设按名称后 ID。
    fn cycle_preset(&self, forward: bool) -> Result<VoiceStatusDto> {
        let (ids, current) = {
            let state = self.lock();
            let mut ids: Vec<String> = builtin_voice_presets()
                .iter()
                .map(|preset| preset.id.clone())
                .collect();
            let mut users = state.user_presets.clone();
            users.sort_by(|left, right| {
                left.name
                    .cmp(&right.name)
                    .then_with(|| left.id.cmp(&right.id))
            });
            ids.extend(users.into_iter().map(|preset| preset.id));
            (ids, state.preset_id.clone())
        };
        anyhow::ensure!(!ids.is_empty(), "没有可用的变声预设");
        let index = ids.iter().position(|id| *id == current).unwrap_or(0);
        let next = if forward {
            (index + 1) % ids.len()
        } else {
            (index + ids.len() - 1) % ids.len()
        };
        self.select_preset(&ids[next])
    }

    /// 新会话发布后绑定控制句柄；会话重建会清除临时旁路。
    pub(crate) fn attach(&self, control: VoiceControl) {
        let mut state = self.lock();
        state.bypassed = false;
        state.control = Some(control.clone());
        control.set_params(state.params);
        if state.effective_enabled() {
            control.request_enable();
        } else {
            control.request_disable();
        }
    }

    /// 会话释放后丢弃旧句柄，旧控制不能再影响之后的命令。
    ///
    /// 暂停计数属于调用方生命周期，不能随会话释放清掉；否则自检恢复后
    /// 会在暂停尚未结束时提前启用人声。
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
            enabled: state.effective_enabled(),
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
            Self::apply_enabled(&mut state, enabled);
            self.queue_write(&mut state);
        }
        Ok(self.status())
    }

    fn apply_enabled(state: &mut VoiceState, enabled: bool) {
        state.enabled = enabled;
        state.bypassed = false;
        state.operation = state.operation.wrapping_add(1);
        Self::apply_control(state);
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

    /// 切换预设；切换前的未保存修改留在内存草稿里。
    ///
    /// 选择预设只更新参数与激活项，不改变启用状态，也不清除临时旁路。
    pub(crate) fn select_preset(&self, id: &str) -> Result<VoiceStatusDto> {
        self.select_preset_inner(id, None)
    }

    fn select_preset_for_remote(&self, id: &str, epoch: u64) -> Result<VoiceStatusDto> {
        self.select_preset_inner(id, Some(epoch))
    }

    fn select_preset_inner(&self, id: &str, remote_epoch: Option<u64>) -> Result<VoiceStatusDto> {
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
            if let Some(epoch) = remote_epoch {
                anyhow::ensure!(state.remote_epoch == epoch, "遥控路由已过期");
                #[cfg(test)]
                self.pause_remote_write();
            }
            // 重复选择当前预设不能丢弃正在编辑的参数。
            if state.preset_id != preset.id {
                if state.params != state.saved_params {
                    state.draft = Some(VoiceDraft {
                        source_preset_id: state.preset_id.clone(),
                        params: state.params,
                    });
                }
                state.preset_id = preset.id;
                state.preset_name = preset.name;
                state.saved_params = params;
                state.params = params;
                state.pending_save_rollback = None;
                state.operation = state.operation.wrapping_add(1);
                if let Some(control) = &state.control {
                    control.set_params(params);
                }
                self.queue_write(&mut state);
            }
        }
        Ok(self.status())
    }

    /// 另存为用户预设：生成稳定 ID、写入完整预设列表并设为当前预设。
    pub(crate) fn save_as_user_preset(&self, name: &str) -> Result<VoiceStatusDto> {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "预设名称不能为空");
        {
            let mut state = self.lock();
            let id = Config {
                voice_presets: state.user_presets.clone(),
                ..Config::default()
            }
            .new_user_preset_id(name);
            let params = state.params;
            state.user_presets.push(VoicePreset {
                id: id.clone(),
                name: name.to_owned(),
                params: params_to_config(params),
                unknown_lines: Vec::new(),
            });
            state.pending_save_rollback = Some(PendingSaveRollback {
                preset_id: id.clone(),
                saved_params: state.saved_params,
            });
            state.preset_id = id;
            state.preset_name = name.to_owned();
            state.saved_params = params;
            state.operation = state.operation.wrapping_add(1);
            self.queue_write(&mut state);
        }
        Ok(self.status())
    }

    /// 保存当前用户预设；内置预设只能另存。
    pub(crate) fn save_preset(&self) -> Result<VoiceStatusDto> {
        {
            let mut state = self.lock();
            let id = state.preset_id.clone();
            anyhow::ensure!(
                !id.starts_with("builtin:"),
                "内置预设不能保存，请另存为用户预设"
            );
            let index = state
                .user_presets
                .iter()
                .position(|preset| preset.id == id)
                .ok_or_else(|| anyhow::anyhow!("找不到变声预设「{id}」"))?;
            let params = state.params;
            state.user_presets[index].params = params_to_config(params);
            let keeps_rollback = state
                .pending_save_rollback
                .as_ref()
                .is_some_and(|rollback| rollback.preset_id == id);
            if !keeps_rollback {
                state.pending_save_rollback = Some(PendingSaveRollback {
                    preset_id: id,
                    saved_params: state.saved_params,
                });
            }
            state.saved_params = params;
            state.operation = state.operation.wrapping_add(1);
            self.queue_write(&mut state);
        }
        Ok(self.status())
    }

    /// 只改名称，不改 ID；名称不要求唯一，排序按名称后 ID 稳定进行。
    pub(crate) fn rename_preset(&self, id: &str, name: &str) -> Result<VoiceStatusDto> {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "预设名称不能为空");
        anyhow::ensure!(!id.starts_with("builtin:"), "内置预设不能重命名");
        {
            let mut state = self.lock();
            let preset = state
                .user_presets
                .iter_mut()
                .find(|preset| preset.id == id)
                .ok_or_else(|| anyhow::anyhow!("找不到变声预设「{id}」"))?;
            preset.name = name.to_owned();
            if state.preset_id == id {
                state.preset_name = name.to_owned();
            }
            state.operation = state.operation.wrapping_add(1);
            self.queue_write(&mut state);
        }
        Ok(self.status())
    }

    /// 删除用户预设；删除当前预设时撤销草稿、旁路与试听，再回退并关闭变声。
    pub(crate) fn delete_preset(&self, id: &str) -> Result<VoiceStatusDto> {
        anyhow::ensure!(!id.starts_with("builtin:"), "内置预设不能删除");
        {
            let mut state = self.lock();
            let index = state
                .user_presets
                .iter()
                .position(|preset| preset.id == id)
                .ok_or_else(|| anyhow::anyhow!("找不到变声预设「{id}」"))?;
            state.user_presets.remove(index);
            if state.preset_id == id {
                // 先撤销试听与草稿，再关闭并回退，运行态不能保留已删除预设的痕迹。
                state.preview = VoicePreviewDto::idle();
                state.draft = None;
                state.pending_save_rollback = None;
                state.bypassed = false;
                state.enabled = false;
                let fallback = builtin_voice_presets()
                    .into_iter()
                    .find(|preset| preset.id == FALLBACK_PRESET_ID)
                    .expect("内置回退预设必须存在");
                let params = params_from_config(fallback.params);
                state.preset_id = fallback.id;
                state.preset_name = fallback.name;
                state.saved_params = params;
                state.params = params;
                state.operation = state.operation.wrapping_add(1);
                Self::apply_control(&state);
            } else {
                state.operation = state.operation.wrapping_add(1);
            }
            self.queue_write(&mut state);
        }
        Ok(self.status())
    }

    /// 恢复切换预设时留下的草稿。
    ///
    /// 源预设仍存在时切回源预设；源预设已被删除时把草稿应用到当前预设，
    /// 并保持未保存状态。草稿只存在内存里，不排队写配置。
    pub(crate) fn restore_draft(&self) -> Result<VoiceStatusDto> {
        {
            let mut state = self.lock();
            let draft = state
                .draft
                .take()
                .ok_or_else(|| anyhow::anyhow!("没有可恢复的未保存修改"))?;
            let source = builtin_voice_presets()
                .into_iter()
                .find(|preset| preset.id == draft.source_preset_id)
                .or_else(|| {
                    state
                        .user_presets
                        .iter()
                        .find(|preset| preset.id == draft.source_preset_id)
                        .cloned()
                });
            state.params = draft.params;
            state.operation = state.operation.wrapping_add(1);
            if let Some(preset) = source {
                let switched = state.preset_id != preset.id;
                state.preset_id = preset.id;
                state.preset_name = preset.name;
                state.saved_params = params_from_config(preset.params);
                state.pending_save_rollback = None;
                if switched {
                    self.queue_write(&mut state);
                }
            }
            if let Some(control) = &state.control {
                control.set_params(state.params);
            }
        }
        Ok(self.status())
    }

    /// 自检、校准与设备恢复开始前叠加一层暂停；只改运行状态，不写配置。
    ///
    /// 暂停用计数而不是快照回写：暂停期间用户仍可启用、关闭或旁路变声，
    /// 恢复时只解除这一层暂停，不能覆盖期间发生的命令。
    pub(crate) fn suspend(&self) {
        let mut state = self.lock();
        state.suspended = state.suspended.saturating_add(1);
        state.operation = state.operation.wrapping_add(1);
        Self::apply_control(&state);
    }

    /// 解除一层暂停；用户状态保持命令后的最新值。
    pub(crate) fn restore(&self) {
        let mut state = self.lock();
        state.suspended = state.suspended.saturating_sub(1);
        state.operation = state.operation.wrapping_add(1);
        Self::apply_control(&state);
    }

    fn apply_control(state: &VoiceState) {
        let Some(control) = &state.control else {
            return;
        };
        control.set_params(state.params);
        if state.effective_enabled() {
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
            saved_params: state.saved_params,
            rollback: state.pending_save_rollback.clone(),
            user_presets: state.user_presets.clone(),
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
            match result {
                Ok(()) => {
                    state.save_error = None;
                    state.saved_params = write.saved_params;
                    state.pending_save_rollback = None;
                }
                Err(error) => {
                    state.save_error = Some(format!("保存变声配置失败: {error:#}"));
                    // 只有保存命令会带基线；失败的保存不能把未保存修改显示成已保存。
                    if let Some(rollback) = &write.rollback {
                        if state.preset_id == rollback.preset_id {
                            state.saved_params = rollback.saved_params;
                        }
                    }
                }
            }
        } else if let Err(error) = result {
            log::warn!(
                "过期的变声配置保存失败（代次 {}）: {error:#}",
                write.generation
            );
        }
        shared.writes_finished.notify_all();
    }
}

/// 在进程级写入门内严格重读磁盘配置，写入服务拥有的变声字段后原子保存。
///
/// 用户预设以权威列表整体替换；`[voice]` 的热键与未知键仍来自磁盘，
/// 因此并发的非变声配置修改不会被旧快照覆盖。读取失败时保留原文件并返回错误。
#[cfg(feature = "voice")]
fn save_voice_snapshot(path: &Path, write: &VoiceWrite) -> Result<()> {
    crate::host::with_config_write(|| {
        let mut config = crate::host::load_config_for_write(path)?;
        config.voice.enabled = write.enabled;
        config.voice.active_preset = write.preset_id.clone();
        config.voice_presets.clone_from(&write.user_presets);
        config.save_to(path)
    })
}

#[cfg(all(test, feature = "voice"))]
#[path = "voice_tests.rs"]
mod tests;
