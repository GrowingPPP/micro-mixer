use super::*;
use audio_core::device::{AudioStream, CaptureCallback, DeviceInfo, RenderCallback};
use audio_core::engine::SfxSlot;
use std::sync::atomic::AtomicUsize;

#[cfg(feature = "voice")]
use crate::voice::{VoiceParamsDto, VoiceService, VoiceStartConfig};
#[cfg(feature = "voice")]
use audio_core::voice::{VoiceBackend, VoiceParams, VoicePhase};

type CloseBarrier = (mpsc::Sender<()>, Arc<Mutex<mpsc::Receiver<()>>>);

#[derive(Clone, Default)]
struct FakeBackend {
    outputs: Arc<Mutex<Vec<RenderCallback>>>,
    captures: Arc<Mutex<Vec<CaptureCallback>>>,
    opened: Arc<Mutex<Option<mpsc::Sender<()>>>>,
    close_barrier: Arc<Mutex<Option<CloseBarrier>>>,
    closed: Arc<Mutex<Option<mpsc::Sender<()>>>>,
}
struct FakeStream(StreamFormat, Option<CloseBarrier>, Option<mpsc::Sender<()>>);
impl Drop for FakeStream {
    fn drop(&mut self) {
        if let Some((entered, release)) = &self.1 {
            entered.send(()).unwrap();
            release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
        if let Some(closed) = &self.2 {
            let _ = closed.send(());
        }
    }
}
impl AudioStream for FakeStream {
    fn format(&self) -> StreamFormat {
        self.0
    }
    fn device_name(&self) -> &str {
        "fake endpoint"
    }
}
impl AudioBackend for FakeBackend {
    fn backend_name(&self) -> &'static str {
        "fake"
    }
    fn devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(Vec::new())
    }
    fn default_input(&self) -> Option<DeviceId> {
        None
    }
    fn default_output(&self) -> Option<DeviceId> {
        None
    }
    fn input_format(&self, _: &DeviceId, prefer: Option<StreamFormat>) -> Result<StreamFormat> {
        Ok(prefer.unwrap())
    }
    fn output_format(&self, _: &DeviceId, prefer: Option<StreamFormat>) -> Result<StreamFormat> {
        Ok(prefer.unwrap())
    }
    fn open_input(
        &self,
        _: &DeviceId,
        format: Option<StreamFormat>,
        callback: CaptureCallback,
    ) -> Result<Box<dyn AudioStream>> {
        self.captures.lock().unwrap().push(callback);
        Ok(Box::new(FakeStream(format.unwrap(), None, None)))
    }
    fn open_output(
        &self,
        _: &DeviceId,
        format: Option<StreamFormat>,
        callback: RenderCallback,
    ) -> Result<Box<dyn AudioStream>> {
        self.outputs.lock().unwrap().push(callback);
        if let Some(sender) = self.opened.lock().unwrap().as_ref() {
            let _ = sender.send(());
        }
        Ok(Box::new(FakeStream(
            format.unwrap(),
            self.close_barrier.lock().unwrap().clone(),
            self.closed.lock().unwrap().clone(),
        )))
    }
}

fn config() -> StartConfig {
    StartConfig {
        input_device: None,
        output_device: Some("fake output".into()),
        files: vec!["invalid.wav".into(), "good.wav".into()],
        ids: vec!["invalid-id".into(), "good-id".into()],
        tags: vec![vec![], vec!["B, only".into()]],
        gains: vec![12.0, 0.0],
        names: vec!["Invalid".into(), "Good".into()],
        hotkeys: vec![String::new(), String::new()],
        buffer_ms: 20,
        mic_gain_db: -100.0,
        sfx_gain_db: 0.0,
        looping: false,
        monitor: true,
        monitor_device: Some("fake monitor".into()),
        remote_port: Some(8765),
        voice: None,
    }
}
// 既有 PCM 场景使用单位增益，产品默认增益在专门的回归中验证。
fn preview_test_host() -> AudioHost {
    let host = AudioHost::new();
    host.set_gains(0.0, 0.0);
    host
}

fn fixture() -> (AudioHost, FakeBackend) {
    let cfg = config();
    let backend = FakeBackend::default();
    let format = StreamFormat::stereo(44_100);
    let slot = SfxSlot {
        name: "Good".into(),
        samples: Arc::new(vec![0.2; 88_200]),
        looping: false,
        gain_db: 0.0,
    };
    let session = Session::open_with_backend(
        SessionOptions {
            input: DeviceId::new("fake input"),
            output: DeviceId::new("fake output"),
            input_format: format,
            output_format: format,
            buffer_ms: cfg.buffer_ms,
            slots: vec![slot.clone()],
            mic_gain_db: cfg.mic_gain_db,
            sfx_gain_db: cfg.sfx_gain_db,
            monitor: Some(MonitorOptions {
                device: DeviceId::new("fake monitor"),
                format,
                slots: vec![slot],
            }),
            voice: None,
        },
        backend.clone(),
        backend.clone(),
    )
    .unwrap();
    let playback = session.playback.clone();
    let host = preview_test_host();
    host.next_generation.store(1, Ordering::Relaxed);
    host.audio_requests.register_session(1);
    let shared = Arc::new(remote::Shared::default());
    shared.set_routed(
        playback.control.clone(),
        vec!["Good".into()],
        host.play_handler(playback.clone(), 1),
        host.remote_stop_handler(playback.clone(), 1),
    );
    *host.remote.lock().unwrap() = Some(shared);
    *host.running.lock().unwrap() = Some(Running {
        generation: 1,
        stats: session.stats.clone(),
        playback,
        session,
        shutdown: Arc::new(AtomicBool::new(false)),
        hotkey_thread: None,
        slots_meta: vec![SlotDto {
            index: 0,
            source_index: 1,
            id: "good-id".into(),
            name: "Good".into(),
            file: "good.wav".into(),
            duration: 1.0,
            peak_db: -14.0,
            hotkey: String::new(),
            gain_db: 0.0,
        }],
        out_device: "fake output".into(),
        in_device: "fake input".into(),
        input_id: DeviceId::new("fake input"),
        output_id: DeviceId::new("fake output"),
        monitor_error: Mutex::new(None),
        started_at: std::time::Instant::now(),
        skipped_notes: vec!["Invalid".into()],
        hotkeys: Arc::new(Mutex::new(hotkeys::HotkeyGroups::default())),
        cfg_key: cfg_fingerprint(&cfg),
        cfg,
    });
    (host, backend)
}

#[cfg(feature = "voice")]
struct FakeVoiceBackend;

#[cfg(feature = "voice")]
impl VoiceBackend for FakeVoiceBackend {
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        let frames = input.len().min(output.len());
        output[..frames].copy_from_slice(&input[..frames]);
        frames
    }

    fn latency_frames(&self) -> usize {
        0
    }

    fn reset(&mut self) {}

    fn set_params(&mut self, _: VoiceParams) {}
}

#[cfg(feature = "voice")]
fn voice_fixture(enabled: bool) -> (AudioHost, FakeBackend, TemporaryDir) {
    let cfg = config();
    let backend = FakeBackend::default();
    let format = StreamFormat::stereo(44_100);
    let slot = SfxSlot {
        name: "Good".into(),
        samples: Arc::new(vec![0.2; 88_200]),
        looping: false,
        gain_db: 0.0,
    };
    let dir = TemporaryDir::new();
    let mut host = preview_test_host();
    host.voice =
        VoiceService::new_with_path(&app_config::Config::default(), dir.0.join("config.toml"));
    let params = VoiceParams {
        pitch_semitones: 4.0,
        ..VoiceParams::neutral()
    };
    host.voice
        .apply_start_config(&VoiceStartConfig {
            enabled,
            params: Some(params.into()),
            block_ms: Some(crate::voice::DEFAULT_VOICE_BLOCK_MS),
        })
        .unwrap();
    let session = Session::open_with_backend(
        SessionOptions {
            input: DeviceId::new("fake input"),
            output: DeviceId::new("fake output"),
            input_format: format,
            output_format: format,
            buffer_ms: cfg.buffer_ms,
            slots: vec![slot.clone()],
            mic_gain_db: 0.0,
            sfx_gain_db: 0.0,
            monitor: Some(MonitorOptions {
                device: DeviceId::new("fake monitor"),
                format,
                slots: vec![slot],
            }),
            voice: Some(audio_session::VoiceOptions {
                enabled,
                params,
                block_ms: crate::voice::DEFAULT_VOICE_BLOCK_MS,
                backend: Box::new(|_| Ok(Box::new(FakeVoiceBackend))),
            }),
        },
        backend.clone(),
        backend.clone(),
    )
    .unwrap();
    let control = session.voice_control();
    let playback = session.playback.clone();
    host.next_generation.store(1, Ordering::Relaxed);
    host.audio_requests.register_session(1);
    *host.running.lock().unwrap() = Some(Running {
        generation: 1,
        stats: session.stats.clone(),
        playback,
        session,
        shutdown: Arc::new(AtomicBool::new(false)),
        hotkey_thread: None,
        slots_meta: vec![SlotDto {
            index: 0,
            source_index: 0,
            id: "good-id".into(),
            name: "Good".into(),
            file: "good.wav".into(),
            duration: 1.0,
            peak_db: -14.0,
            hotkey: String::new(),
            gain_db: 0.0,
        }],
        out_device: "fake output".into(),
        in_device: "fake input".into(),
        input_id: DeviceId::new("fake input"),
        output_id: DeviceId::new("fake output"),
        monitor_error: Mutex::new(None),
        started_at: std::time::Instant::now(),
        skipped_notes: Vec::new(),
        hotkeys: Arc::new(Mutex::new(hotkeys::HotkeyGroups::default())),
        cfg_key: cfg_fingerprint(&cfg),
        cfg,
    });
    host.attach_voice_control(control);
    (host, backend, dir)
}

#[test]
fn every_start_setting_changes_the_fingerprint() {
    let baseline = config();
    let key = cfg_fingerprint(&baseline);
    type Change = (&'static str, fn(&mut StartConfig));
    let changes: &[Change] = &[
        ("input", |c| c.input_device = Some("different".into())),
        ("output", |c| c.output_device = Some("different".into())),
        ("files", |c| c.files.push("new.wav".into())),
        ("ids", |c| c.ids[1] = "new-id".into()),
        ("tags", |c| c.tags[1].push("tag".into())),
        ("gains", |c| c.gains[1] = -20.0),
        ("names", |c| c.names[1] = "Renamed".into()),
        ("hotkeys", |c| c.hotkeys[1] = "F2".into()),
        ("buffer", |c| c.buffer_ms = 40),
        ("mic gain", |c| c.mic_gain_db = -12.0),
        ("sfx gain", |c| c.sfx_gain_db = -3.0),
        ("looping", |c| c.looping = true),
        ("monitor", |c| c.monitor = false),
        ("monitor endpoint", |c| {
            c.monitor_device = Some("different".into())
        }),
        ("remote port", |c| c.remote_port = Some(9000)),
    ];
    for (name, change) in changes {
        let mut cfg = baseline.clone();
        change(&mut cfg);
        assert_ne!(
            key,
            cfg_fingerprint(&cfg),
            "changed {name} must rebuild the session"
        );
    }
}

#[test]
fn 变声字段不参与启动指纹() {
    let mut cfg = config();
    let key = cfg_fingerprint(&cfg);
    cfg.voice = Some(crate::voice::VoiceStartConfig {
        enabled: true,
        params: Some(crate::voice::VoiceParamsDto {
            pitch_semitones: 12.0,
            ..crate::voice::VoiceParamsDto::default()
        }),
        block_ms: Some(20.0),
    });
    assert_eq!(
        key,
        cfg_fingerprint(&cfg),
        "变声参数只能作用于运行控制，不能触发音频会话重启"
    );
}

#[cfg(feature = "voice")]
#[test]
fn 变声参数变化不重启音频会话() {
    let (host, backend, _dir) = voice_fixture(true);
    let before_generation = host.audio_progress().0;
    let before_key = host.running_cfg_key().unwrap();
    let before_outputs = backend.outputs.lock().unwrap().len();

    let status = host
        .voice_set_params(VoiceParamsDto {
            pitch_semitones: 9.0,
            formant_semitones: -3.0,
            ..VoiceParamsDto::default()
        })
        .unwrap();

    assert_eq!(status.params.pitch_semitones, 9.0);
    assert_eq!(status.params.formant_semitones, -3.0);
    assert_eq!(host.audio_progress().0, before_generation);
    assert_eq!(host.running_cfg_key().unwrap(), before_key);
    assert_eq!(backend.outputs.lock().unwrap().len(), before_outputs);
    let control = host.voice_control.lock().unwrap().clone().unwrap();
    assert_eq!(control.params().pitch_semitones, 9.0);
}

#[cfg(feature = "voice")]
#[test]
fn 变声待机保留变声控制() {
    let (host, _backend, _dir) = voice_fixture(true);
    let before = host.status().voice.expect("运行时应带变声状态");

    host.stop();

    let status = host.status();
    assert!(!status.running);
    assert!(status.passthrough);
    assert!(
        host.voice_control.lock().unwrap().is_some(),
        "待机只关闭音效，变声控制句柄必须保留"
    );
    let voice = status.voice.expect("待机仍应报告变声状态");
    assert!(voice.enabled);
    assert_eq!(voice.phase, before.phase);
}

#[cfg(feature = "voice")]
#[test]
fn 变声释放后重建使用最新配置() {
    let (host, _backend, dir) = voice_fixture(false);
    host.release();
    assert!(host.voice_control.lock().unwrap().is_none());

    host.voice_set_params(VoiceParamsDto {
        pitch_semitones: 9.0,
        ..VoiceParamsDto::default()
    })
    .unwrap();
    host.voice_set_enabled(true).unwrap();

    let options = host
        .voice
        .prepare_options(crate::voice::DEFAULT_VOICE_BLOCK_MS)
        .unwrap();
    assert!(options.enabled, "重建会话必须采用最新启用状态");
    assert_eq!(options.params.pitch_semitones, 9.0);

    let control = VoiceControl::new(options.params);
    if options.enabled {
        control.request_enable();
    }
    host.attach_voice_control(Some(control.clone()));
    assert_eq!(control.phase(), VoicePhase::Warmup);
    assert_eq!(control.params().pitch_semitones, 9.0);
    host.voice.flush_saves();
    let saved = app_config::Config::load_from(&dir.0.join("config.toml"));
    assert!(saved.voice.enabled);
}

#[cfg(feature = "voice")]
#[test]
fn 变声自检与校准期间临时旁路并在结束后恢复() {
    let (host, _backend, dir) = voice_fixture(true);
    host.voice_set_enabled(true).unwrap();
    host.voice.flush_saves();
    assert_eq!(host.voice_status().phase, "warmup");

    host.voice.suspend();
    let options = host
        .voice
        .prepare_options(crate::voice::DEFAULT_VOICE_BLOCK_MS)
        .unwrap();
    assert!(!options.enabled, "暂停期间重建会话也不得启用人声");
    assert!(host.voice_status().bypassed);
    assert_eq!(host.voice_status().phase, "off", "旁路期间输出干声");

    // 生产释放路径会 detach 控制句柄，但暂停层必须保留到 restore。
    host.release_inner();
    assert!(host.voice_control.lock().unwrap().is_none());
    assert!(host.voice_status().bypassed);
    assert_eq!(host.voice_status().phase, "off");

    host.voice.restore();
    let status = host.voice_status();
    assert!(status.enabled);
    assert!(!status.bypassed);

    // 模拟恢复音频后绑定新会话句柄。
    let rebuilt = VoiceControl::new(VoiceParams::neutral());
    host.attach_voice_control(Some(rebuilt.clone()));
    assert_eq!(rebuilt.phase(), VoicePhase::Warmup);
    host.voice.flush_saves();
    assert!(
        app_config::Config::load_from(&dir.0.join("config.toml"))
            .voice
            .enabled,
        "自检暂停与恢复不得改写启用配置"
    );
}

#[cfg(feature = "voice")]
#[test]
fn 变声暂停期间的关闭命令不会被恢复覆盖() {
    let (host, _backend, dir) = voice_fixture(true);
    host.voice.suspend();
    host.voice_set_enabled(false).unwrap();
    assert!(
        host.voice_status().bypassed,
        "暂停层仍在时即使显式关闭也应保持干声"
    );

    host.release_inner();
    host.voice.restore();

    let status = host.voice_status();
    assert!(!status.enabled, "恢复不能把暂停期间的关闭命令改回启用");
    assert!(!status.bypassed);
    assert_eq!(status.phase, "off");
    host.voice.flush_saves();
    assert!(
        !app_config::Config::load_from(&dir.0.join("config.toml"))
            .voice
            .enabled,
        "暂停期间的关闭命令必须保持落盘结果"
    );
}

#[cfg(feature = "voice")]
#[test]
fn 变声监听路径不接受人声() {
    let (host, backend, _dir) = voice_fixture(true);
    host.set_gains(0.0, 0.0);
    let mut primary_tail = 0.0;
    for _ in 0..400 {
        for callback in backend.captures.lock().unwrap().iter_mut() {
            callback(&vec![0.3; 4_410]);
        }
        let mut outputs = backend.outputs.lock().unwrap();
        let mut primary = vec![0.0; 4_410 * 2];
        outputs[0](&mut primary);
        let mut monitor = vec![0.0; 4_410 * 2];
        outputs[1](&mut monitor);
        primary_tail = primary[primary.len() - 2_000..]
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        assert!(
            monitor.iter().all(|sample| sample.abs() < 1e-6),
            "监听路径只能播放音效，不能混入人声"
        );
    }
    assert!(primary_tail > 0.1, "主输出应收到人声，实际 {primary_tail}");
    assert_eq!(
        host.status().voice.unwrap().phase,
        "wet",
        "驱动采集与渲染后应进入湿声"
    );
}

#[test]
fn status_uses_the_negotiated_sample_rate_for_time_and_ring_fill() {
    let (host, _) = fixture();
    {
        let running = host.running.lock().unwrap();
        let stats = &running.as_ref().unwrap().stats;
        stats.playhead.store(44_100, Ordering::Relaxed);
        stats.ring_fill.store(1_764, Ordering::Relaxed);
    }
    let status = host.status();
    assert!((status.playhead_secs - 1.0).abs() < 1e-6);
    assert!((status.ring_ms - 20.0).abs() < 1e-6);
}

#[test]
fn standby_rejects_gui_and_remote_triggers_and_activate_restores_remote() {
    let (host, _) = fixture();
    let shared = host.remote.lock().unwrap().as_ref().unwrap().clone();
    host.stop();
    assert!(!host.play(0).unwrap());
    assert!(!shared.play(0));
    assert!(shared.control.lock().unwrap().is_none());
    assert!(!host.status().running);
    assert!(host.status().passthrough);
    host.activate(None).unwrap();
    assert!(host.status().running);
    assert!(shared.play(0));
    assert!(shared.control.lock().unwrap().is_some());
    assert_eq!(host.status().monitor_plays, 1);
}

#[test]
fn live_gains_update_source_metadata_saved_settings_and_both_render_paths() {
    let (host, backend) = fixture();
    assert!(host.set_slot_gain(0, -20.0).unwrap());
    host.set_gains(-12.0, -6.0);
    {
        let guard = host.running.lock().unwrap();
        let running = guard.as_ref().unwrap();
        assert_eq!(running.cfg.gains, [12.0, -20.0]);
        assert_eq!(running.slots_meta[0].gain_db, -20.0);
        assert_eq!(running.cfg.mic_gain_db, -12.0);
        assert_eq!(running.cfg.sfx_gain_db, -6.0);
        assert_eq!(running.cfg_key, cfg_fingerprint(&running.cfg));
    }
    assert!(host.play(0).unwrap());
    let mut outputs = backend.outputs.lock().unwrap();
    assert_eq!(outputs.len(), 2);
    for callback in outputs.iter_mut() {
        let mut output = vec![0.0; 8_820];
        callback(&mut output);
        let peak = output[output.len() - 1_000..]
            .iter()
            .fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(
            (peak - 0.2 * 10.0f32.powf(-26.0 / 20.0)).abs() < 0.001,
            "primary and monitor must receive live gains, got {peak}"
        );
    }
}

#[test]
fn looping_updates_current_primary_and_monitor_then_stops_at_the_current_pass_end() {
    let (host, backend) = fixture();
    assert!(host.play(0).unwrap());
    for callback in backend.outputs.lock().unwrap().iter_mut() {
        callback(&mut [0.0; 8_820]);
    }
    let before = host.status().playhead_secs;
    assert!(host.set_looping(true));
    assert_eq!(
        host.status().playhead_secs,
        before,
        "changing loop must not rewind the sound"
    );
    assert!(host.status().looping);
    for callback in backend.outputs.lock().unwrap().iter_mut() {
        let mut pcm = vec![0.0; 176_400];
        callback(&mut pcm);
        assert!(
            (pcm[pcm.len() - 1_000] - 0.2).abs() < 1e-6,
            "primary and monitor must repeat"
        );
    }
    assert!(host.set_looping(false));
    assert!(!host.status().looping);
    for callback in backend.outputs.lock().unwrap().iter_mut() {
        let mut pcm = vec![0.0; 132_300];
        callback(&mut pcm);
        assert!(
            pcm[..44_100].iter().any(|&v| v > 0.1),
            "finish the current pass instead of stopping abruptly"
        );
        assert!(
            pcm[pcm.len() - 1_000..].iter().all(|&v| v == 0.0),
            "unchecking loop must stop at EOF"
        );
    }
    assert!(!host.status().playing);
    {
        let guard = host.running.lock().unwrap();
        let running = guard.as_ref().unwrap();
        assert!(!running.cfg.looping);
        assert_eq!(running.cfg_key, cfg_fingerprint(&running.cfg));
    }
    assert_eq!(
        backend.outputs.lock().unwrap().len(),
        2,
        "loop changes must retain both output streams"
    );
    assert!(host.play(0).unwrap());
    for callback in backend.outputs.lock().unwrap().iter_mut() {
        let mut pcm = vec![0.0; 132_300];
        callback(&mut pcm);
        assert!(
            pcm[88_400..].iter().all(|&v| v == 0.0),
            "future triggers must also play only once"
        );
    }
}

#[test]
fn standby_looping_edit_applies_to_next_remote_trigger_and_idle_edit_is_safe() {
    assert!(!preview_test_host().set_looping(true));
    let (host, backend) = fixture();
    host.stop();
    assert!(host.set_looping(true));
    assert!(!host.status().running);
    assert!(host.status().looping);
    host.activate(None).unwrap();
    assert!(host.remote.lock().unwrap().as_ref().unwrap().play(0));
    for callback in backend.outputs.lock().unwrap().iter_mut() {
        let mut pcm = vec![0.0; 176_400];
        callback(&mut pcm);
        assert!((pcm[pcm.len() - 1_000] - 0.2).abs() < 1e-6);
    }
    host.stop_all().unwrap();
    assert!(host.set_looping(true));
    for callback in backend.outputs.lock().unwrap().iter_mut() {
        let mut pcm = [0.0; 8_192];
        callback(&mut pcm);
        assert!(
            pcm[4_096..].iter().all(|&v| v == 0.0),
            "loop edits must not revive stopped sounds"
        );
    }
}

#[test]
fn only_primary_session_errors_drive_main_device_recovery() {
    let (host, _) = fixture();
    let (primary, monitor) = {
        let guard = host.running.lock().unwrap();
        let running = guard.as_ref().unwrap();
        (
            running.session.health.clone(),
            running.session.monitor_health.clone(),
        )
    };
    monitor.report_error("输出", "monitor disconnected");
    monitor.mark_panicked();
    assert!(primary.take_error().is_none());
    assert!(!host.audio_progress().3);
    primary.report_error("输入", "main input disconnected");
    let loss = host.check_devices().unwrap();
    assert_eq!(loss.0, 1);
    assert!(loss.1 && !loss.2);
    assert_eq!(loss.3, "fake input");
    let status = host.status();
    assert!(status
        .monitor_error
        .unwrap()
        .contains("monitor disconnected"));
    assert!(status.running);
    assert!(host
        .status()
        .monitor_error
        .unwrap()
        .contains("monitor disconnected"));
    assert!(monitor.take_error().is_none());
}

#[test]
fn suspension_holds_lifecycle_lock_until_test_finishes() {
    let host = Arc::new(preview_test_host());
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let testing = host.clone();
    let test = std::thread::spawn(move || {
        testing.with_suspended(None, || {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok(())
        })
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let recovery = host;
    let recovering = std::thread::spawn(move || {
        let result = recovery.recover_input_device(1, None);
        done_tx.send(result.unwrap().is_none()).unwrap();
    });
    assert!(done_rx.recv_timeout(Duration::from_millis(30)).is_err());
    release_tx.send(()).unwrap();
    test.join().unwrap().unwrap();
    assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
    recovering.join().unwrap();
}

#[test]
fn start_cannot_activate_while_another_lifecycle_operation_holds_the_lock() {
    let (host, _) = fixture();
    let host = Arc::new(host);
    let lock = host.lifecycle.lock().unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let starting = host.clone();
    let worker = std::thread::spawn(move || {
        done_tx
            .send(starting.start(config(), None).is_ok())
            .unwrap();
    });
    assert!(done_rx.recv_timeout(Duration::from_millis(30)).is_err());
    drop(lock);
    assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
    worker.join().unwrap();
}

struct TemporaryDir(PathBuf);
impl TemporaryDir {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "mixer-host-regression-{}-{}",
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
fn write_wav(path: &Path) {
    let rate = 8_000u32;
    let length = rate * 2;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + length).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&length.to_le_bytes());
    for i in 0..rate {
        let sample = (0.25
            * (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin()
            * i16::MAX as f32) as i16;
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn bad_file_filtering_keeps_gain_hotkey_path_tags_and_stable_id_together() {
    let directory = TemporaryDir::new();
    let good = directory.0.join("Doe, John sound.wav");
    write_wav(&good);
    let specs = [
        SoundSpec {
            id: "bad-id".into(),
            file: directory.0.join("missing.wav"),
            name: "Bad".into(),
            gain_db: 12.0,
            hotkey: "F1".into(),
            tags: vec![],
        },
        SoundSpec {
            id: "good-id".into(),
            file: good.clone(),
            name: "Good".into(),
            gain_db: -18.0,
            hotkey: "F2".into(),
            tags: vec!["B, only".into()],
        },
    ];
    let library = Library::load(&specs, 44_100, false, false);
    assert_eq!(library.skipped.len(), 1);
    assert_eq!(library.slots.len(), 1);
    let meta = &library.meta[0];
    assert_eq!(meta.source_index, 1);
    assert_eq!(meta.spec.id, "good-id");
    assert_eq!(meta.spec.file, good);
    assert_eq!(meta.spec.hotkey, "F2");
    assert_eq!(meta.spec.tags, ["B, only"]);
    assert_eq!(library.slots[0].gain_db, -18.0);
    assert!((meta.duration_secs - 1.0).abs() < 0.01);
}

#[test]
fn delayed_fault_cannot_override_a_new_session_selection_or_warning() {
    let (host, _) = fixture();
    let host = Arc::new(host);
    let operation = host.lifecycle.lock().unwrap();
    let waiting = host.clone();
    let (done_tx, done_rx) = mpsc::channel();
    let recovery = std::thread::spawn(move || {
        done_tx
            .send(waiting.recover_input_device(1, None).unwrap())
            .unwrap();
    });
    assert!(done_rx.recv_timeout(Duration::from_millis(30)).is_err());
    let new_key = {
        let mut running = host.running.lock().unwrap();
        let session = running.as_mut().unwrap();
        session.generation = 2;
        session.cfg.input_device = Some("user-selected new mic".into());
        session.cfg_key = cfg_fingerprint(&session.cfg);
        session.cfg_key.clone()
    };
    assert!(host.set_device_lost_for(2, Some("new session warning".into())));
    drop(operation);
    assert!(done_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .is_none());
    recovery.join().unwrap();
    assert!(host.restart_audio(1, None).unwrap().is_none());
    assert!(!host.set_device_lost_for(1, Some("stale warning".into())));
    assert!(!host.set_audio_stalled_for(1, Some("stale stall".into())));
    assert_eq!(host.running_cfg_key().unwrap(), new_key);
    assert_eq!(
        host.status().device_lost.as_deref(),
        Some("new session warning")
    );
    assert!(host.status().audio_stalled.is_none());
    assert!(host.status().running);
}

#[test]
fn hotplug_matches_native_identity_even_for_same_named_endpoints() {
    let input = DeviceId::new("coreaudio:mic-uid-A");
    let output = DeviceId::new("coreaudio:out-uid");
    assert_eq!(
        missing_device_ids(&input, &output, &[input.clone(), output.clone()]),
        (false, false)
    );
    // Another device can keep the old display name but cannot stand in for A.
    let other = DeviceId::new("coreaudio:mic-uid-B");
    assert_eq!(
        missing_device_ids(&input, &output, &[other, output.clone()]),
        (true, false)
    );
}

#[test]
fn cancelling_during_preview_decode_cannot_open_a_late_stream() {
    for cancel in ["stop", "stop-all", "standby", "release", "selftest", "quit"] {
        let host = Arc::new(preview_test_host());
        let (preparing_tx, preparing_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let played = Arc::new(AtomicUsize::new(0));
        let playing = played.clone();
        let decoding = host.clone();
        let worker = std::thread::spawn(move || {
            decoding.launch_preview(
                "decoding-id".into(),
                0.0,
                || {
                    preparing_tx.send(()).unwrap();
                    finish_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                    Ok(())
                },
                move |(), _, _, _| {
                    playing.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                },
            )
        });
        preparing_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        match cancel {
            "stop" => host.stop_preview(),
            "stop-all" => host.stop_all().unwrap(),
            "standby" => host.stop(),
            "release" => host.release(),
            "selftest" => host.with_suspended(None, || Ok(())).unwrap(),
            "quit" => {
                host.begin_shutdown();
            }
            _ => unreachable!(),
        }
        finish_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
        assert_eq!(
            played.load(Ordering::Relaxed),
            0,
            "late preview after {cancel}"
        );
        assert!(host.preview_thread.lock().unwrap().is_none());
    }
}

#[test]
fn selftest_waits_for_preview_close_and_detaches_retained_callback_pcm() {
    let host = preview_test_host();
    let backend = FakeBackend::default();
    let (opened_tx, opened_rx) = mpsc::channel();
    *backend.opened.lock().unwrap() = Some(opened_tx);
    let samples = Arc::new(vec![0.2; 88_200]);
    let weak = Arc::downgrade(&samples);
    let playing = backend.clone();
    host.launch_preview(
        "test-preview".into(),
        0.0,
        || Ok(samples),
        move |samples, token, generation, gain| {
            play_samples(
                playing,
                samples,
                DeviceId::new("fake"),
                StreamFormat::new(44_100, 6),
                gain,
                token,
                generation,
            )
        },
    )
    .unwrap();
    opened_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(weak.upgrade().is_some());
    host.with_suspended(None, || {
        assert!(
            weak.upgrade().is_none(),
            "PCM must be gone before self-test opens streams"
        );
        let mut out = [1.0; 24];
        backend.outputs.lock().unwrap()[0](&mut out);
        assert_eq!(out, [0.0; 24], "retained driver callback must be silent");
        Ok(())
    })
    .unwrap();
    assert!(host.preview_thread.lock().unwrap().is_none());
}

fn launch_fake_preview(host: &AudioHost, backend: &FakeBackend, samples: Arc<Vec<f32>>) {
    let (opened_tx, opened_rx) = mpsc::channel();
    *backend.opened.lock().unwrap() = Some(opened_tx);
    let playing = backend.clone();
    host.launch_preview(
        "good-id".into(),
        0.0,
        || Ok(samples),
        move |samples, token, generation, gain| {
            play_samples(
                playing,
                samples,
                DeviceId::new("fake preview"),
                StreamFormat::new(44_100, 6),
                gain,
                token,
                generation,
            )
        },
    )
    .unwrap();
    opened_rx.recv_timeout(Duration::from_secs(2)).unwrap();
}

fn drain_primary(backend: &FakeBackend) {
    for callback in backend.outputs.lock().unwrap().iter_mut() {
        callback(&mut [0.0; 8_192]);
    }
}

#[test]
fn stop_all_silences_preview_without_a_main_session_and_releases_pcm() {
    let host = preview_test_host();
    let backend = FakeBackend::default();
    let samples = Arc::new(vec![0.2; 88_200]);
    let weak = Arc::downgrade(&samples);
    launch_fake_preview(&host, &backend, samples);
    let mut output = [0.0; 600];
    backend.outputs.lock().unwrap()[0](&mut output);
    assert_eq!(output[594], 0.2);
    host.stop_all().unwrap();
    backend.outputs.lock().unwrap()[0](&mut output);
    assert!(
        output.iter().all(|&sample| sample == 0.0),
        "bottom stop must silence the preview callback, got {output:?}"
    );
    assert!(
        weak.upgrade().is_none(),
        "stop must close the preview owner and release PCM"
    );
    assert!(host.preview_thread.lock().unwrap().is_none());
}

#[test]
fn live_gain_changes_preview_pcm_without_restarting_the_cursor() {
    let (host, main_backend) = fixture();
    let preview_backend = FakeBackend::default();
    let samples = Arc::new(
        (0..44_100)
            .flat_map(|frame| [if frame < 100 { 0.2 } else { 0.4 }; 2])
            .collect(),
    );
    launch_fake_preview(&host, &preview_backend, samples);
    drain_primary(&main_backend);
    let mut output = [0.0; 600];
    preview_backend.outputs.lock().unwrap()[0](&mut output);
    assert!((output[594] - 0.2).abs() < 1e-6);
    assert!(host.set_slot_gain(0, -20.0).unwrap());
    preview_backend.outputs.lock().unwrap()[0](&mut output);
    assert!(
        (output[594] - 0.04).abs() < 1e-6,
        "dragging gain must update the next preview PCM frame without rewinding, got {}",
        output[594]
    );
    assert_eq!(
        preview_backend.outputs.lock().unwrap().len(),
        1,
        "gain updates must not reopen playback"
    );
    assert!(host.play(0).unwrap());
    for callback in main_backend.outputs.lock().unwrap().iter_mut() {
        let mut output = vec![0.0; 8_820];
        callback(&mut output);
        assert!(
            output[output.len() - 1_000..]
                .iter()
                .all(|&sample| (sample - 0.02).abs() < 1e-6),
            "main output and monitor must retain the same live gain"
        );
    }
}

#[test]
fn preview_gain_works_without_main_session_and_cannot_change_another_sound() {
    let host = preview_test_host();
    let backend = FakeBackend::default();
    launch_fake_preview(&host, &backend, Arc::new(vec![0.2; 88_200]));
    assert!(!host.set_sound_gain("another-sound-id", -20.0).unwrap());
    let mut output = [0.0; 600];
    backend.outputs.lock().unwrap()[0](&mut output);
    assert_eq!(output[594], 0.2, "another card must not alter this preview");
    assert!(host.set_sound_gain("good-id", -20.0).unwrap());
    backend.outputs.lock().unwrap()[0](&mut output);
    assert!((output[594] - 0.02).abs() < 1e-6);
    for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::MAX] {
        assert!(host.set_sound_gain("good-id", invalid).is_err());
    }
    backend.outputs.lock().unwrap()[0](&mut output);
    assert!(
        (output[594] - 0.02).abs() < 1e-6,
        "invalid gains must preserve the valid PCM gain"
    );
    host.stop_all().unwrap();
    assert!(!host.set_sound_gain("good-id", -10.0).unwrap());
}

#[test]
fn gain_adjusted_during_decode_is_applied_to_the_first_preview_frame() {
    let host = Arc::new(preview_test_host());
    let backend = FakeBackend::default();
    let (prepared_tx, prepared_rx) = mpsc::channel();
    let (continue_tx, continue_rx) = mpsc::channel();
    let (opened_tx, opened_rx) = mpsc::channel();
    *backend.opened.lock().unwrap() = Some(opened_tx);
    let preparing = host.clone();
    let playing = backend.clone();
    let thread = std::thread::spawn(move || {
        preparing.launch_preview(
            "decoding-sound".into(),
            0.0,
            || {
                prepared_tx.send(()).unwrap();
                continue_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                Ok(Arc::new(vec![0.2; 88_200]))
            },
            move |samples, token, generation, gain| {
                play_samples(
                    playing,
                    samples,
                    DeviceId::new("fake preview"),
                    StreamFormat::new(44_100, 6),
                    gain,
                    token,
                    generation,
                )
            },
        )
    });
    prepared_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(host.set_sound_gain("decoding-sound", -20.0).unwrap());
    continue_tx.send(()).unwrap();
    thread.join().unwrap().unwrap();
    opened_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let mut output = [0.0; 600];
    backend.outputs.lock().unwrap()[0](&mut output);
    assert!((output[594] - 0.02).abs() < 1e-6);
    host.stop_all().unwrap();
}

#[test]
fn standby_stops_preview_primary_and_monitor_but_keeps_passthrough_session() {
    let (host, main_backend) = fixture();
    let preview_backend = FakeBackend::default();
    let samples = Arc::new(vec![0.2; 88_200]);
    let weak = Arc::downgrade(&samples);
    launch_fake_preview(&host, &preview_backend, samples);
    assert!(host.play(0).unwrap());
    for callback in main_backend.outputs.lock().unwrap().iter_mut() {
        let mut output = vec![0.0; 4_096];
        callback(&mut output);
        assert!(output.iter().any(|&sample| sample > 0.05));
    }
    host.stop();
    assert!(!host.status().running);
    assert!(host.status().passthrough);
    assert!(weak.upgrade().is_none());
    let mut output = [1.0; 600];
    preview_backend.outputs.lock().unwrap()[0](&mut output);
    assert_eq!(output, [0.0; 600]);
    for callback in main_backend.outputs.lock().unwrap().iter_mut() {
        let mut output = vec![0.0; 4_096];
        callback(&mut output);
        assert!(output[2_048..].iter().all(|&sample| sample == 0.0));
    }
}

#[test]
fn stale_remote_stop_cannot_cancel_new_session_preview() {
    let (host, main_backend) = fixture();
    let playback = host
        .running
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .playback
        .clone();
    let stale_stop = host.remote_stop_handler(playback.clone(), 1);
    host.running.lock().unwrap().as_mut().unwrap().generation = 2;
    host.audio_requests.register_session(2);
    let backend = FakeBackend::default();
    launch_fake_preview(&host, &backend, Arc::new(vec![0.2; 88_200]));
    drain_primary(&main_backend);
    stale_stop();
    let mut output = [0.0; 600];
    backend.outputs.lock().unwrap()[0](&mut output);
    assert_eq!(
        output[594], 0.2,
        "old remote handler must not silence a new session's preview"
    );
    host.remote_stop_handler(playback, 2)();
    backend.outputs.lock().unwrap()[0](&mut output);
    assert_eq!(
        output, [0.0; 600],
        "current remote stop cancels preview PCM immediately"
    );
    host.stop_all().unwrap();
}

#[test]
fn primary_play_interrupts_an_independent_preview_pcm() {
    let (host, main_backend) = fixture();
    let backend = FakeBackend::default();
    launch_fake_preview(&host, &backend, Arc::new(vec![0.3; 88_200]));
    drain_primary(&main_backend);
    let mut output = [0.0; 600];
    backend.outputs.lock().unwrap()[0](&mut output);
    assert_eq!(output[594], 0.3);
    assert!(host.play(0).unwrap());
    backend.outputs.lock().unwrap()[0](&mut output);
    assert_eq!(
        output, [0.0; 600],
        "a new main trigger must stop local preview too"
    );
    host.stop_all().unwrap();
}

#[test]
fn independent_preview_interrupts_primary_and_monitor_pcm() {
    let (host, backend) = fixture();
    assert!(host.play(0).unwrap());
    for callback in backend.outputs.lock().unwrap().iter_mut() {
        let mut output = [0.0; 8_192];
        callback(&mut output);
        assert!((output[8_000] - 0.2).abs() < 1e-6);
    }
    let preview = FakeBackend::default();
    launch_fake_preview(&host, &preview, Arc::new(vec![0.3; 88_200]));
    let mut preview_pcm = [1.0; 600];
    preview.outputs.lock().unwrap()[0](&mut preview_pcm);
    assert_eq!(
        preview_pcm, [0.0; 600],
        "preview waits for preceding PCM to retire"
    );
    assert_eq!(host.status().preview_playhead_secs, 0.0);
    let mut callbacks = backend.outputs.lock().unwrap();
    for (index, callback) in callbacks.iter_mut().enumerate() {
        let mut output = [0.0; 8_192];
        callback(&mut output);
        assert!(
            output[4_096..].iter().all(|&sample| sample == 0.0),
            "preview must replace both primary playback and monitoring"
        );
        preview.outputs.lock().unwrap()[0](&mut preview_pcm);
        if index == 0 {
            assert_eq!(preview_pcm, [0.0; 600], "monitor must confirm stop too");
        } else {
            assert_eq!(preview_pcm[594], 0.3);
        }
    }
    drop(callbacks);
    host.stop_all().unwrap();
}

#[test]
fn every_main_trigger_cancels_preview_but_invalid_and_stale_triggers_preserve_it() {
    for trigger in ["card", "hotkey", "phone"] {
        let (host, main_backend) = fixture();
        let backend = FakeBackend::default();
        launch_fake_preview(&host, &backend, Arc::new(vec![0.3; 88_200]));
        drain_primary(&main_backend);
        let mut pcm = [0.0; 600];
        let playback = host
            .running
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .playback
            .clone();
        assert!(!host.play(1).unwrap());
        assert!(!host.play_handler(playback.clone(), 0)(0));
        backend.outputs.lock().unwrap()[0](&mut pcm);
        assert_eq!(pcm[594], 0.3, "invalid requests must preserve preview");
        let accepted = match trigger {
            "card" => host.play(0).unwrap(),
            "hotkey" => host.play_handler(playback, 1)(0),
            "phone" => host.remote.lock().unwrap().as_ref().unwrap().play(0),
            _ => unreachable!(),
        };
        assert!(accepted, "{trigger}");
        backend.outputs.lock().unwrap()[0](&mut pcm);
        assert_eq!(pcm, [0.0; 600], "{trigger} must cancel preview");
        assert!(host.status().preview_sound_id.is_none());
        host.stop_all().unwrap();
    }
}

#[test]
fn preview_status_tracks_consumed_pcm_and_replacement_without_a_main_session() {
    let host = preview_test_host();
    let backend = FakeBackend::default();
    launch_fake_preview(&host, &backend, Arc::new(vec![0.2; 88_200]));
    let status = host.status();
    assert_eq!(status.preview_sound_id.as_deref(), Some("good-id"));
    assert!(!status.preview_playing);
    assert_eq!(status.preview_duration_secs, 1.0);
    backend.outputs.lock().unwrap()[0](&mut [0.0; 26_460]);
    let status = host.status();
    assert!(status.preview_playing);
    assert!((status.preview_playhead_secs - 0.1).abs() < 1e-6);
    launch_fake_preview(&host, &backend, Arc::new(vec![0.4; 88_200]));
    let mut pcm = [1.0; 600];
    backend.outputs.lock().unwrap()[0](&mut pcm);
    assert_eq!(
        pcm, [0.0; 600],
        "replacement silences the old preview callback"
    );
    backend.outputs.lock().unwrap()[1](&mut pcm);
    assert_eq!(pcm[594], 0.4);
    backend.outputs.lock().unwrap()[1](&mut vec![0.0; 264_600]);
    assert!(
        !host.status().preview_playing,
        "natural EOF clears playing state"
    );
    host.stop_all().unwrap();
    assert!(host.status().preview_sound_id.is_none());
}

#[test]
fn looping_preference_does_not_make_preview_repeat() {
    let (host, primary) = fixture();
    assert!(host.set_looping(true));
    let backend = FakeBackend::default();
    launch_fake_preview(&host, &backend, Arc::new(vec![0.3; 8_820]));
    drain_primary(&primary);
    let mut pcm = [0.0; 2_646];
    backend.outputs.lock().unwrap()[0](&mut pcm);
    assert_eq!(pcm[594], 0.3);
    assert!(host.set_looping(false));
    assert!(host.set_looping(true));
    let mut tail = vec![0.0; 52_920];
    backend.outputs.lock().unwrap()[0](&mut tail);
    assert!(
        tail[26_460..].iter().all(|&v| v == 0.0),
        "preview must end once despite loop preference"
    );
    assert!(!host.status().preview_playing);
    host.stop_all().unwrap();
}

#[test]
fn delayed_stop_ack_does_not_advance_or_expire_preview_audio() {
    let (host, primary) = fixture();
    let backend = FakeBackend::default();
    launch_fake_preview(&host, &backend, Arc::new(vec![0.3; 8_820]));
    let mut pcm = [1.0; 600];
    backend.outputs.lock().unwrap()[0](&mut pcm);
    assert_eq!(pcm, [0.0; 600]);
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(host.status().preview_playhead_secs, 0.0);
    drain_primary(&primary);
    backend.outputs.lock().unwrap()[0](&mut pcm);
    assert_eq!(
        pcm[594], 0.3,
        "waiting longer than sample duration must not truncate PCM"
    );
    backend.outputs.lock().unwrap()[0](&mut vec![0.0; 26_436]);
    assert!((host.status().preview_playhead_secs - 0.1).abs() < 1e-6);
    host.stop_all().unwrap();
}

#[test]
fn a_main_trigger_during_preview_decode_cannot_start_a_late_preview() {
    let (host, _) = fixture();
    let host = Arc::new(host);
    let (prepared_tx, prepared_rx) = mpsc::channel();
    let (continue_tx, continue_rx) = mpsc::channel();
    let played = Arc::new(AtomicUsize::new(0));
    let preparing = host.clone();
    let playing = played.clone();
    let worker = std::thread::spawn(move || {
        preparing.launch_preview(
            "late-preview".into(),
            0.0,
            || {
                prepared_tx.send(()).unwrap();
                continue_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                Ok(())
            },
            move |_, _, _, _| {
                playing.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        )
    });
    prepared_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(host.play(0).unwrap());
    continue_tx.send(()).unwrap();
    worker.join().unwrap().unwrap();
    assert_eq!(played.load(Ordering::Relaxed), 0);
    assert!(host.status().preview_sound_id.is_none());
}

#[test]
fn rejected_main_play_preserves_the_current_preview() {
    let (host, primary) = fixture();
    let backend = FakeBackend::default();
    launch_fake_preview(&host, &backend, Arc::new(vec![0.3; 88_200]));
    drain_primary(&primary);
    let control = host
        .running
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .playback
        .control
        .clone();
    let mut filled = false;
    for _ in 0..1_024 {
        if !control.set_slot_gain(0, 0.0) {
            filled = true;
            break;
        }
    }
    assert!(filled, "exercise a genuinely full producer queue");
    assert!(!host.play(0).unwrap());
    let mut pcm = [0.0; 600];
    backend.outputs.lock().unwrap()[0](&mut pcm);
    assert_eq!(
        pcm[594], 0.3,
        "rejected B must not cancel a valid preview A"
    );
    assert!(host.status().preview_playing);
    drain_primary(&primary);
    assert!(host.play(0).unwrap());
    backend.outputs.lock().unwrap()[0](&mut pcm);
    assert_eq!(pcm, [0.0; 600]);
    host.stop_all().unwrap();
}

#[test]
fn main_pcm_waits_until_the_previous_preview_stream_has_closed() {
    let (host, primary) = fixture();
    let host = Arc::new(host);
    let backend = FakeBackend::default();
    let (close_tx, close_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *backend.close_barrier.lock().unwrap() = Some((close_tx, Arc::new(Mutex::new(release_rx))));
    launch_fake_preview(&host, &backend, Arc::new(vec![0.3; 88_200]));
    drain_primary(&primary);
    let mut preview_pcm = [0.0; 600];
    backend.outputs.lock().unwrap()[0](&mut preview_pcm);
    assert_eq!(preview_pcm[594], 0.3);
    let switching = host.clone();
    let worker = std::thread::spawn(move || switching.play(0));
    close_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let mut leaked_new_pcm = false;
    for callback in primary.outputs.lock().unwrap().iter_mut() {
        let mut pcm = [0.0; 8_192];
        callback(&mut pcm);
        leaked_new_pcm |= pcm.iter().any(|&sample| sample != 0.0);
    }
    release_tx.send(()).unwrap();
    assert!(worker.join().unwrap().unwrap());
    assert!(
        !leaked_new_pcm,
        "new PCM must be withheld while old preview stream is closing"
    );
    for callback in primary.outputs.lock().unwrap().iter_mut() {
        let mut pcm = [0.0; 8_192];
        callback(&mut pcm);
        assert!((pcm[8_000] - 0.2).abs() < 1e-6);
    }
    backend.outputs.lock().unwrap()[0](&mut preview_pcm);
    assert_eq!(
        preview_pcm, [0.0; 600],
        "retained old preview callback is detached"
    );
    host.stop_all().unwrap();
}

#[test]
fn a_lifecycle_caller_closing_preview_cannot_be_bypassed_by_main_play() {
    for action in ["stop", "replace"] {
        let (host, primary) = fixture();
        let host = Arc::new(host);
        let backend = FakeBackend::default();
        let (close_tx, close_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        *backend.close_barrier.lock().unwrap() = Some((close_tx, Arc::new(Mutex::new(release_rx))));
        launch_fake_preview(&host, &backend, Arc::new(vec![0.3; 88_200]));
        drain_primary(&primary);
        backend.outputs.lock().unwrap()[0](&mut [0.0; 600]);
        let closing = host.clone();
        let closer = std::thread::spawn(move || {
            if action == "stop" {
                closing.stop_preview();
            } else {
                closing
                    .launch_preview("new-preview".into(), 0.0, || Ok(()), |_, _, _, _| Ok(()))
                    .unwrap();
            }
        });
        close_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (trying_tx, trying_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let switching = host.clone();
        let player = std::thread::spawn(move || {
            trying_tx.send(()).unwrap();
            let result = switching.play(0);
            done_tx.send(()).unwrap();
            result
        });
        trying_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let returned_before_close = done_rx.recv_timeout(Duration::from_millis(50)).is_ok();
        let mut leaked_new_pcm = false;
        for callback in primary.outputs.lock().unwrap().iter_mut() {
            let mut pcm = [0.0; 8_192];
            callback(&mut pcm);
            leaked_new_pcm |= pcm.iter().any(|&sample| sample != 0.0);
        }
        release_tx.send(()).unwrap();
        closer.join().unwrap();
        assert!(player.join().unwrap().unwrap());
        assert!(
            !returned_before_close,
            "{action}: acceptance must wait for stream closure"
        );
        assert!(
            !leaked_new_pcm,
            "{action} must hold the gate until the old stream closes"
        );
        for callback in primary.outputs.lock().unwrap().iter_mut() {
            let mut pcm = [0.0; 8_192];
            callback(&mut pcm);
            assert!((pcm[8_000] - 0.2).abs() < 1e-6);
        }
        host.stop_all().unwrap();
    }
}

#[test]
fn combo_stop_supersedes_an_older_main_request_waiting_for_dispatch() {
    let (host, backend) = fixture();
    let host = Arc::new(host);
    let ticket = host.reserve_audio_request();
    let (queued_tx, queued_rx) = mpsc::channel();
    let (dispatch_tx, dispatch_rx) = mpsc::channel();
    let delayed = host.clone();
    let worker = std::thread::spawn(move || {
        queued_tx.send(()).unwrap();
        dispatch_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        delayed.play_requested(0, ticket)
    });
    queued_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    host.stop_all().unwrap();
    drain_primary(&backend);
    dispatch_tx.send(()).unwrap();
    let accepted = worker.join().unwrap().unwrap();
    let mut leaked = false;
    for callback in backend.outputs.lock().unwrap().iter_mut() {
        let mut pcm = [0.0; 8_192];
        callback(&mut pcm);
        leaked |= pcm.iter().any(|&sample| sample.abs() > 0.05);
    }
    host.stop_all().unwrap();
    assert!(
        !accepted && !leaked,
        "an older queued play must not undo the later stop"
    );
}

#[test]
fn combo_stop_supersedes_an_older_preview_request_waiting_for_dispatch() {
    let (host, primary) = fixture();
    let host = Arc::new(host);
    let ticket = host.reserve_audio_request();
    let backend = FakeBackend::default();
    let (queued_tx, queued_rx) = mpsc::channel();
    let (dispatch_tx, dispatch_rx) = mpsc::channel();
    let delayed = host.clone();
    let output = backend.clone();
    let worker = std::thread::spawn(move || {
        queued_tx.send(()).unwrap();
        dispatch_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // This is the same host boundary as a queued Tauri preview command.
        delayed
            .launch_preview_requested(
                "late-preview".into(),
                0.0,
                ticket,
                || Ok(Arc::new(vec![0.3; 88_200])),
                move |samples, token, generation, audio| {
                    play_samples(
                        output,
                        samples,
                        DeviceId::new("fake preview"),
                        StreamFormat::new(44_100, 6),
                        audio,
                        token,
                        generation,
                    )
                },
            )
            .unwrap();
    });
    queued_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    host.stop_all().unwrap();
    drain_primary(&primary);
    dispatch_tx.send(()).unwrap();
    worker.join().unwrap();
    drain_primary(&primary);
    let mut leaked = false;
    for callback in backend.outputs.lock().unwrap().iter_mut() {
        let mut pcm = [0.0; 600];
        callback(&mut pcm);
        leaked |= pcm.iter().any(|&sample| sample.abs() > 0.05);
    }
    host.stop_all().unwrap();
    assert!(
        !leaked,
        "an older queued preview must not open a late audible stream after stop"
    );
}

#[test]
fn combo_newer_card_hotkey_phone_preview_or_shutdown_retires_queued_audio() {
    for looping in [false, true] {
        for action in [
            "card",
            "hotkey",
            "phone",
            "preview",
            "stop",
            "phone-stop",
            "standby",
            "release",
        ] {
            let (host, main) = fixture();
            host.set_looping(looping);
            let stale = host.reserve_audio_request();
            let preview = FakeBackend::default();
            let playback = host
                .running
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .playback
                .clone();
            let remote = host.remote.lock().unwrap().as_ref().unwrap().clone();
            match action {
                "card" => assert!(host.play(0).unwrap()),
                "hotkey" => assert!(host.play_handler(playback, 1)(0)),
                "phone" => assert!(remote.play(0)),
                "preview" => launch_fake_preview(&host, &preview, Arc::new(vec![0.3; 88_200])),
                "stop" => host.stop_all().unwrap(),
                "phone-stop" => remote.stop_all(),
                "standby" => host.stop(),
                "release" => host.release(),
                _ => unreachable!(),
            }
            let opened_before = preview.outputs.lock().unwrap().len();
            assert!(
                !host.play_requested(0, stale).unwrap(),
                "{action}: reject late main"
            );
            let prepared = AtomicBool::new(false);
            host.launch_preview_requested(
                "late-id".into(),
                0.0,
                stale,
                || {
                    prepared.store(true, Ordering::Release);
                    Ok(())
                },
                |(), _, _, _| panic!("stale preview cannot open any output"),
            )
            .unwrap();
            assert!(
                !prepared.load(Ordering::Acquire),
                "{action}: reject before decode"
            );
            assert_eq!(preview.outputs.lock().unwrap().len(), opened_before);
            for callback in main.outputs.lock().unwrap().iter_mut() {
                let mut pcm = [0.0; 8_192];
                callback(&mut pcm);
                let expected = if ["card", "hotkey", "phone"].contains(&action) {
                    0.2
                } else {
                    0.0
                };
                assert!(pcm[8_000..].iter().all(|&s| (s - expected).abs() < 1e-6),
                    "{action}, loop={looping}: late work must preserve the newer main / monitor PCM");
            }
            if action == "preview" {
                let mut pcm = [0.0; 600];
                preview.outputs.lock().unwrap()[0](&mut pcm);
                assert_eq!(pcm[594], 0.3, "new preview survives late old requests");
                assert_eq!(host.status().preview_sound_id.as_deref(), Some("good-id"));
            }
            host.stop_all().unwrap();
        }
    }
}

#[test]
fn combo_stop_reserved_during_preview_close_prevents_late_main_pcm() {
    let (host, main) = fixture();
    let host = Arc::new(host);
    let preview = FakeBackend::default();
    let (close_tx, close_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *preview.close_barrier.lock().unwrap() = Some((close_tx, Arc::new(Mutex::new(release_rx))));
    let samples = Arc::new(vec![0.3; 88_200]);
    let weak = Arc::downgrade(&samples);
    launch_fake_preview(&host, &preview, samples);
    drain_primary(&main);
    let mut preview_pcm = [0.0; 600];
    preview.outputs.lock().unwrap()[0](&mut preview_pcm);
    assert_eq!(preview_pcm[594], 0.3);
    let old = host.reserve_audio_request();
    let switching = host.clone();
    let worker = std::thread::spawn(move || switching.play_requested(0, old));
    close_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    // Reserving is nonblocking even while a driver is finishing stream closure.
    let stop = host.reserve_audio_request();
    release_tx.send(()).unwrap();
    assert!(!worker.join().unwrap().unwrap());
    host.stop_all_requested(stop).unwrap();
    for callback in main.outputs.lock().unwrap().iter_mut() {
        let mut pcm = [1.0; 8_192];
        callback(&mut pcm);
        assert!(
            pcm.iter().all(|&s| s == 0.0),
            "neither main nor monitor may receive stale Play"
        );
    }
    preview.outputs.lock().unwrap()[0](&mut preview_pcm);
    assert_eq!(preview_pcm, [0.0; 600]);
    assert!(
        weak.upgrade().is_none(),
        "closing preview relinquishes PCM before returning"
    );
}

#[test]
fn combo_late_stop_standby_release_or_start_cannot_undo_a_newer_play() {
    for action in ["stop", "preview-stop", "standby", "release", "start"] {
        let (host, main) = fixture();
        let old = host.reserve_audio_request();
        assert!(host.play(0).unwrap());
        match action {
            "stop" => host.stop_all_requested(old).unwrap(),
            "preview-stop" => host.stop_preview_requested(old),
            "standby" => host.stop_requested(old),
            "release" => host.release_requested(old),
            "start" => host.start_requested(config(), None, old).unwrap(),
            _ => unreachable!(),
        }
        assert!(
            host.status().running,
            "late {action} must not remove current session"
        );
        for callback in main.outputs.lock().unwrap().iter_mut() {
            let mut pcm = [0.0; 8_192];
            callback(&mut pcm);
            assert!(
                pcm[8_000..].iter().all(|&s| (s - 0.2).abs() < 1e-6),
                "late {action} must leave the actual current audio playing"
            );
        }
        host.stop_all().unwrap();
    }
}

#[test]
fn combo_standby_preview_loop_edit_and_reactivation_keep_one_audible_owner() {
    let (host, main) = fixture();
    assert!(host.set_looping(true));
    assert!(host.play(0).unwrap());
    drain_primary(&main);
    host.stop();
    let preview = FakeBackend::default();
    let samples = Arc::new(vec![0.3; 88_200]);
    let weak = Arc::downgrade(&samples);
    launch_fake_preview(&host, &preview, samples);
    drain_primary(&main);
    assert!(!host.play(0).unwrap());
    assert!(!host.remote.lock().unwrap().as_ref().unwrap().play(0));
    let mut pcm = [0.0; 600];
    preview.outputs.lock().unwrap()[0](&mut pcm);
    assert_eq!(pcm[594], 0.3, "rejected standby requests preserve preview");
    assert!(host.set_looping(false));
    host.activate(None).unwrap();
    assert!(host.remote.lock().unwrap().as_ref().unwrap().play(0));
    preview.outputs.lock().unwrap()[0](&mut pcm);
    assert_eq!(pcm, [0.0; 600]);
    assert!(
        weak.upgrade().is_none(),
        "main resumes only after preview gives up PCM"
    );
    for callback in main.outputs.lock().unwrap().iter_mut() {
        let mut pcm = vec![0.0; 100_000];
        callback(&mut pcm);
        assert!(pcm[..44_000].iter().any(|&s| s > 0.1));
        assert!(
            pcm[90_000..].iter().all(|&s| s == 0.0),
            "loop edit applies after standby"
        );
    }
    host.release();
    for callback in main.outputs.lock().unwrap().iter_mut() {
        let mut pcm = [1.0; 8_192];
        callback(&mut pcm);
        assert_eq!(pcm, [0.0; 8_192], "released callback has no audible owner");
    }
}

#[test]
fn combo_rejected_main_request_preserves_a_preview_still_decoding() {
    for rejection in ["invalid", "standby", "gui-invalid", "gui-full"] {
        let (host, main) = fixture();
        let host = Arc::new(host);
        if rejection == "standby" {
            host.stop();
        }
        let preview = FakeBackend::default();
        let (opened_tx, opened_rx) = mpsc::channel();
        *preview.opened.lock().unwrap() = Some(opened_tx);
        let (decoding_tx, decoding_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let preparing = host.clone();
        let output = preview.clone();
        let worker = std::thread::spawn(move || {
            preparing.launch_preview(
                "good-id".into(),
                0.0,
                || {
                    decoding_tx.send(()).unwrap();
                    finish_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                    Ok(Arc::new(vec![0.3; 88_200]))
                },
                move |samples, token, generation, audio| {
                    play_samples(
                        output,
                        samples,
                        DeviceId::new("fake preview"),
                        StreamFormat::new(44_100, 6),
                        audio,
                        token,
                        generation,
                    )
                },
            )
        });
        decoding_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        if rejection == "gui-full" {
            let control = host
                .running
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .playback
                .control
                .clone();
            let mut full = false;
            for _ in 0..1_024 {
                if !control.set_slot_gain(0, 0.0) {
                    full = true;
                    break;
                }
            }
            assert!(full, "use real producer backpressure");
        }
        let accepted = if rejection.starts_with("gui-") {
            let ticket = host.reserve_audio_request();
            host.play_requested(if rejection == "gui-invalid" { 99 } else { 0 }, ticket)
        } else {
            host.play(if rejection == "standby" { 0 } else { 1 })
        };
        assert!(!accepted.unwrap());
        finish_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
        let opened = opened_rx.recv_timeout(Duration::from_millis(100)).is_ok();
        drain_primary(&main);
        let mut audible = false;
        for callback in preview.outputs.lock().unwrap().iter_mut() {
            let mut pcm = [0.0; 600];
            callback(&mut pcm);
            audible |= pcm[594] == 0.3;
        }
        host.stop_all().unwrap();
        assert!(
            opened && audible,
            "{rejection}: rejected main must preserve a decoding preview"
        );
    }
}

#[test]
fn combo_cancelled_same_config_start_keeps_the_existing_passthrough_owner() {
    let (host, main) = fixture();
    let host = Arc::new(host);
    let playback = host
        .running
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .playback
        .clone();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let blocker = std::thread::spawn(move || {
        playback.play_with_checked_barrier(0, || {
            entered_tx.send(()).unwrap();
            finish_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            false
        })
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let start = host.reserve_audio_request();
    let starting = host.clone();
    let worker = std::thread::spawn(move || starting.start_requested(config(), None, start));
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while host.lifecycle.try_lock().is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "start must enter lifecycle before cancellation"
        );
        std::thread::yield_now();
    }
    let stop = host.reserve_audio_request();
    finish_tx.send(()).unwrap();
    assert!(!blocker.join().unwrap());
    worker.join().unwrap().unwrap();
    host.stop_all_requested(stop).unwrap();
    let passthrough = host.status().passthrough;
    let accepted = host.play(0).is_ok_and(|ok| ok);
    let mut audible = true;
    for callback in main.outputs.lock().unwrap().iter_mut() {
        let mut pcm = [0.0; 8_192];
        callback(&mut pcm);
        audible &= pcm[8_000..].iter().all(|&s| (s - 0.2).abs() < 1e-6);
    }
    host.stop_all().unwrap();
    assert!(
        passthrough && accepted && audible,
        "cancelling an activation must not release the already owned mic / monitor session"
    );
}

#[test]
fn combo_phone_stop_play_and_hotkey_arrive_before_a_slow_preview_close_finishes() {
    for route in ["phone-stop", "phone-play", "hotkey"] {
        let (host, main) = fixture();
        let host = Arc::new(host);
        let preview = FakeBackend::default();
        let (close_tx, close_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        *preview.close_barrier.lock().unwrap() = Some((close_tx, Arc::new(Mutex::new(release_rx))));
        launch_fake_preview(&host, &preview, Arc::new(vec![0.3; 88_200]));
        drain_primary(&main);
        let old = host.reserve_audio_request();
        let switching = host.clone();
        let gui = std::thread::spawn(move || switching.play_requested(0, old));
        close_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let remote = host.remote.lock().unwrap().as_ref().unwrap().clone();
        let playback = host
            .running
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .playback
            .clone();
        let hotkey = host.play_handler(playback, 1);
        let (arrived_tx, arrived_rx) = mpsc::channel();
        let latest = std::thread::spawn(move || {
            arrived_tx.send(()).unwrap();
            match route {
                "phone-stop" => {
                    remote.stop_all();
                    true
                }
                "phone-play" => remote.play(0),
                "hotkey" => hotkey(0),
                _ => unreachable!(),
            }
        });
        arrived_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(100);
        while host.audio_requests.current.load(Ordering::Acquire) == old
            && std::time::Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        let reserved_before_close = host.audio_requests.current.load(Ordering::Acquire) > old;
        release_tx.send(()).unwrap();
        let old_accepted = gui.join().unwrap().unwrap();
        assert!(latest.join().unwrap());
        assert!(
            reserved_before_close && !old_accepted,
            "{route} must retire old GUI intent before waiting for stream close"
        );
        for callback in main.outputs.lock().unwrap().iter_mut() {
            let mut pcm = [0.0; 8_192];
            callback(&mut pcm);
            let expected = if route == "phone-stop" { 0.0 } else { 0.2 };
            assert!(
                pcm[8_000..].iter().all(|&s| (s - expected).abs() < 1e-6),
                "{route}: both output renderers must obey the latest accepted route"
            );
        }
        host.stop_all().unwrap();
    }
}

#[test]
fn combo_session_publication_rejects_old_start_but_transfers_committed_owner_to_new_actions() {
    for cancelled_before_commit in [false, true] {
        let (host, _) = fixture();
        host.release();
        let (prepared_host, prepared_audio) = fixture();
        let mut running = prepared_host.running.lock().unwrap().take().unwrap();
        running.generation = 2;
        let playback = running.playback.clone();
        let shared = host.remote.lock().unwrap().as_ref().unwrap().clone();
        shared.set_routed(
            playback.control.clone(),
            vec!["Good".into()],
            host.play_handler(playback.clone(), 2),
            host.remote_stop_handler(playback, 2),
        );
        let start = host.reserve_audio_request();
        if cancelled_before_commit {
            host.reserve_audio_request();
        }
        let rejected = host.publish_running(running, Some(start));
        if cancelled_before_commit {
            assert!(!host.status().passthrough);
            shared.clear();
            host.close_running(rejected.expect("a superseded start cannot publish its owner"));
            for callback in prepared_audio.outputs.lock().unwrap().iter_mut() {
                let mut pcm = [1.0; 8_192];
                callback(&mut pcm);
                assert_eq!(
                    pcm, [0.0; 8_192],
                    "uncommitted owner must close all callback shells"
                );
            }
        } else {
            assert!(rejected.is_none());
            assert!(
                shared.play(0),
                "new phone request can use the committed session"
            );
            assert!(
                !host.audio_request_is_current(start),
                "phone supersedes returning start worker"
            );
            assert!(
                host.status().passthrough,
                "committed owner remains the host's responsibility"
            );
            for callback in prepared_audio.outputs.lock().unwrap().iter_mut() {
                let mut pcm = [0.0; 8_192];
                callback(&mut pcm);
                assert!(
                    pcm[8_000..].iter().all(|&s| (s - 0.2).abs() < 1e-6),
                    "phone owns current main / monitor PCM after start commit"
                );
            }
        }
        host.release();
    }
}

#[test]
fn combo_retired_hotkey_and_phone_callbacks_do_not_cancel_new_session_decode() {
    let (host, main) = fixture();
    let host = Arc::new(host);
    let playback = host
        .running
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .playback
        .clone();
    let retired_play = host.play_handler(playback.clone(), 1);
    let retired_stop = host.remote_stop_handler(playback, 1);
    host.running.lock().unwrap().as_mut().unwrap().generation = 2;
    host.audio_requests.register_session(2);
    let preview = FakeBackend::default();
    let (opened_tx, opened_rx) = mpsc::channel();
    *preview.opened.lock().unwrap() = Some(opened_tx);
    let (decoding_tx, decoding_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let preparing = host.clone();
    let output = preview.clone();
    let worker = std::thread::spawn(move || {
        preparing.launch_preview(
            "new-id".into(),
            0.0,
            || {
                decoding_tx.send(()).unwrap();
                finish_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                Ok(Arc::new(vec![0.3; 88_200]))
            },
            move |samples, token, generation, audio| {
                play_samples(
                    output,
                    samples,
                    DeviceId::new("fake preview"),
                    StreamFormat::new(44_100, 6),
                    audio,
                    token,
                    generation,
                )
            },
        )
    });
    decoding_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let current = host.audio_requests.current.load(Ordering::Acquire);
    assert!(!retired_play(0));
    retired_stop();
    assert_eq!(
        host.audio_requests.current.load(Ordering::Acquire),
        current,
        "retired session callbacks cannot even supersede the new request"
    );
    finish_tx.send(()).unwrap();
    worker.join().unwrap().unwrap();
    opened_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    drain_primary(&main);
    let mut pcm = [0.0; 600];
    preview.outputs.lock().unwrap()[0](&mut pcm);
    assert_eq!(pcm[594], 0.3);
    assert_eq!(host.status().preview_sound_id.as_deref(), Some("new-id"));
    host.stop_all().unwrap();
}

#[test]
fn 未启动试听采用默认音效总音量并实时叠加卡片增益() {
    let host = AudioHost::new();
    let backend = FakeBackend::default();
    launch_fake_preview(&host, &backend, Arc::new(vec![0.2; 88_200]));
    let mut pcm = vec![0.0; 6 * 500];
    backend.outputs.lock().unwrap()[0](&mut pcm);
    let tail = pcm.len() - 6;
    assert!((pcm[tail] - 0.2 * 10.0f32.powf(-6.0 / 20.0)).abs() < 1e-6);
    let before = host.status().preview_playhead_secs;
    host.set_gains(-20.0, -12.0);
    assert!(host.set_sound_gain("good-id", -8.0).unwrap());
    backend.outputs.lock().unwrap()[0](&mut pcm);
    assert!((pcm[tail] - 0.02).abs() < 1e-6);
    assert!(host.status().preview_playhead_secs > before);
    assert_eq!(backend.outputs.lock().unwrap().len(), 1);
    host.set_gains(12.0, -12.0);
    backend.outputs.lock().unwrap()[0](&mut pcm);
    assert!((pcm[tail] - 0.02).abs() < 1e-6, "麦克风音量不影响试听");
    host.stop_all().unwrap();
    host.set_gains(0.0, 6.0);
    backend.outputs.lock().unwrap()[0](&mut pcm);
    assert!(pcm.iter().all(|sample| *sample == 0.0));
    launch_fake_preview(&host, &backend, Arc::new(vec![0.2; 88_200]));
    backend.outputs.lock().unwrap()[1](&mut pcm);
    assert!((pcm[tail] - 0.2 * 10.0f32.powf(6.0 / 20.0)).abs() < 1e-6);
    host.stop_all().unwrap();
}

#[test]
fn 总音量和卡片音量对试听正式播放及监听产生相同样本增益() {
    let (host, primary) = fixture();
    let preview = FakeBackend::default();
    host.set_gains(-60.0, -12.0);
    assert!(host.set_sound_gain("good-id", -8.0).unwrap());
    launch_fake_preview(&host, &preview, Arc::new(vec![0.2; 88_200]));
    // 当前试听入口参数来自卡片，测试传入相同的卡片值。
    assert!(host.set_sound_gain("good-id", -8.0).unwrap());
    drain_primary(&primary);
    let mut pcm = vec![0.0; 6 * 1_000];
    preview.outputs.lock().unwrap()[0](&mut pcm);
    assert!((pcm[pcm.len() - 6] - 0.02).abs() < 1e-6);
    assert!(host.play(0).unwrap());
    for callback in primary.outputs.lock().unwrap().iter_mut() {
        let mut output = vec![0.0; 8_820];
        callback(&mut output);
        assert!(output[output.len() - 1_000..]
            .iter()
            .all(|s| (*s - 0.02).abs() < 1e-6));
    }
    preview.outputs.lock().unwrap()[0](&mut pcm);
    assert!(pcm.iter().all(|sample| *sample == 0.0));
}

#[test]
fn 解码期间调整音效总音量作用于试听首次出声且不被麦克风增益替代() {
    let host = Arc::new(preview_test_host());
    let backend = FakeBackend::default();
    let (prepared_tx, prepared_rx) = mpsc::channel();
    let (continue_tx, continue_rx) = mpsc::channel();
    let (opened_tx, opened_rx) = mpsc::channel();
    *backend.opened.lock().unwrap() = Some(opened_tx);
    let preparing = host.clone();
    let playing = backend.clone();
    let worker = std::thread::spawn(move || {
        preparing.launch_preview(
            "decode-total-gain".into(),
            -8.0,
            || {
                prepared_tx.send(()).unwrap();
                continue_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                Ok(Arc::new(vec![0.2; 88_200]))
            },
            move |samples, token, generation, audio| {
                play_samples(
                    playing,
                    samples,
                    DeviceId::new("fake preview"),
                    StreamFormat::stereo(44_100),
                    audio,
                    token,
                    generation,
                )
            },
        )
    });
    prepared_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    host.set_gains(24.0, -12.0);
    continue_tx.send(()).unwrap();
    worker.join().unwrap().unwrap();
    opened_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let mut pcm = vec![0.0; 2 * 500];
    backend.outputs.lock().unwrap()[0](&mut pcm);
    assert!((pcm[pcm.len() - 2] - 0.02).abs() < 1e-6);
    host.stop_all().unwrap();
}

#[test]
fn 提高试听总音量使用与正式播放相同的限幅并排空短音频尾部() {
    let host = preview_test_host();
    host.set_gains(0.0, 20.0);
    let backend = FakeBackend::default();
    let (closed_tx, closed_rx) = mpsc::channel();
    *backend.closed.lock().unwrap() = Some(closed_tx);
    let samples = Arc::new(vec![0.2; 40]);
    let weak = Arc::downgrade(&samples);
    launch_fake_preview(&host, &backend, samples);
    let (observed_tx, observed_rx) = mpsc::channel();
    {
        let control = host.preview_control.lock().unwrap();
        *control
            .as_ref()
            .unwrap()
            .audio
            .owner_observed_source_end
            .lock()
            .unwrap() = Some(observed_tx);
    }
    let mut initial = vec![0.0; 6 * 20];
    backend.outputs.lock().unwrap()[0](&mut initial);
    assert!(initial.iter().all(|sample| *sample == 0.0));
    // 收到信号说明所有者已经复核完毕，仍因限幅尾部未排空而保留流。
    observed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(
        closed_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert!(weak.upgrade().is_some());
    let mut pcm = vec![0.0; 6 * 300];
    backend.outputs.lock().unwrap()[0](&mut pcm);
    let nonzero = pcm
        .as_chunks::<6>()
        .0
        .iter()
        .filter(|frame| frame[0] != 0.0)
        .collect::<Vec<_>>();
    assert_eq!(nonzero.len(), 20, "短于前视窗口的音频也必须完整出声");
    let ceiling = 10.0f32.powf(-1.0 / 20.0);
    assert!(nonzero
        .iter()
        .all(|frame| (frame[0] - ceiling).abs() < 1e-5));
    assert!(pcm.iter().all(|sample| sample.abs() <= ceiling));
    closed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    // join只等待已经自动关闭的所有者完成，不发出停止或取消请求。
    host.preview_thread
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .join()
        .unwrap();
    assert!(weak.upgrade().is_none(), "自动结束必须回收PCM");
    backend.outputs.lock().unwrap()[0](&mut pcm);
    assert!(
        pcm.iter().all(|sample| *sample == 0.0),
        "保留的回调不再拥有PCM或尾部"
    );
}

#[test]
fn 启动命令排队及开流期间的总音量调整都优先于旧配置() {
    let (host, backend) = fixture();
    let ticket = host.reserve_audio_request();
    host.set_gains(-20.0, -12.0);
    let mut old_config = config();
    assert!(host
        .prepare_start_gains(&mut old_config, Some(ticket))
        .unwrap());
    assert_eq!(old_config.mic_gain_db, -20.0);
    assert_eq!(old_config.sfx_gain_db, -12.0);
    let running = host.running.lock().unwrap().take().unwrap();
    host.set_gains(-30.0, -20.0);
    assert!(host.publish_running(running, Some(ticket)).is_none());
    {
        let guard = host.running.lock().unwrap();
        let running = guard.as_ref().unwrap();
        assert_eq!(running.cfg.mic_gain_db, -30.0);
        assert_eq!(running.cfg.sfx_gain_db, -20.0);
        assert_eq!(running.cfg_key, cfg_fingerprint(&running.cfg));
    }
    assert!(host.play(0).unwrap());
    for callback in backend.outputs.lock().unwrap().iter_mut() {
        let mut pcm = vec![0.0; 8_820];
        callback(&mut pcm);
        assert!(pcm[pcm.len() - 1_000..]
            .iter()
            .all(|s| (*s - 0.02).abs() < 1e-6));
    }
}

#[test]
fn 过期启动和无效总音量不会覆盖当前试听音量() {
    let host = preview_test_host();
    let old_ticket = host.reserve_audio_request();
    host.set_gains(0.0, -20.0);
    let backend = FakeBackend::default();
    launch_fake_preview(&host, &backend, Arc::new(vec![0.2; 88_200]));
    assert!(!host
        .prepare_start_gains(&mut config(), Some(old_ticket))
        .unwrap());
    for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::MAX] {
        host.set_gains(0.0, invalid);
        host.set_gains(invalid, 0.0);
    }
    let mut pcm = vec![0.0; 6 * 500];
    backend.outputs.lock().unwrap()[0](&mut pcm);
    assert!((pcm[pcm.len() - 6] - 0.02).abs() < 1e-6);
    host.stop_all().unwrap();
}

#[test]
fn 音效总音量过载时试听正式播放和监听共享同一限幅上限() {
    let (host, main) = fixture();
    host.set_gains(-60.0, 20.0);
    let preview = FakeBackend::default();
    launch_fake_preview(&host, &preview, Arc::new(vec![0.2; 88_200]));
    drain_primary(&main);
    let mut preview_pcm = vec![0.0; 6 * 1_000];
    preview.outputs.lock().unwrap()[0](&mut preview_pcm);
    let preview_level = preview_pcm[preview_pcm.len() - 6];
    let ceiling = 10.0f32.powf(-1.0 / 20.0);
    assert!((preview_level - ceiling).abs() < 1e-5);
    assert!(host.play(0).unwrap());
    for callback in main.outputs.lock().unwrap().iter_mut() {
        let mut pcm = vec![0.0; 8_820];
        callback(&mut pcm);
        assert!(pcm[pcm.len() - 1_000..]
            .iter()
            .all(|s| (*s - preview_level).abs() < 1e-5));
        assert!(pcm.iter().all(|sample| sample.abs() <= ceiling));
    }
}

#[test]
fn 试听先立体声限幅再按正式输出规则映射单声道和多声道() {
    let ceiling = 10.0f32.powf(-1.0 / 20.0);
    for channels in [1, 2, 6] {
        let host = preview_test_host();
        host.set_gains(0.0, 20.0);
        let backend = FakeBackend::default();
        let (opened_tx, opened_rx) = mpsc::channel();
        *backend.opened.lock().unwrap() = Some(opened_tx);
        let playing = backend.clone();
        let samples = Arc::new((0..44_100).flat_map(|_| [0.6, 0.2]).collect());
        host.launch_preview(
            "channel-layout".into(),
            0.0,
            || Ok(samples),
            move |samples, token, generation, audio| {
                play_samples(
                    playing,
                    samples,
                    DeviceId::new("fake preview"),
                    StreamFormat::new(44_100, channels),
                    audio,
                    token,
                    generation,
                )
            },
        )
        .unwrap();
        opened_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut pcm = vec![0.0; channels as usize * 500];
        backend.outputs.lock().unwrap()[0](&mut pcm);
        let frame = &pcm[pcm.len() - channels as usize..];
        let mono = ceiling * (1.0 + 1.0 / 3.0) * 0.5;
        if channels == 1 {
            assert!((frame[0] - mono).abs() < 1e-5);
        } else {
            assert!((frame[0] - ceiling).abs() < 1e-5);
            assert!((frame[1] - ceiling / 3.0).abs() < 1e-5);
            assert!(frame[2..]
                .iter()
                .all(|sample| (*sample - mono).abs() < 1e-5));
        }
        host.stop_all().unwrap();
    }
}

#[test]
fn 超范围的有限音量被所有入口拒绝且不会污染试听或正式输出() {
    let (host, primary) = fixture();
    let preview = FakeBackend::default();
    host.set_gains(-60.0, 0.0);
    launch_fake_preview(&host, &preview, Arc::new(vec![0.2; 88_200]));
    assert!(host.set_sound_gain("good-id", 12.0).unwrap());
    drain_primary(&primary);
    for invalid in [36.01, 760.0, f32::MAX] {
        host.set_gains(0.0, invalid);
        host.set_gains(invalid, 0.0);
        assert!(host.set_sound_gain("good-id", invalid).is_err());
        assert!(host.set_slot_gain(0, invalid).is_err());
        assert!(host
            .launch_preview("good-id".into(), invalid, || Ok(()), |_, _, _, _| Ok(()))
            .is_err());
        let ticket = host.reserve_audio_request();
        let mut bad_config = config();
        bad_config.sfx_gain_db = invalid;
        assert!(host
            .start_requested(bad_config.clone(), None, ticket)
            .is_err());
        bad_config.sfx_gain_db = 0.0;
        bad_config.mic_gain_db = invalid;
        assert!(host
            .start_requested(bad_config.clone(), None, ticket)
            .is_err());
        bad_config.mic_gain_db = 0.0;
        bad_config.gains[1] = invalid;
        assert!(host
            .start_requested(bad_config.clone(), None, ticket)
            .is_err());
    }
    let expected = 0.2 * 10.0f32.powf(12.0 / 20.0);
    let mut pcm = vec![0.0; 6 * 1_000];
    preview.outputs.lock().unwrap()[0](&mut pcm);
    assert!(pcm.iter().all(|sample| sample.is_finite()));
    assert!((pcm[pcm.len() - 6] - expected).abs() < 1e-6);
    assert!(host.play(0).unwrap());
    for callback in primary.outputs.lock().unwrap().iter_mut() {
        let mut pcm = vec![0.0; 8_820];
        callback(&mut pcm);
        assert!(pcm.iter().all(|sample| sample.is_finite()));
        assert!(pcm[pcm.len() - 1_000..]
            .iter()
            .all(|sample| (*sample - expected).abs() < 1e-6));
    }
    host.set_gains(-60.0, 36.0);
    assert!(host.set_sound_gain("good-id", 36.0).unwrap());
    assert!(host.play(0).unwrap());
    for callback in primary.outputs.lock().unwrap().iter_mut() {
        let mut pcm = vec![0.0; 8_820];
        callback(&mut pcm);
        assert!(pcm
            .iter()
            .all(|sample| sample.is_finite() && sample.abs() <= 10.0f32.powf(-1.0 / 20.0)));
    }
}

#[cfg(feature = "voice")]
#[test]
fn 无音频会话时变声热键不启动音频() {
    let dir = TemporaryDir::new();
    let path = dir.0.join("config.toml");
    let mut config = app_config::Config::default();
    config.voice.active_preset = "builtin:female".into();
    let mut host = preview_test_host();
    host.voice = VoiceService::new_with_path(&config, path.clone());

    let handler = host.voice_hotkey_handler(0);
    handler(1); // 下一个预设

    assert!(
        host.running.lock().unwrap().is_none(),
        "热键不能建立音频会话"
    );
    assert!(!host.status().running);
    assert_eq!(host.voice.status().preset_id, "builtin:male");
    host.voice.flush_saves();
    assert_eq!(
        app_config::Config::load_from(&path).voice.active_preset,
        "builtin:male"
    );
}

#[cfg(feature = "voice")]
#[test]
fn 旧会话热键事件不影响新会话() {
    let (host, _backend, _dir) = voice_fixture(true);
    let before = host.voice.status();
    // 新会话已发布代次 2；旧会话捕获的事件仍带代次 1。
    host.audio_requests.register_session(2);
    let handler = host.voice_hotkey_handler(1);

    handler(1);

    let after = host.voice.status();
    assert_eq!(after.preset_id, before.preset_id);
    assert_eq!(after.generation, before.generation);
}

#[cfg(feature = "voice")]
#[test]
fn 变声热键命令校验跨组冲突并持久化() {
    let dir = TemporaryDir::new();
    let path = dir.0.join("config.toml");
    let config = app_config::Config {
        sound_files: vec![PathBuf::from("/sounds/a.wav")],
        hotkeys: vec!["F9".into()],
        ..app_config::Config::default()
    };
    config.save_to(&path).unwrap();
    let mut host = preview_test_host();
    host.voice = VoiceService::new_with_path(&config, path.clone());

    let error = host.voice_set_hotkeys("F9", "F10", "F8").unwrap_err();
    assert!(
        error.to_string().contains("F9"),
        "冲突错误必须点名重复键：{error}"
    );
    let unchanged = app_config::Config::load_from(&path);
    assert!(unchanged.voice.bypass_hotkey.is_empty(), "冲突不能写入配置");
    assert!(unchanged.voice.next_preset_hotkey.is_empty());

    host.voice_set_hotkeys("F5", "F6", "F7").unwrap();
    let saved = app_config::Config::load_from(&path);
    assert_eq!(saved.voice.bypass_hotkey, "F5");
    assert_eq!(saved.voice.next_preset_hotkey, "F6");
    assert_eq!(saved.voice.prev_preset_hotkey, "F7");
    assert_eq!(saved.hotkeys, vec!["F9".to_owned()], "音效热键保持不变");
}
