use super::*;

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

fn user_config() -> Config {
    let mut config = Config::default();
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
fn 变声暂停期间临时旁路并在结束后恢复() {
    let dir = TemporaryDir::new();
    let service = service(&dir);
    let control = VoiceControl::new(VoiceParams::neutral());
    service.attach(control.clone());
    service.set_enabled(true).unwrap();
    service.flush_saves();

    let saved = service.suspend();
    assert!(service.status().bypassed);
    assert_eq!(control.phase(), VoicePhase::Off);
    service.flush_saves();
    assert!(
        Config::load_from(&config_path(&dir)).voice.enabled,
        "自检暂停不得改写启用配置"
    );

    service.restore(saved);
    let status = service.status();
    assert!(!status.bypassed);
    assert!(status.enabled);
    assert_eq!(control.phase(), VoicePhase::Warmup);
    service.flush_saves();
    assert!(Config::load_from(&config_path(&dir)).voice.enabled);
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
