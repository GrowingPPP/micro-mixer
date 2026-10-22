use super::*;

use crate::host::route_voice_hotkey;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

struct TemporaryDir(PathBuf);

impl TemporaryDir {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "mixer-voice-regression-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TemporaryDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config_path(dir: &TemporaryDir) -> PathBuf {
    dir.0.join("config.toml")
}

fn service(dir: &TemporaryDir) -> VoiceService {
    VoiceService::new_with_path(&Config::default(), config_path(dir))
}

fn preset(id: &str, name: &str, pitch_semitones: f32) -> app_config::VoicePreset {
    app_config::VoicePreset {
        id: id.into(),
        name: name.into(),
        params: app_config::VoiceParamsConfig {
            pitch_semitones,
            ..app_config::VoiceParamsConfig::neutral()
        },
        unknown_lines: Vec::new(),
    }
}

fn user_config() -> Config {
    let mut config = Config::default();
    config.voice.enabled = true;
    config.voice.active_preset = "builtin:male".into();
    config.voice.bypass_hotkey = "F9".into();
    config.voice.next_preset_hotkey = "F10".into();
    config.voice.prev_preset_hotkey = "F8".into();
    config.voice_presets.push(app_config::VoicePreset {
        id: "user:test-abcdef".into(),
        name: "测试预设".into(),
        params: app_config::VoiceParamsConfig {
            pitch_semitones: 5.0,
            ..app_config::VoiceParamsConfig::neutral()
        },
        unknown_lines: Vec::new(),
    });
    config
}

#[test]
fn 变声启用与关闭更新运行状态并持久化() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());

    let status = service.set_enabled(true).unwrap();
    assert!(status.enabled);
    assert_eq!(status.phase, "warmup");
    assert_eq!(control.phase(), VoicePhase::Warmup);

    service.flush_saves();
    let saved = Config::load_from(&config_path(&dir));
    assert!(saved.voice.enabled);
    assert_eq!(saved.voice.active_preset, "builtin:female");

    let status = service.set_enabled(false).unwrap();
    assert!(!status.enabled);
    assert_eq!(status.phase, "off");
    assert_eq!(control.phase(), VoicePhase::Off);

    service.flush_saves();
    assert!(!Config::load_from(&config_path(&dir)).voice.enabled);
}

#[test]
fn 变声临时旁路不改变启用配置() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());
    service.set_enabled(true).unwrap();
    service.flush_saves();

    let status = service.set_bypass(true).unwrap();
    assert!(status.bypassed);
    assert!(status.enabled, "临时旁路只改变运行状态");
    assert_eq!(status.phase, "off");
    service.flush_saves();
    assert!(Config::load_from(&config_path(&dir)).voice.enabled);

    let status = service.set_bypass(false).unwrap();
    assert!(!status.bypassed);
    assert_eq!(status.phase, "warmup");
    assert_eq!(control.phase(), VoicePhase::Warmup);
}

#[test]
fn 变声选择预设只加载参数不启用变声() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());

    let status = service.select_preset("builtin:male").unwrap();
    assert!(!status.enabled, "选择预设不能自动启用变声");
    assert_eq!(status.preset_id, "builtin:male");
    assert_eq!(status.preset_name, "男声");
    assert_eq!(status.params.pitch_semitones, -4.0);
    assert_eq!(control.params().pitch_semitones, -4.0);
    assert_eq!(control.phase(), VoicePhase::Off);

    service.flush_saves();
    let saved = Config::load_from(&config_path(&dir));
    assert!(!saved.voice.enabled);
    assert_eq!(saved.voice.active_preset, "builtin:male");
}

#[test]
fn 变声参数命令先钳制再发布() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());

    let status = service
        .set_params(VoiceParamsDto {
            pitch_semitones: 99.0,
            formant_semitones: f32::NAN,
            gate_db: -100.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();

    assert_eq!(status.params.pitch_semitones, 24.0);
    assert_eq!(status.params.formant_semitones, 0.0);
    assert_eq!(status.params.gate_db, -60.0);
    assert_eq!(control.params().pitch_semitones, 24.0);
    assert_eq!(control.params().formant_semitones, 0.0);
}

#[test]
fn 变声旧保存结果不能覆盖新状态() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let entered = Mutex::new(entered_tx);
    let release = Mutex::new(release_rx);
    let service = VoiceService::new_with_save(
        &Config::default(),
        path.clone(),
        Arc::new(move |path: &Path, write: &VoiceWrite| {
            if write.generation == 1 {
                entered.lock().unwrap().send(()).unwrap();
                release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            save_voice_snapshot(path, write)
        }),
    );

    let old_generation = service.generation();
    service.set_enabled(true).unwrap();
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("第一次保存应开始");
    let newer = service.set_enabled(false).unwrap();
    let new_generation = service.generation();
    assert!(new_generation > old_generation, "命令必须推进操作代次");

    release_tx.send(()).unwrap();
    service.flush_saves();

    let status = service.status();
    assert!(!status.enabled, "旧保存返回后不得恢复旧启用状态");
    assert_eq!(status.phase, "off");
    assert!(!newer.enabled);
    assert_eq!(service.generation(), new_generation);
    assert!(status.save_error.is_none());
    assert!(!Config::load_from(&path).voice.enabled);
}

#[test]
fn 变声保存失败保留运行参数并报告未保存() {
    let dir = TemporaryDir::new();
    let blocker = dir.0.join("blocker");
    std::fs::write(&blocker, "not a directory").unwrap();
    let path = blocker.join("config.toml");
    let service = VoiceService::new_with_path(&Config::default(), path.clone());
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());

    service
        .set_params(VoiceParamsDto {
            pitch_semitones: 6.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();
    service.set_enabled(true).unwrap();
    service.flush_saves();

    let status = service.status();
    assert!(status.enabled);
    assert_eq!(status.params.pitch_semitones, 6.0);
    assert_eq!(control.params().pitch_semitones, 6.0);
    assert!(
        status
            .save_error
            .as_deref()
            .is_some_and(|error| error.contains("保存变声配置失败")),
        "保存失败必须在状态里报告，实际 {:?}",
        status.save_error
    );
    assert!(!path.exists());
}

#[test]
fn 变声连续切换预设只落盘最后一次() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let entered = Mutex::new(entered_tx);
    let release = Mutex::new(release_rx);
    let writes = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded = writes.clone();
    let service = VoiceService::new_with_save(
        &Config::default(),
        path.clone(),
        Arc::new(move |path: &Path, write: &VoiceWrite| {
            recorded.lock().unwrap().push(write.preset_id.clone());
            if write.generation == 1 {
                entered.lock().unwrap().send(()).unwrap();
                release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            save_voice_snapshot(path, write)
        }),
    );

    service.select_preset("builtin:male").unwrap();
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("第一次保存应开始");
    service.select_preset("builtin:child").unwrap();
    service.select_preset("builtin:deep").unwrap();
    release_tx.send(()).unwrap();
    service.flush_saves();

    let saved = Config::load_from(&path);
    assert_eq!(saved.voice.active_preset, "builtin:deep");
    let writes = writes.lock().unwrap();
    assert_eq!(writes.last().map(String::as_str), Some("builtin:deep"));
    assert!(
        writes.len() <= 2,
        "连续操作必须合并待写快照，实际写入 {writes:?}"
    );
}

#[test]
fn 变声显式关闭再开启清除临时旁路() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());

    service.set_enabled(true).unwrap();
    assert!(service.set_bypass(true).unwrap().bypassed);
    assert_eq!(control.phase(), VoicePhase::Off);

    let status = service.set_enabled(false).unwrap();
    assert!(!status.bypassed, "显式关闭应清除临时旁路");

    let status = service.set_enabled(true).unwrap();
    assert!(!status.bypassed);
    assert!(status.enabled);
    assert_eq!(control.phase(), VoicePhase::Warmup);
}

#[test]
fn 变声自检暂停期间回到干声并在结束后恢复() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());
    service.set_enabled(true).unwrap();
    service.flush_saves();

    service.suspend();
    assert!(service.status().bypassed);
    assert_eq!(control.phase(), VoicePhase::Off);
    service.flush_saves();
    assert!(
        Config::load_from(&config_path(&dir)).voice.enabled,
        "自检暂停不得改写启用配置"
    );

    service.restore();
    let status = service.status();
    assert!(!status.bypassed);
    assert!(status.enabled);
    assert_eq!(control.phase(), VoicePhase::Warmup);
    service.flush_saves();
    assert!(Config::load_from(&config_path(&dir)).voice.enabled);
}

#[test]
fn 变声暂停期间命令不被恢复覆盖() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());
    service.set_enabled(true).unwrap();

    service.suspend();
    service.set_enabled(false).unwrap();
    assert!(
        service.status().bypassed,
        "暂停层仍在时即使显式命令清除旁路也应保持干声"
    );

    service.restore();
    let status = service.status();
    assert!(!status.enabled, "恢复不能把暂停期间的关闭命令改回启用");
    assert!(!status.bypassed);
    assert_eq!(control.phase(), VoicePhase::Off);
    service.flush_saves();
    assert!(
        !Config::load_from(&config_path(&dir)).voice.enabled,
        "暂停期间的关闭命令必须保持落盘结果"
    );
}

#[test]
fn 变声多层暂停逐层恢复() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());
    service.set_enabled(true).unwrap();

    service.suspend();
    service.suspend();
    service.restore();
    assert!(service.status().bypassed, "还有一层暂停时不得提前启用");
    assert_eq!(control.phase(), VoicePhase::Off);

    service.restore();
    let status = service.status();
    assert!(!status.bypassed);
    assert_eq!(control.phase(), VoicePhase::Warmup);
}

const CALIBRATION_LEASE: Duration = Duration::from_secs(30);

#[test]
fn 校准暂停让变声回到干声且不改启用状态与配置() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());
    service.set_enabled(true).unwrap();
    service.flush_saves();
    assert_eq!(control.phase(), VoicePhase::Warmup);

    service.begin_calibration(CALIBRATION_LEASE);
    assert_eq!(control.phase(), VoicePhase::Off, "校准读取的必须是干声");
    let status = service.status();
    assert!(status.enabled, "校准不改写启用状态");
    assert!(status.bypassed, "暂停期间界面显示旁路，因为输出确实是干声");
    service.flush_saves();
    assert!(
        Config::load_from(&config_path(&dir)).voice.enabled,
        "校准暂停不得改写启用配置"
    );

    service.end_calibration();
    let status = service.status();
    assert!(status.enabled && !status.bypassed);
    assert_eq!(
        control.phase(),
        VoicePhase::Warmup,
        "结束后恢复到校准前的启用状态"
    );
}

#[test]
fn 校准暂停独立于用户临时旁路() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());
    service.set_enabled(true).unwrap();
    service.set_bypass(true).unwrap();

    service.begin_calibration(CALIBRATION_LEASE);
    service.end_calibration();

    let status = service.status();
    assert!(
        status.bypassed,
        "结束校准只解除自己那一层，不能清除用户的临时旁路"
    );
    assert_eq!(control.phase(), VoicePhase::Off);
    service.set_bypass(false).unwrap();
    assert_eq!(control.phase(), VoicePhase::Warmup);
}

#[test]
fn 校准暂停期间的用户命令不被结束覆盖() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());
    service.set_enabled(true).unwrap();

    service.begin_calibration(CALIBRATION_LEASE);
    service.set_enabled(false).unwrap();
    assert_eq!(control.phase(), VoicePhase::Off);
    service.select_preset("builtin:male").unwrap();

    service.end_calibration();
    let status = service.status();
    assert!(!status.enabled, "结束不能把暂停期间的关闭命令改回启用");
    assert_eq!(status.preset_id, "builtin:male");
    assert_eq!(control.phase(), VoicePhase::Off);
    service.flush_saves();
    let saved = Config::load_from(&config_path(&dir));
    assert!(!saved.voice.enabled);
    assert_eq!(saved.voice.active_preset, "builtin:male");
}

#[test]
fn 校准暂停重复开始只刷新租约重复结束无操作() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());
    service.set_enabled(true).unwrap();

    service.begin_calibration(CALIBRATION_LEASE);
    service.begin_calibration(CALIBRATION_LEASE);
    service.end_calibration();
    assert_eq!(
        control.phase(),
        VoicePhase::Warmup,
        "重复开始不能叠成两层，一次结束就应解除"
    );

    let generation = service.generation();
    service.end_calibration();
    assert_eq!(service.generation(), generation, "没有在校准时结束应无操作");
    assert_eq!(control.phase(), VoicePhase::Warmup);
}

#[test]
fn 校准暂停租约到期自动恢复() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());
    service.set_enabled(true).unwrap();

    // 界面在校准中刷新或崩溃时不会再调用结束；租约到期后变声必须自己回来。
    service.begin_calibration(Duration::from_millis(40));
    assert_eq!(control.phase(), VoicePhase::Off);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while service.status().bypassed && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = service.status();
    assert!(!status.bypassed, "租约到期后必须自动解除暂停");
    assert!(status.enabled);
    assert_eq!(control.phase(), VoicePhase::Warmup);
}

#[test]
fn 校准暂停与自检暂停叠加后逐层解除() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());
    service.set_enabled(true).unwrap();

    service.suspend();
    service.begin_calibration(CALIBRATION_LEASE);
    service.restore();
    assert!(service.status().bypassed, "校准层还在时不得提前启用");
    assert_eq!(control.phase(), VoicePhase::Off);

    service.end_calibration();
    assert!(!service.status().bypassed);
    assert_eq!(control.phase(), VoicePhase::Warmup);
}

#[test]
fn 校准暂停取消进行中的试听() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    service.attach(VoiceControl::new(VoiceParams::neutral()));
    service.set_enabled(true).unwrap();
    service.preview_begin();
    assert_eq!(service.status().preview.state, "preparing");

    service.begin_calibration(CALIBRATION_LEASE);
    assert_eq!(
        service.status().preview.state,
        "idle",
        "暂停会改变人声路径，进行中的试听不能跨阶段继续"
    );
}

#[test]
fn 校准暂停期间会话重建仍保持干声直到结束() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    service.attach(VoiceControl::new(VoiceParams::neutral()));
    service.set_enabled(true).unwrap();

    service.begin_calibration(CALIBRATION_LEASE);
    // 看门狗或设备恢复在校准期间重启音频：新会话不能把人声路径重新打开。
    service.detach();
    let rebuilt = VoiceControl::new(VoiceParams::neutral());
    service.attach(rebuilt.clone());
    assert_eq!(rebuilt.phase(), VoicePhase::Off);
    assert!(service.status().bypassed);

    service.end_calibration();
    assert_eq!(rebuilt.phase(), VoicePhase::Warmup);
}

#[test]
fn 自检暂停重建会话保留临时旁路普通重建清除() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    service.attach(VoiceControl::new(VoiceParams::neutral()));
    service.set_enabled(true).unwrap();
    service.set_bypass(true).unwrap();

    service.suspend();
    service.detach();
    let rebuilt = VoiceControl::new(VoiceParams::neutral());
    service.attach(rebuilt.clone());
    service.restore();
    let status = service.status();
    assert!(
        status.bypassed,
        "自检暂停期间重建会话必须保留用户的临时旁路"
    );
    assert_eq!(rebuilt.phase(), VoicePhase::Off);

    service.detach();
    let restarted = VoiceControl::new(VoiceParams::neutral());
    service.attach(restarted.clone());
    assert!(
        !service.status().bypassed,
        "没有暂停层的会话重启清除临时旁路"
    );
    assert_eq!(restarted.phase(), VoicePhase::Warmup);
}

#[test]
fn 变声非法块时长返回命令错误且不改变状态() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let before = service.status();
    let start = VoiceStartConfig {
        enabled: true,
        params: None,
        block_ms: None,
    };
    for block_ms in [f32::NAN, f32::INFINITY, 0.0, -1.0, 40.1] {
        assert!(service.prepare_options(block_ms).is_err());
        let invalid = VoiceStartConfig {
            block_ms: Some(block_ms),
            ..start.clone()
        };
        assert!(service.apply_start_config(&invalid).is_err());
    }
    let status = service.status();
    assert_eq!(status.enabled, before.enabled);
    assert_eq!(status.params, before.params);
    assert!(service
        .prepare_options(crate::voice::DEFAULT_VOICE_BLOCK_MS)
        .is_ok());
}

#[test]
fn 变声启动值缺省沿用配置中的启用与预设() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    let mut config = Config::default();
    config.voice.enabled = true;
    config.voice.active_preset = "builtin:male".into();
    config.save_to(&path).unwrap();

    let service = VoiceService::new_with_path(&Config::load_from(&path), path);
    let options = service
        .prepare_options(crate::voice::DEFAULT_VOICE_BLOCK_MS)
        .unwrap();
    assert!(options.enabled);
    assert_eq!(options.params.pitch_semitones, -4.0);
    let status = service.status();
    assert_eq!(status.preset_id, "builtin:male");
    assert_eq!(status.preset_name, "男声");
}

#[test]
fn 变声完整保存保留磁盘变声配置与用户预设() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    let config = user_config();
    config.save_to(&path).unwrap();

    crate::save_config_at(
        &path,
        Vec::new(),
        false,
        None,
        None,
        String::new(),
        None,
        None,
        20,
        0.0,
        -6.0,
        String::new(),
    )
    .unwrap();

    let saved = Config::load_from(&path);
    assert_eq!(saved.voice, config.voice);
    assert_eq!(saved.voice_presets, config.voice_presets);
}

#[test]
fn 变声配置读取失败不写盘并报告错误() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    let original = vec![0xff, 0xfe, 0x00, b'['];
    std::fs::write(&path, &original).unwrap();

    let service = VoiceService::new_with_path(&Config::default(), path.clone());
    service.set_enabled(true).unwrap();
    service.flush_saves();

    let status = service.status();
    assert!(status.enabled);
    assert!(
        status
            .save_error
            .as_deref()
            .is_some_and(|error| error.contains("读取配置文件失败")),
        "读取失败必须进入保存错误，实际 {:?}",
        status.save_error
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        original,
        "读取失败时不得用默认配置覆盖原文件"
    );

    let error = crate::save_config_at(
        &path,
        Vec::new(),
        false,
        None,
        None,
        String::new(),
        None,
        None,
        20,
        0.0,
        -6.0,
        String::new(),
    )
    .unwrap_err();
    assert!(error.contains("读取配置文件失败"));
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

/// 与界面「保存设置」相同的入口：不带变声字段，只覆盖音效库与连接设置。
fn full_save(path: &Path) -> Result<(), String> {
    crate::save_config_at(
        path,
        Vec::new(),
        false,
        None,
        None,
        String::new(),
        None,
        None,
        20,
        0.0,
        -6.0,
        String::new(),
    )
}

/// 音效库、变声节与用户预设俱全，只有一处非法反斜杠转义。
const INVALID_TOML_LIBRARY: &str = concat!(
    "[[sounds]]\n",
    "id = \"s1\"\n",
    "path = \"C:\\Users\\Doe\\a.wav\"\n",
    "name = \"笑声\"\n",
    "\n",
    "[voice]\n",
    "enabled = false\n",
    "active_preset = \"user:mine-abc123\"\n",
    "bypass_hotkey = \"F9\"\n",
    "\n",
    "[[voice_presets]]\n",
    "id = \"user:mine-abc123\"\n",
    "name = \"我的声音\"\n",
    "pitch_semitones = 3.0\n",
);

#[test]
fn 配置不是有效_toml_时所有写入路径都拒绝且保留原文件() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    std::fs::write(&path, INVALID_TOML_LIBRARY).unwrap();

    // 变声后台写入：无需用户显式保存就会触发，必须报告「未保存」而不是改写文件。
    let service = VoiceService::new_with_path(&Config::load_from(&path), path.clone());
    service.set_enabled(true).unwrap();
    service.select_preset("builtin:male").unwrap();
    service.flush_saves();
    let status = service.status();
    assert!(status.enabled, "写入失败时运行期状态仍然生效");
    let error = status.save_error.expect("拒绝写入必须进入保存错误");
    assert!(error.contains("不是有效 TOML"), "{error}");

    // 界面完整保存与热键保存走同一严格读取。
    let error = full_save(&path).unwrap_err();
    assert!(error.contains("不是有效 TOML"), "{error}");
    let before = service.hotkey_specs();
    let error = service.save_hotkeys("F1", "F2", "F3").unwrap_err();
    assert!(format!("{error:#}").contains("不是有效 TOML"));
    assert_eq!(service.hotkey_specs(), before, "落盘失败不得更新内存热键");

    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        INVALID_TOML_LIBRARY,
        "音效库与用户预设不能被降级成注释或默认值"
    );
}

#[test]
fn 被拒绝的预设不会被后台写入静默删除_修复后恢复保存() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    // 第二条预设缺少 user: 前缀，加载时被拒绝；其余条目有效。
    let broken = concat!(
        "[voice]\n",
        "enabled = false\n",
        "active_preset = \"user:good\"\n",
        "\n",
        "[[voice_presets]]\n",
        "id = \"user:good\"\n",
        "name = \"完好\"\n",
        "pitch_semitones = 2.0\n",
        "\n",
        "[[voice_presets]]\n",
        "id = \"handwritten\"\n",
        "name = \"手写条目\"\n",
        "pitch_semitones = 5.0\n",
        "\n",
        "[[sounds]]\n",
        "id = \"s1\"\n",
        "path = \"/a.wav\"\n",
    );
    std::fs::write(&path, broken).unwrap();

    let loaded = Config::load_from(&path);
    assert!(
        loaded
            .voice_warnings
            .iter()
            .any(|w| w.contains("handwritten")),
        "启动读取必须产生警告：{:?}",
        loaded.voice_warnings
    );
    let service = VoiceService::new_with_path(&loaded, path.clone());
    service.set_enabled(true).unwrap();
    service.flush_saves();
    let error = service
        .status()
        .save_error
        .expect("拒绝写入必须进入保存错误");
    assert!(
        error.contains("handwritten") && error.contains("拒绝写入"),
        "{error}"
    );
    assert!(full_save(&path).unwrap_err().contains("handwritten"));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        broken,
        "被拒绝的手写条目必须仍在文件里"
    );

    // 用户修复条目后，下一次命令恢复正常落盘并清除错误。
    let fixed = broken.replace("id = \"handwritten\"", "id = \"user:handwritten\"");
    std::fs::write(&path, &fixed).unwrap();
    service.set_enabled(false).unwrap();
    service.flush_saves();
    assert!(service.status().save_error.is_none());
    let saved = Config::load_for_write_from(&path).unwrap();
    assert_eq!(saved.voice_presets.len(), 2, "两条预设都应保留");
    assert!(saved.find_voice_preset("user:handwritten").is_some());
    assert_eq!(saved.sound_entries().len(), 1);
}

#[test]
fn 载入配置带出配置提示与保存阻塞标记() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);

    let clean = crate::config_dto_with_notices(Config::load_report_from(&path));
    assert_eq!(clean["config_notices"], serde_json::json!([]));
    assert_eq!(clean["config_write_blocked"], serde_json::json!(false));

    std::fs::write(&path, INVALID_TOML_LIBRARY).unwrap();
    let invalid = crate::config_dto_with_notices(Config::load_report_from(&path));
    assert_eq!(invalid["config_write_blocked"], serde_json::json!(true));
    let notices = invalid["config_notices"].as_array().unwrap();
    assert!(
        notices[0].as_str().unwrap().contains("不是有效 TOML"),
        "{notices:?}"
    );

    std::fs::write(
        &path,
        "[[voice_presets]]\nid = \"mine\"\nname = \"缺前缀\"\n",
    )
    .unwrap();
    let rejected = crate::config_dto_with_notices(Config::load_report_from(&path));
    assert_eq!(rejected["config_write_blocked"], serde_json::json!(true));
    let text = rejected["config_notices"].to_string();
    assert!(
        text.contains("缺少 user: 前缀") && text.contains("拒绝"),
        "{text}"
    );
}

#[test]
fn 保存发生_panic_时写线程不卡死并报告未保存_之后恢复() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let save: Arc<VoiceSaveFn> = Arc::new(move |path, write| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("注入的保存 panic");
        }
        let mut config = Config::load_for_write_from(path)?;
        config.voice.enabled = write.enabled;
        config.save_to(path)
    });
    let service = VoiceService::new_with_save(&Config::default(), path.clone(), save);

    service.set_enabled(true).unwrap();
    // 修复前这里会永久等待：写线程 panic 退出，`writer_active` 一直为真。
    let (done_tx, done_rx) = mpsc::channel();
    let waiter = {
        let service = service.clone();
        std::thread::spawn(move || {
            service.flush_saves();
            done_tx.send(()).unwrap();
        })
    };
    assert!(
        done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
        "保存 panic 后写线程必须收尾，flush 不得无限等待"
    );
    waiter.join().unwrap();

    let status = service.status();
    assert!(status.enabled, "保存失败时运行期状态仍然生效");
    let error = status.save_error.expect("panic 必须进入保存错误");
    assert!(
        error.contains("注入的保存 panic") && error.contains("内部错误"),
        "{error}"
    );
    assert!(!path.exists(), "panic 的那次保存没有写出任何内容");

    // 写线程仍可用：下一次命令照常落盘并清除错误。
    service.set_enabled(false).unwrap();
    service.set_enabled(true).unwrap();
    service.flush_saves();
    assert!(
        service.status().save_error.is_none(),
        "{:?}",
        service.status().save_error
    );
    assert!(Config::load_from(&path).voice.enabled);
    assert!(calls.load(Ordering::SeqCst) >= 2);
}

#[test]
fn 变声配置缺失时按默认值创建() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    assert!(!path.exists());

    let service = VoiceService::new_with_path(&Config::default(), path.clone());
    service.set_enabled(true).unwrap();
    service.flush_saves();

    assert!(service.status().save_error.is_none());
    assert!(path.exists());
    assert!(Config::load_from(&path).voice.enabled);
}

#[test]
fn 变声持久化与完整保存共用写入门() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let holder = std::thread::spawn(move || {
        crate::host::with_config_write(|| {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok::<(), String>(())
        })
        .unwrap();
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    let service = VoiceService::new_with_path(&Config::default(), path.clone());
    service.set_enabled(true).unwrap();
    std::thread::sleep(Duration::from_millis(30));
    assert!(!path.exists(), "变声保存必须等待其他配置写入完成，不能交错");

    release_tx.send(()).unwrap();
    holder.join().unwrap();
    service.flush_saves();
    assert!(Config::load_from(&path).voice.enabled);
}

// ─────────────────────────────────────────── 用户预设与草稿

#[test]
fn 切换预设保留未保存修改为草稿() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    service
        .set_params(VoiceParamsDto {
            pitch_semitones: 6.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();

    let status = service.select_preset("builtin:male").unwrap();
    assert_eq!(status.preset_id, "builtin:male");
    assert_eq!(status.params.pitch_semitones, -4.0);
    assert!(status.draft_available, "切换前有未保存修改时必须留下草稿");
    assert!(!status.modified);

    let status = service.restore_draft().unwrap();
    assert_eq!(status.preset_id, "builtin:female");
    assert_eq!(status.preset_name, "女声");
    assert_eq!(status.params.pitch_semitones, 6.0);
    assert!(status.modified, "恢复的草稿相对源预设仍是未保存修改");
    assert!(!status.draft_available);
}

#[test]
fn 新修改替换旧草稿() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    service
        .set_params(VoiceParamsDto {
            pitch_semitones: 5.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();
    service.select_preset("builtin:male").unwrap();

    service
        .set_params(VoiceParamsDto {
            pitch_semitones: -7.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();
    service.select_preset("builtin:child").unwrap();

    let status = service.restore_draft().unwrap();
    assert_eq!(status.preset_id, "builtin:male");
    assert_eq!(status.params.pitch_semitones, -7.0);
    assert!(!status.draft_available);
}

#[test]
fn 内置预设不能直接保存() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    let original = Config::default();
    original.save_to(&path).unwrap();
    let service = VoiceService::new_with_path(&original, path.clone());
    service
        .set_params(VoiceParamsDto {
            pitch_semitones: 6.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();

    let error = service.save_preset().unwrap_err().to_string();
    assert!(
        error.contains("请另存为用户预设"),
        "内置预设保存必须提示另存，实际 {error}"
    );
    assert_eq!(service.status().params.pitch_semitones, 6.0);

    service.flush_saves();
    let saved = Config::load_from(&path);
    assert_eq!(saved.voice, original.voice);
    assert!(saved.voice_presets.is_empty());
}

#[test]
fn 另存为用户预设后成为当前预设() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    let service = service(&dir);
    service
        .set_params(VoiceParamsDto {
            pitch_semitones: 6.0,
            formant_semitones: 2.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();

    let status = service.save_as_user_preset("  我的预设  ").unwrap();
    assert!(status.preset_id.starts_with("user:"));
    assert_eq!(status.preset_name, "我的预设");
    assert!(!status.modified);
    assert!(service.presets().iter().any(|p| p.id == status.preset_id));

    service.flush_saves();
    let saved = Config::load_from(&path);
    assert_eq!(saved.voice.active_preset, status.preset_id);
    let preset = saved
        .voice_presets
        .iter()
        .find(|preset| preset.id == status.preset_id)
        .expect("用户预设必须写入配置");
    assert_eq!(preset.name, "我的预设");
    assert_eq!(preset.params.pitch_semitones, 6.0);
    assert_eq!(preset.params.formant_semitones, 2.0);
    assert_eq!(
        VoiceParamsDto::from(params_from_config(preset.params)),
        status.params
    );
    assert!(
        service.save_as_user_preset("   ").is_err(),
        "空名称必须被拒绝"
    );
}

#[test]
fn 保存修改更新用户预设() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    let config = Config {
        voice: app_config::VoiceConfig {
            enabled: true,
            active_preset: "user:alpha-000001".into(),
            ..app_config::VoiceConfig::default()
        },
        voice_presets: vec![preset("user:alpha-000001", "甲", 3.0)],
        ..Config::default()
    };
    let service = VoiceService::new_with_path(&config, path.clone());
    service
        .set_params(VoiceParamsDto {
            pitch_semitones: 9.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();
    assert!(service.status().modified);

    let status = service.save_preset().unwrap();
    assert_eq!(status.preset_id, "user:alpha-000001");
    assert_eq!(status.preset_name, "甲");
    assert!(!status.modified);
    service.flush_saves();

    let status = service.status();
    assert!(status.save_error.is_none());
    assert!(!status.modified);
    let saved = Config::load_from(&path);
    assert_eq!(saved.voice.active_preset, "user:alpha-000001");
    assert_eq!(saved.voice_presets[0].params.pitch_semitones, 9.0);
}

#[test]
fn 草稿源预设删除后应用到当前预设() {
    let dir = TemporaryDir::new();
    let config = Config {
        voice: app_config::VoiceConfig {
            active_preset: "user:alpha-000001".into(),
            ..app_config::VoiceConfig::default()
        },
        voice_presets: vec![
            preset("user:alpha-000001", "甲", 3.0),
            preset("user:beta-000002", "乙", -3.0),
        ],
        ..Config::default()
    };
    let service = VoiceService::new_with_path(&config, config_path(&dir));
    service
        .set_params(VoiceParamsDto {
            pitch_semitones: 8.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();
    service.select_preset("user:beta-000002").unwrap();
    service.delete_preset("user:alpha-000001").unwrap();
    assert!(service.status().draft_available);

    let status = service.restore_draft().unwrap();
    assert_eq!(status.preset_id, "user:beta-000002");
    assert_eq!(status.params.pitch_semitones, 8.0);
    assert!(status.modified, "源预设删除后恢复的草稿仍属未保存修改");
    assert!(!status.draft_available);
}

#[test]
fn 重命名保持稳定预设标识() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    let service = service(&dir);
    let created = service.save_as_user_preset("旧名称").unwrap();
    let id = created.preset_id;
    service.flush_saves();

    let renamed = service.rename_preset(&id, "  新名称  ").unwrap();
    assert_eq!(renamed.preset_id, id, "重命名不能改变 ID");
    assert_eq!(renamed.preset_name, "新名称");
    service.flush_saves();

    let saved = Config::load_from(&path);
    let preset = saved
        .voice_presets
        .iter()
        .find(|preset| preset.id == id)
        .expect("重命名后预设仍在配置中");
    assert_eq!(preset.name, "新名称");
    assert_eq!(saved.voice.active_preset, id);

    let selected = service.select_preset(&id).unwrap();
    assert_eq!(selected.preset_id, id);
    assert_eq!(selected.preset_name, "新名称");
    assert!(
        service.rename_preset(&id, "   ").is_err(),
        "空名称必须被拒绝"
    );
}

#[test]
fn 删除当前用户预设回退女声并关闭() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    let mut config = Config::default();
    config.voice.enabled = true;
    config.voice.active_preset = "user:alpha-000001".into();
    config.voice_presets = vec![
        preset("user:alpha-000001", "甲", 3.0),
        preset("user:beta-000002", "乙", -3.0),
    ];
    let service = VoiceService::new_with_path(&config, path.clone());
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());

    service
        .set_params(VoiceParamsDto {
            pitch_semitones: 8.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();
    service.select_preset("user:beta-000002").unwrap();
    service.select_preset("user:alpha-000001").unwrap();
    assert!(service.status().draft_available);

    let status = service.delete_preset("user:alpha-000001").unwrap();
    assert_eq!(status.preset_id, "builtin:female");
    assert_eq!(status.preset_name, "女声");
    assert!(!status.enabled);
    assert!(!status.bypassed);
    assert!(!status.draft_available, "删除当前预设必须清除草稿");
    assert_eq!(status.params.pitch_semitones, 4.0);
    assert_eq!(control.params().pitch_semitones, 4.0);
    assert_eq!(control.phase(), VoicePhase::Off);

    service.flush_saves();
    let saved = Config::load_from(&path);
    assert!(!saved.voice.enabled);
    assert_eq!(saved.voice.active_preset, "builtin:female");
    assert_eq!(saved.voice_presets.len(), 1);
    assert_eq!(saved.voice_presets[0].id, "user:beta-000002");
}

#[test]
fn 删除非当前用户预设不影响当前状态() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    let mut config = Config::default();
    config.voice.enabled = true;
    config.voice.active_preset = "user:alpha-000001".into();
    config.voice_presets = vec![
        preset("user:alpha-000001", "甲", 3.0),
        preset("user:beta-000002", "乙", -3.0),
    ];
    let service = VoiceService::new_with_path(&config, path.clone());

    service
        .set_params(VoiceParamsDto {
            pitch_semitones: 8.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();
    service.select_preset("user:beta-000002").unwrap();
    service.select_preset("user:alpha-000001").unwrap();
    service
        .set_params(VoiceParamsDto {
            pitch_semitones: 9.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();
    let before = service.status();
    assert!(before.draft_available);

    let after = service.delete_preset("user:beta-000002").unwrap();
    assert_eq!(after.preset_id, before.preset_id);
    assert_eq!(after.preset_name, before.preset_name);
    assert_eq!(after.params, before.params);
    assert_eq!(after.enabled, before.enabled);
    assert_eq!(after.bypassed, before.bypassed);
    assert_eq!(after.draft_available, before.draft_available);
    assert_eq!(after.modified, before.modified);

    service.flush_saves();
    let saved = Config::load_from(&path);
    assert_eq!(saved.voice_presets.len(), 1);
    assert_eq!(saved.voice_presets[0].id, "user:alpha-000001");
    let restored = service.restore_draft().unwrap();
    assert_eq!(restored.preset_id, "user:alpha-000001");
    assert_eq!(restored.params.pitch_semitones, 8.0);
}

#[test]
fn 保存失败保留修改状态并可重试() {
    let dir = TemporaryDir::new();
    let blocker = dir.0.join("blocker");
    std::fs::write(&blocker, "not a directory").unwrap();
    let path = blocker.join("config.toml");
    let service = VoiceService::new_with_path(&Config::default(), path.clone());
    service
        .set_params(VoiceParamsDto {
            pitch_semitones: 6.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();

    service.save_as_user_preset("失败预设").unwrap();
    service.flush_saves();
    let status = service.status();
    assert_eq!(status.params.pitch_semitones, 6.0, "运行参数必须已生效");
    assert!(
        status
            .save_error
            .as_deref()
            .is_some_and(|error| error.contains("保存变声配置失败")),
        "保存失败必须报告，实际 {:?}",
        status.save_error
    );
    assert!(status.modified, "保存失败必须保持未保存状态");
    assert!(!path.exists());

    std::fs::remove_file(&blocker).unwrap();
    let retried = service.save_preset().unwrap();
    assert_eq!(retried.preset_id, status.preset_id);
    service.flush_saves();

    let status = service.status();
    assert!(status.save_error.is_none(), "重试成功后必须清除错误");
    assert!(!status.modified);
    let saved = Config::load_from(&path);
    let preset = saved
        .voice_presets
        .iter()
        .find(|preset| preset.id == status.preset_id)
        .expect("重试成功后预设必须落盘");
    assert_eq!(preset.params.pitch_semitones, 6.0);
}

#[test]
fn 会话重启与待机保留草稿与修改() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let first = VoiceControl::new(VoiceParams::neutral());
    service.attach(first);
    service
        .set_params(VoiceParamsDto {
            pitch_semitones: 7.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();

    service.detach();
    let second = VoiceControl::new(VoiceParams::neutral());
    service.attach(second.clone());
    let status = service.status();
    assert!(status.modified, "会话重建不得清除未保存修改");
    assert_eq!(status.params.pitch_semitones, 7.0);
    assert_eq!(second.params().pitch_semitones, 7.0);

    service.select_preset("builtin:male").unwrap();
    service.suspend();
    service.restore();
    service.flush_saves();
    service.detach();
    service.attach(VoiceControl::new(VoiceParams::neutral()));

    let status = service.status();
    assert!(status.draft_available, "待机与重建都不得清除草稿");
    let restored = service.restore_draft().unwrap();
    assert_eq!(restored.preset_id, "builtin:female");
    assert_eq!(restored.params.pitch_semitones, 7.0);
}

#[test]
fn 完整链路待机重建释放组合不丢预设与草稿() {
    let dir = TemporaryDir::new();
    let service = VoiceService::new_with_path(&user_config(), config_path(&dir));
    service.attach(VoiceControl::new(VoiceParams::neutral()));
    service.select_preset("builtin:male").unwrap();
    service
        .set_params(VoiceParamsDto {
            pitch_semitones: 7.0,
            formant_semitones: 2.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();
    let edited = service.select_preset("builtin:female").unwrap();
    assert!(edited.enabled);
    assert!(edited.draft_available, "切换预设应留下未保存草稿");
    assert_eq!(service.status().preset_id, "builtin:female");

    // 待机：运行控制保留，预设与草稿不变。
    service.suspend();
    let standby = service.status();
    assert!(standby.enabled);
    assert!(standby.draft_available);
    assert_eq!(standby.preset_id, "builtin:female");

    // 恢复并模拟设备重建：新控制句柄必须收到当前参数。
    service.restore();
    service.detach();
    let rebuilt = VoiceControl::new(VoiceParams::neutral());
    service.attach(rebuilt.clone());
    let after_rebuild = service.status();
    assert!(after_rebuild.enabled);
    assert!(after_rebuild.draft_available);
    assert_eq!(after_rebuild.preset_id, "builtin:female");
    assert_eq!(
        rebuilt.params(),
        VoiceParams::from(after_rebuild.params),
        "重建后的链必须沿用当前参数"
    );

    // 释放后再次接入：草稿仍保留，可恢复编辑前的预设。
    service.detach();
    let released = service.status();
    assert!(released.draft_available, "释放不得清除草稿");
    let third = VoiceControl::new(VoiceParams::neutral());
    service.attach(third.clone());
    let restored = service.restore_draft().unwrap();
    assert_eq!(restored.preset_id, "builtin:male");
    assert_eq!(restored.params.pitch_semitones, 7.0);
    assert_eq!(restored.params.formant_semitones, 2.0);
    assert!(!restored.draft_available);
    assert_eq!(third.params().pitch_semitones, 7.0);
    assert!(restored.enabled);
}

#[test]
fn 旧预设列表回复不能覆盖新选择() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let stale_generation = service.status().generation;
    let stale_presets = service.presets();
    assert!(stale_presets
        .iter()
        .any(|preset| preset.id == "builtin:female"));

    let selected = service.select_preset("builtin:male").unwrap();
    assert!(selected.generation > stale_generation);
    assert!(
        selected.is_stale_reply(stale_generation),
        "代次落后的预设列表回复必须丢弃"
    );
    assert!(!selected.is_stale_reply(selected.generation));
    assert_eq!(service.status().preset_id, "builtin:male");
}

#[test]
fn 选择预设不清除临时旁路() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());
    service.set_enabled(true).unwrap();
    service.set_bypass(true).unwrap();

    let status = service.select_preset("builtin:male").unwrap();
    assert!(status.bypassed, "选择预设不得清除临时旁路");
    assert!(status.enabled);
    assert_eq!(status.phase, "off");
    assert_eq!(status.params.pitch_semitones, -4.0);
    assert_eq!(control.params().pitch_semitones, -4.0);
    assert_eq!(control.phase(), VoicePhase::Off);
}

#[test]
fn 删除当前预设清除旁路并关闭() {
    let dir = TemporaryDir::new();
    let mut config = Config::default();
    config.voice.enabled = true;
    config.voice.active_preset = "user:alpha-000001".into();
    config.voice_presets = vec![preset("user:alpha-000001", "甲", 3.0)];
    let service = VoiceService::new_with_path(&config, config_path(&dir));
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());

    service
        .set_params(VoiceParamsDto {
            pitch_semitones: 6.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();
    service.select_preset("builtin:male").unwrap();
    service.select_preset("user:alpha-000001").unwrap();
    service.set_bypass(true).unwrap();
    let status = service.status();
    assert!(status.bypassed);
    assert!(status.draft_available);

    let status = service.delete_preset("user:alpha-000001").unwrap();
    assert!(!status.bypassed, "删除当前预设必须清除旁路");
    assert!(!status.enabled);
    assert_eq!(status.phase, "off");
    assert_eq!(status.preset_id, "builtin:female");
    assert!(!status.draft_available);
    assert_eq!(control.phase(), VoicePhase::Off);
}

#[test]
fn 用户预设写入保留并发非变声配置() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    let base = Config {
        mic_gain_db: Some(-3.0),
        voice: app_config::VoiceConfig {
            bypass_hotkey: "F9".into(),
            ..app_config::VoiceConfig::default()
        },
        ..Config::default()
    };
    base.save_to(&path).unwrap();

    let service = VoiceService::new_with_path(&Config::default(), path.clone());
    let mut concurrent = Config::load_from(&path);
    concurrent.mic_gain_db = Some(-2.0);
    concurrent.voice.bypass_hotkey = "F10".into();
    concurrent.save_to(&path).unwrap();

    let created = service.save_as_user_preset("写入门预设").unwrap();
    service.flush_saves();

    let saved = Config::load_from(&path);
    assert_eq!(saved.mic_gain_db, Some(-2.0), "不得覆盖并发的非变声配置");
    assert_eq!(saved.voice.bypass_hotkey, "F10", "不得覆盖并发的热键配置");
    assert_eq!(saved.voice.active_preset, created.preset_id);
    assert!(saved
        .voice_presets
        .iter()
        .any(|preset| preset.id == created.preset_id));
}

#[test]
fn 用户预设列表内置在前并按名称稳定排序() {
    let dir = TemporaryDir::new();
    let config = Config {
        voice_presets: vec![
            preset("user:zulu-000003", "B", 1.0),
            preset("user:alpha-000001", "A", 2.0),
            preset("user:beta-000002", "A", 3.0),
        ],
        ..Config::default()
    };
    let service = VoiceService::new_with_path(&config, config_path(&dir));

    let presets = service.presets();
    let builtin_count = builtin_voice_presets().len();
    assert!(presets[..builtin_count].iter().all(|preset| preset.builtin));
    let user_ids: Vec<_> = presets[builtin_count..]
        .iter()
        .map(|preset| preset.id.as_str())
        .collect();
    assert_eq!(
        user_ids,
        ["user:alpha-000001", "user:beta-000002", "user:zulu-000003"]
    );
}

#[test]
fn 旁路热键只切换临时旁路() {
    let dir = TemporaryDir::new();
    let config = user_config();
    config.save_to(&config_path(&dir)).unwrap();
    let service = VoiceService::new_with_path(&config, config_path(&dir));
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());
    service.set_enabled(true).unwrap();
    service.flush_saves();
    let before = service.status();
    let params = control.params();

    let outcome = route_voice_hotkey(&service, 7, VoiceHotkeyAction::ToggleBypass).unwrap();

    assert!(outcome.acted);
    assert!(outcome.status.bypassed, "热键应打开临时旁路");
    assert!(outcome.status.enabled, "临时旁路不能改写启用配置");
    assert_eq!(outcome.status.preset_id, before.preset_id);
    assert_eq!(control.phase(), VoicePhase::Off);
    assert_eq!(control.params(), params, "旁路不能改写参数快照");
    service.flush_saves();
    let saved = Config::load_from(&config_path(&dir));
    assert!(saved.voice.enabled);
    assert_eq!(saved.voice.active_preset, before.preset_id);
    assert_eq!(saved.voice.bypass_hotkey, "F9", "旁路不能改写保存的热键");

    let outcome = route_voice_hotkey(&service, 7, VoiceHotkeyAction::ToggleBypass).unwrap();
    assert!(!outcome.status.bypassed, "再次按旁路应恢复原状态");
    assert!(outcome.status.enabled);
}

#[test]
fn 变声关闭时旁路热键只提示状态() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());
    let before = service.status();

    let outcome = route_voice_hotkey(&service, 3, VoiceHotkeyAction::ToggleBypass).unwrap();

    assert!(!outcome.acted, "关闭状态不能建立旁路");
    assert!(!outcome.status.bypassed);
    assert!(!outcome.status.enabled);
    assert_eq!(
        outcome.status.generation, before.generation,
        "不得产生状态变更"
    );
    assert!(
        outcome
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("变声未开启")),
        "必须提示变声未开启：{:?}",
        outcome.notice
    );
    assert_eq!(control.phase(), VoicePhase::Off);
    assert_eq!(service.presets().len(), builtin_voice_presets().len());
}

#[test]
fn 无音频会话时切预设更新待用参数() {
    let dir = TemporaryDir::new();
    let config = Config {
        voice: app_config::VoiceConfig {
            enabled: true,
            active_preset: "builtin:male".into(),
            ..app_config::VoiceConfig::default()
        },
        ..Config::default()
    };
    let service = VoiceService::new_with_path(&config, config_path(&dir));

    let outcome = route_voice_hotkey(&service, 0, VoiceHotkeyAction::NextPreset).unwrap();

    assert!(outcome.acted);
    assert_eq!(outcome.status.preset_id, "builtin:child");
    assert_eq!(outcome.status.phase, "off", "无会话时不应启动音频");
    assert!(outcome.status.enabled, "切换预设不能改变启用状态");
    service.flush_saves();
    let saved = Config::load_from(&config_path(&dir));
    assert_eq!(saved.voice.active_preset, "builtin:child");
    assert!(saved.voice.enabled);
}

#[test]
fn 无音频会话时旁路热键只提示音频未启动() {
    let dir = TemporaryDir::new();
    let config = Config {
        voice: app_config::VoiceConfig {
            enabled: true,
            ..app_config::VoiceConfig::default()
        },
        ..Config::default()
    };
    let service = VoiceService::new_with_path(&config, config_path(&dir));
    let before = service.status();

    let outcome = route_voice_hotkey(&service, 0, VoiceHotkeyAction::ToggleBypass).unwrap();

    assert!(!outcome.acted, "无会话时不能建立旁路");
    assert!(!outcome.status.bypassed);
    assert_eq!(outcome.status.generation, before.generation);
    assert!(
        outcome
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("音频未启动")),
        "必须提示音频未启动：{:?}",
        outcome.notice
    );
}

#[test]
fn 预设循环顺序稳定() {
    let dir = TemporaryDir::new();
    let config = Config {
        voice: app_config::VoiceConfig {
            enabled: true,
            active_preset: "builtin:female".into(),
            ..app_config::VoiceConfig::default()
        },
        voice_presets: vec![
            preset("user:zulu-000003", "B", 1.0),
            preset("user:alpha-000001", "A", 2.0),
            preset("user:beta-000002", "A", 3.0),
        ],
        ..Config::default()
    };
    let service = VoiceService::new_with_path(&config, config_path(&dir));

    let mut seen = vec![service.status().preset_id];
    for _ in 0..8 {
        route_voice_hotkey(&service, 0, VoiceHotkeyAction::NextPreset).unwrap();
        seen.push(service.status().preset_id);
    }
    assert_eq!(
        seen,
        [
            "builtin:female",
            "builtin:male",
            "builtin:child",
            "builtin:deep",
            "builtin:robot",
            "builtin:radio",
            "user:alpha-000001",
            "user:beta-000002",
            "user:zulu-000003",
        ],
        "内置预设按定义顺序，用户预设按名称后 ID 排序"
    );
    assert!(service.status().enabled, "循环预设不能改变启用状态");

    route_voice_hotkey(&service, 0, VoiceHotkeyAction::NextPreset).unwrap();
    assert_eq!(
        service.status().preset_id,
        "builtin:female",
        "末尾应回到第一个"
    );
    route_voice_hotkey(&service, 0, VoiceHotkeyAction::PrevPreset).unwrap();
    assert_eq!(
        service.status().preset_id,
        "user:zulu-000003",
        "首端应回绕到末尾"
    );
}

#[test]
fn 热键按当前会话代次选择运行或待用路径() {
    let dir = TemporaryDir::new();
    let service = VoiceService::new_with_path(&user_config(), config_path(&dir));
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control);
    service.set_enabled(true).unwrap();

    let active = route_voice_hotkey(&service, 4, VoiceHotkeyAction::ToggleBypass).unwrap();
    assert!(active.acted, "有会话时代次非 0，旁路应生效");
    assert!(active.status.bypassed);

    let released = route_voice_hotkey(&service, 0, VoiceHotkeyAction::ToggleBypass).unwrap();
    assert!(!released.acted, "无会话时代次 0，旁路只提示");
    assert!(released.status.bypassed, "无会话不能改写运行状态");
    assert!(
        released
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("音频未启动")),
        "必须提示音频未启动：{:?}",
        released.notice
    );
}

#[test]
fn 热键事件不进入音频回调只改控制路径() {
    let dir = TemporaryDir::new();
    let service = VoiceService::new_with_path(&user_config(), config_path(&dir));
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());
    service.set_enabled(true).unwrap();
    let params = control.params();
    let generation = control.generation();

    route_voice_hotkey(&service, 5, VoiceHotkeyAction::ToggleBypass).unwrap();

    // 事件只能通过 VoiceControl 的原子快照改变运行阶段；没有样本处理入口。
    assert_eq!(control.phase(), VoicePhase::Off);
    assert!(control.generation() > generation);
    assert_eq!(control.params(), params, "热键不能改写 DSP 参数快照");
}

#[test]
fn 人声热键保存不覆盖变声状态与预设() {
    let dir = TemporaryDir::new();
    let config = user_config();
    let expected_presets = config.voice_presets.clone();
    let path = config_path(&dir);
    config.save_to(&path).unwrap();
    let service = VoiceService::new_with_path(&config, path.clone());

    service.save_hotkeys("F5", "F6", "F7").unwrap();

    assert_eq!(
        service.hotkey_specs(),
        vec!["F5".to_owned(), "F6".to_owned(), "F7".to_owned()]
    );
    let saved = Config::load_from(&path);
    assert_eq!(saved.voice.bypass_hotkey, "F5");
    assert_eq!(saved.voice.next_preset_hotkey, "F6");
    assert_eq!(saved.voice.prev_preset_hotkey, "F7");
    assert!(saved.voice.enabled);
    assert_eq!(saved.voice.active_preset, "builtin:male");
    assert_eq!(saved.voice_presets, expected_presets);
}

#[test]
fn 遥控路由读取变声状态与用户预设() {
    let dir = TemporaryDir::new();
    let service = VoiceService::new_with_path(&user_config(), config_path(&dir));
    let route = service.remote_route();

    let state = (route.state)();

    assert!(state.enabled);
    assert_eq!(state.preset_id, "builtin:male");
    assert_eq!(state.preset_name, "男声");
    assert!(state
        .presets
        .iter()
        .any(|(id, name)| id == "user:test-abcdef" && name == "测试预设"));
}

#[test]
fn 遥控路由启用写入宿主配置且选择预设不启用() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    let service = VoiceService::new_with_path(&Config::default(), path.clone());
    let route = service.remote_route();

    assert!((route.select_preset)("builtin:male"));
    assert!(!(route.state)().enabled, "选择预设不能隐式启用变声");
    assert!(!(route.select_preset)("missing"), "未知预设必须失败");
    assert_eq!((route.state)().preset_id, "builtin:male");

    assert!((route.set_enabled)(true));
    assert!((route.state)().enabled);
    service.flush_saves();
    let saved = Config::load_from(&path);
    assert!(saved.voice.enabled, "遥控启用必须写入宿主配置");
    assert_eq!(saved.voice.active_preset, "builtin:male");
}

#[test]
fn 退休的遥控路由不能改变状态() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let retired = service.remote_route();
    let current = service.remote_route();

    assert!(!(retired.set_enabled)(true), "旧路由不能启用变声");
    assert!(
        !(retired.select_preset)("builtin:male"),
        "旧路由不能切换预设"
    );
    let status = service.status();
    assert!(!status.enabled);
    assert_eq!(status.preset_id, "builtin:female");

    assert!((current.set_enabled)(true));
    assert!(service.status().enabled);
}

#[test]
fn 遥控写入检查与退休在同一状态锁内() {
    let dir = TemporaryDir::new();
    let path = config_path(&dir);
    let service = VoiceService::new_with_path(&Config::default(), path.clone());
    let retired = service.remote_route();
    let stale_enable = retired.set_enabled.clone();
    let (checked_tx, checked_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    *service.shared.remote_pause.lock().unwrap() = Some(RemoteWritePause {
        checked: checked_tx,
        resume: Mutex::new(resume_rx),
    });

    let command = std::thread::spawn(move || (retired.set_enabled)(true));
    checked_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    let (retired_tx, retired_rx) = mpsc::channel();
    let retiring = service.clone();
    let retire = std::thread::spawn(move || {
        retiring.retire_remote_route();
        retired_tx.send(()).unwrap();
    });
    assert!(
        retired_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "退休必须等待已经通过代次检查的遥控写入完成"
    );

    resume_tx.send(()).unwrap();
    assert!(command.join().unwrap(), "退休前通过检查的命令应完成写入");
    retired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    retire.join().unwrap();

    assert!(service.status().enabled);
    assert!(!stale_enable(false), "退休后的旧闭包必须失败");
    assert!(service.status().enabled, "旧闭包不能改变状态");
    service.flush_saves();
    let saved = Config::load_from(&path);
    assert!(saved.voice.enabled, "旧闭包不能覆盖退休前完成的配置写入");
}

// ── 三秒人声试听的状态机 ─────────────────────────────────────

#[test]
fn 试听状态经历准备录制回放与取消() {
    let dir = TemporaryDir::new();
    let service = service(&dir);

    let generation = service.preview_begin();
    let preview = service.status().preview;
    assert_eq!(preview.state, "preparing");
    assert_eq!(preview.elapsed_ms, 0);
    assert!(preview.reason.is_none());
    assert!(service.preview_is_current(generation));

    service.preview_mark_recording(generation);
    std::thread::sleep(Duration::from_millis(20));
    let preview = service.status().preview;
    assert_eq!(preview.state, "recording");
    assert!(preview.elapsed_ms >= 10, "录制进度应来自实际经过时间");

    service.preview_mark_playing(generation);
    assert_eq!(service.status().preview.state, "playing");

    service.preview_finish(generation);
    let preview = service.status().preview;
    assert_eq!(preview.state, "idle");
    assert_eq!(preview.elapsed_ms, 0);
}

#[test]
fn 旧试听完成不能覆盖新试听() {
    let dir = TemporaryDir::new();
    let service = service(&dir);

    let old = service.preview_begin();
    let current = service.preview_begin();
    service.preview_mark_recording(current);

    service.preview_finish(old);
    service.preview_unavailable(old, "旧试听失败");

    let preview = service.status().preview;
    assert_eq!(preview.state, "recording", "旧代次结果不能改写新试听");
    assert!(preview.reason.is_none());
    assert!(service.preview_is_current(current));
    assert!(!service.preview_is_current(old));
}

#[test]
fn 切换预设与启停旁路取消试听() {
    let dir = TemporaryDir::new();
    let service = service(&dir);

    let generation = service.preview_begin();
    service.select_preset("builtin:male").unwrap();
    assert_eq!(service.status().preview.state, "idle");
    assert!(!service.preview_is_current(generation));

    let generation = service.preview_begin();
    service.set_enabled(false).unwrap();
    assert_eq!(service.status().preview.state, "idle");
    assert!(!service.preview_is_current(generation));

    let generation = service.preview_begin();
    service.set_bypass(true).unwrap();
    assert_eq!(service.status().preview.state, "idle");
    assert!(!service.preview_is_current(generation));
}

#[test]
fn 试听不可用保留具体原因() {
    let dir = TemporaryDir::new();
    let service = service(&dir);

    let generation = service.preview_begin();
    service.preview_unavailable(generation, "音频未启动");
    let preview = service.status().preview;
    assert_eq!(preview.state, "unavailable");
    assert_eq!(preview.reason.as_deref(), Some("音频未启动"));

    // 下一次开始会替换旧原因。
    service.preview_begin();
    let preview = service.status().preview;
    assert_eq!(preview.state, "preparing");
    assert!(preview.reason.is_none());
}

#[test]
fn 会话释放清除试听状态() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let generation = service.preview_begin();
    service.preview_mark_recording(generation);

    service.detach();

    let preview = service.status().preview;
    assert_eq!(preview.state, "idle");
    assert!(!service.preview_is_current(generation));
}

#[test]
fn 录制中修改参数立即取消试听() {
    let dir = TemporaryDir::new();
    let service = service(&dir);

    let generation = service.preview_begin();
    service.preview_mark_recording(generation);
    let status = service
        .set_params(VoiceParamsDto {
            pitch_semitones: 9.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();
    assert_eq!(status.preview.state, "idle", "改参必须立即结束录制状态");
    assert!(!service.preview_is_current(generation));

    // 参数没有变化时不能打断正在进行的录制。
    let generation = service.preview_begin();
    service.preview_mark_recording(generation);
    let current = service.status().params;
    let status = service.set_params(current).unwrap();
    assert_eq!(status.preview.state, "recording", "参数未变化不应取消录制");
    assert!(service.preview_is_current(generation));
}
