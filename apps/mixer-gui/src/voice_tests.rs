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
fn 变声暂停期间临时旁路并在结束后恢复() {
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
