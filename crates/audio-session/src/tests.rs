//! Tests use a !Send native stream and explicitly driven fake callbacks.
//! No microphone, speaker, system configuration or native audio driver is opened.

use super::*;
use audio_core::device::{AudioStream, CaptureCallback, DeviceInfo, RenderCallback};
use std::collections::HashMap;
use std::rc::Rc;
use std::thread::ThreadId;

#[derive(Clone, Debug)]
struct Event {
    operation: &'static str,
    route: String,
    stream: usize,
    thread: ThreadId,
}

type SharedRender = Arc<Mutex<RenderCallback>>;
type SharedCapture = Arc<Mutex<CaptureCallback>>;

#[derive(Default)]
struct FakeState {
    next_stream: usize,
    events: Vec<Event>,
    outputs: HashMap<String, SharedRender>,
    inputs: HashMap<String, SharedCapture>,
}

#[derive(Clone)]
struct FakeBackend {
    state: Arc<Mutex<FakeState>>,
    role: &'static str,
    fail_input: bool,
    panic_input: bool,
    changed_input: Option<StreamFormat>,
    changed_output: Option<StreamFormat>,
    retain_callbacks: bool,
    input_gate: Option<Arc<(Mutex<bool>, std::sync::Condvar)>>,
}

impl FakeBackend {
    fn primary(state: &Arc<Mutex<FakeState>>) -> Self {
        Self {
            state: state.clone(),
            role: "primary",
            fail_input: false,
            panic_input: false,
            changed_input: None,
            changed_output: None,
            retain_callbacks: false,
            input_gate: None,
        }
    }

    fn monitor(state: &Arc<Mutex<FakeState>>) -> Self {
        Self {
            role: "monitor",
            ..Self::primary(state)
        }
    }

    fn stream(&self, route: String, format: StreamFormat) -> Box<dyn AudioStream> {
        let mut state = self.state.lock().unwrap();
        let stream = state.next_stream;
        state.next_stream += 1;
        state.events.push(Event {
            operation: "open",
            route: route.clone(),
            stream,
            thread: std::thread::current().id(),
        });
        Box::new(FakeStream {
            state: self.state.clone(),
            route,
            format,
            stream,
            retain_callbacks: self.retain_callbacks,
            _not_send: Rc::new(()),
        })
    }
}

struct FakeStream {
    state: Arc<Mutex<FakeState>>,
    route: String,
    format: StreamFormat,
    stream: usize,
    retain_callbacks: bool,
    _not_send: Rc<()>,
}

impl AudioStream for FakeStream {
    fn format(&self) -> StreamFormat {
        self.format
    }
    fn device_name(&self) -> &str {
        &self.route
    }
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.events.push(Event {
            operation: "drop",
            route: self.route.clone(),
            stream: self.stream,
            thread: std::thread::current().id(),
        });
        if !self.retain_callbacks {
            state.outputs.remove(&self.route);
            state.inputs.remove(&self.route);
        }
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
        Some(DeviceId::new("input"))
    }
    fn default_output(&self) -> Option<DeviceId> {
        Some(DeviceId::new("output"))
    }
    fn input_format(&self, _: &DeviceId, prefer: Option<StreamFormat>) -> Result<StreamFormat> {
        Ok(prefer.unwrap_or(StreamFormat::new(48_000, 1)))
    }
    fn output_format(&self, _: &DeviceId, prefer: Option<StreamFormat>) -> Result<StreamFormat> {
        Ok(prefer.unwrap_or(StreamFormat::stereo(48_000)))
    }
    fn open_input(
        &self,
        _: &DeviceId,
        prefer: Option<StreamFormat>,
        cb: CaptureCallback,
    ) -> Result<Box<dyn AudioStream>> {
        if self.fail_input {
            anyhow::bail!("injected input open failure");
        }
        let route = format!("{}-input", self.role);
        self.state
            .lock()
            .unwrap()
            .inputs
            .insert(route.clone(), Arc::new(Mutex::new(cb)));
        let stream = self.stream(route, self.changed_input.unwrap_or(prefer.unwrap()));
        assert!(!self.panic_input, "injected input open panic");
        if let Some(gate) = &self.input_gate {
            let (lock, changed) = &**gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = changed.wait(released).unwrap();
            }
        }
        Ok(stream)
    }
    fn open_output(
        &self,
        _: &DeviceId,
        prefer: Option<StreamFormat>,
        cb: RenderCallback,
    ) -> Result<Box<dyn AudioStream>> {
        let route = format!("{}-output", self.role);
        self.state
            .lock()
            .unwrap()
            .outputs
            .insert(route.clone(), Arc::new(Mutex::new(cb)));
        Ok(self.stream(route, self.changed_output.unwrap_or(prefer.unwrap())))
    }
}

fn slot(rate: u32, looping: bool) -> SfxSlot {
    let samples = (0..rate)
        .flat_map(|frame| {
            let sample = 0.2 * (frame as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin();
            [sample, sample]
        })
        .collect();
    SfxSlot {
        name: "tone".into(),
        samples: Arc::new(samples),
        looping,
        gain_db: 0.0,
    }
}

fn options(monitor: bool) -> SessionOptions {
    let library = Library {
        slots: vec![slot(48_000, true)],
        ..Library::default()
    };
    SessionOptions {
        input: DeviceId::new("input"),
        output: DeviceId::new("output"),
        input_format: StreamFormat::new(48_000, 1),
        output_format: StreamFormat::stereo(48_000),
        buffer_ms: 20,
        slots: library.slots.clone(),
        mic_gain_db: -60.0,
        sfx_gain_db: 0.0,
        monitor: monitor.then(|| MonitorOptions {
            device: DeviceId::new("monitor"),
            format: StreamFormat::new(44_100, 6),
            slots: library.monitor_slots(48_000, 44_100),
        }),
    }
}

fn render(state: &Arc<Mutex<FakeState>>, route: &str, frames: usize, channels: usize) -> Vec<f32> {
    let callback = state.lock().unwrap().outputs.get(route).unwrap().clone();
    let mut output = vec![f32::NAN; frames * channels];
    callback.lock().unwrap()(&mut output);
    assert!(
        output.iter().all(|sample| sample.is_finite()),
        "every output sample must be initialized"
    );
    output
}

fn assert_streams_closed_on_owner(state: &Arc<Mutex<FakeState>>) {
    let state = state.lock().unwrap();
    for opened in state
        .events
        .iter()
        .filter(|event| event.operation == "open")
    {
        let closed: Vec<_> = state
            .events
            .iter()
            .filter(|event| event.operation == "drop" && event.stream == opened.stream)
            .collect();
        assert_eq!(
            closed.len(),
            1,
            "stream {} on {} must close exactly once",
            opened.stream,
            opened.route
        );
        assert_eq!(
            opened.thread, closed[0].thread,
            "{} moved its native stream across threads",
            opened.route
        );
        assert_ne!(opened.thread, std::thread::current().id());
    }
}

#[test]
fn session_restart_and_cross_thread_drop_close_every_native_stream_on_its_owner() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    for _ in 0..3 {
        let session = Session::open_with_backend(
            options(true),
            FakeBackend::primary(&state),
            FakeBackend::monitor(&state),
        )
        .unwrap();
        assert!(session.playback.play(0));
        assert!(render(&state, "primary-output", 2048, 2)
            .iter()
            .any(|sample| sample.abs() > 0.05));
        std::thread::spawn(move || drop(session)).join().unwrap();
        assert_streams_closed_on_owner(&state);
        let state = state.lock().unwrap();
        assert!(state.outputs.is_empty());
        assert!(state.inputs.is_empty());
    }
    assert_eq!(
        state
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|event| event.operation == "open")
            .count(),
        9
    );
}

#[test]
fn input_open_failure_closes_output_before_returning_error() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let backend = FakeBackend {
        fail_input: true,
        ..FakeBackend::primary(&state)
    };
    let result = Session::open_with_backend(options(true), backend, FakeBackend::monitor(&state));
    assert!(result.is_err());
    assert_streams_closed_on_owner(&state);
    let state = state.lock().unwrap();
    assert_eq!(
        state
            .events
            .iter()
            .filter(|event| event.operation == "open")
            .count(),
        1
    );
    assert!(state.outputs.is_empty());
    assert!(state.inputs.is_empty());
}

#[test]
fn each_negotiated_format_must_match_and_mismatch_reclaims_prior_streams() {
    for route in ["primary-output", "primary-input", "monitor-output"] {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let mut primary = FakeBackend::primary(&state);
        let mut monitor = FakeBackend::monitor(&state);
        match route {
            "primary-output" => primary.changed_output = Some(StreamFormat::stereo(44_100)),
            "primary-input" => primary.changed_input = Some(StreamFormat::new(44_100, 1)),
            "monitor-output" => monitor.changed_output = Some(StreamFormat::new(48_000, 6)),
            _ => unreachable!(),
        }
        let result = Session::open_with_backend(options(true), primary, monitor);
        assert!(
            result.is_err(),
            "{route} mismatch must not run a misconfigured engine"
        );
        assert_streams_closed_on_owner(&state);
        let state = state.lock().unwrap();
        assert!(state.outputs.is_empty());
        assert!(state.inputs.is_empty());
    }
}

#[test]
fn monitor_uses_one_persistent_stream_and_maps_its_own_format() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let session = Session::open_with_backend(
        options(true),
        FakeBackend::primary(&state),
        FakeBackend::monitor(&state),
    )
    .unwrap();
    for _ in 0..5 {
        assert!(session.playback.play(0));
        let primary = render(&state, "primary-output", 2048, 2);
        let monitor = render(&state, "monitor-output", 2048, 6);
        assert!(primary.iter().any(|sample| sample.abs() > 0.05));
        assert!(monitor.iter().any(|sample| sample.abs() > 0.05));
        assert!(monitor.as_chunks::<6>().0.iter().all(|frame| frame[2..]
            .iter()
            .all(|sample| (*sample - (frame[0] + frame[1]) * 0.5).abs() < 1e-6)));
    }
    assert_eq!(session.playback.monitor_plays.load(Ordering::Relaxed), 5);
    assert_eq!(
        state
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|event| event.operation == "open" && event.route == "monitor-output")
            .count(),
        1
    );
    drop(session);
    assert_streams_closed_on_owner(&state);
}

#[test]
fn stop_and_standby_cancel_primary_and_monitor_pcm_then_reactivation_works() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let session = Session::open_with_backend(
        options(true),
        FakeBackend::primary(&state),
        FakeBackend::monitor(&state),
    )
    .unwrap();
    for standby in [false, true] {
        session.playback.activate();
        assert!(session.playback.play(0));
        render(&state, "primary-output", 2048, 2);
        render(&state, "monitor-output", 2048, 6);
        if standby {
            session.playback.standby();
        } else {
            session.playback.stop_all();
        }
        let primary = render(&state, "primary-output", 2048, 2);
        let monitor = render(&state, "monitor-output", 2048, 6);
        assert!(primary[1024 * 2..].iter().all(|sample| *sample == 0.0));
        assert!(monitor[1024 * 6..].iter().all(|sample| *sample == 0.0));
        if standby {
            assert!(!session.playback.play(0));
            render(&state, "primary-output", 1024, 2);
            render(&state, "monitor-output", 1024, 6);
        }
    }
    session.playback.activate();
    assert!(session.playback.play(0));
    assert!(render(&state, "monitor-output", 2048, 6)
        .iter()
        .any(|sample| sample.abs() > 0.05));
}

#[test]
fn driver_retaining_callbacks_cannot_retain_samples_or_play_after_session_drop() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let opts = options(false);
    let samples = Arc::downgrade(&opts.slots[0].samples);
    // Keep no extra copy of the slot outside the session.
    let backend = FakeBackend {
        retain_callbacks: true,
        ..FakeBackend::primary(&state)
    };
    let session = Session::open_with_backend(opts, backend, FakeBackend::monitor(&state)).unwrap();
    assert!(session.playback.play(0));
    render(&state, "primary-output", 2048, 2);
    drop(session);
    assert!(
        samples.upgrade().is_none(),
        "native callback shells must release decoded samples"
    );
    assert!(render(&state, "primary-output", 2048, 2)
        .iter()
        .all(|sample| *sample == 0.0));
    assert_streams_closed_on_owner(&state);
}

#[test]
fn standby_cancels_already_queued_play_when_command_queue_is_full() {
    let format = StreamFormat::stereo(48_000);
    let mut parts = EngineParts::build(format, format, vec![slot(48_000, true)], 20);
    let mut monitor = EngineParts::build(format, format, vec![slot(48_000, true)], 20);
    let playback = Playback::new(parts.control.clone(), Some(monitor.control.clone()));
    let mut accepted = 0;
    while playback.play(0) {
        accepted += 1;
    }
    assert!(accepted > 0);
    playback.standby();
    let mut primary_pcm = vec![0.0; 4096];
    let mut monitor_pcm = vec![0.0; 4096];
    parts.render.process(&mut primary_pcm);
    monitor.render.process(&mut monitor_pcm);
    assert!(primary_pcm[2048..].iter().all(|sample| *sample == 0.0));
    assert!(monitor_pcm[2048..].iter().all(|sample| *sample == 0.0));
    assert!(!parts.stats.snapshot().playing);
    assert!(!monitor.stats.snapshot().playing);
}

#[test]
fn gain_updates_apply_to_primary_and_monitor_and_invalid_slot_never_triggers_monitor() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let session = Session::open_with_backend(
        options(true),
        FakeBackend::primary(&state),
        FakeBackend::monitor(&state),
    )
    .unwrap();
    assert!(!session.playback.play(1));
    assert_eq!(session.playback.monitor_plays.load(Ordering::Relaxed), 0);
    assert!(session.playback.play(0));
    render(&state, "primary-output", 2048, 2);
    render(&state, "monitor-output", 2048, 6);
    assert!(session.playback.set_slot_gain(0, -100.0));
    assert!(!session.playback.set_slot_gain(1, 0.0));
    let primary = render(&state, "primary-output", 2048, 2);
    let monitor = render(&state, "monitor-output", 2048, 6);
    assert!(primary[1024 * 2..].iter().all(|sample| *sample == 0.0));
    assert!(monitor[1024 * 6..].iter().all(|sample| *sample == 0.0));
    assert!(session.playback.set_slot_gain(0, 0.0));
    assert!(render(&state, "primary-output", 2048, 2)
        .iter()
        .any(|sample| sample.abs() > 0.05));
    assert!(render(&state, "monitor-output", 2048, 6)
        .iter()
        .any(|sample| sample.abs() > 0.05));
    session.playback.set_gains(-100.0, -100.0);
    let primary = render(&state, "primary-output", 2048, 2);
    let monitor = render(&state, "monitor-output", 2048, 6);
    assert!(primary[1024 * 2..].iter().all(|sample| *sample == 0.0));
    assert!(monitor[1024 * 6..].iter().all(|sample| *sample == 0.0));
}

#[test]
fn skipped_file_preserves_good_sound_identity_and_associated_settings() {
    static FIXTURE: AtomicU64 = AtomicU64::new(0);
    struct Folder(PathBuf);
    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let dir = Folder(std::env::temp_dir().join(format!(
        "audio_session_library_{}_{}",
        std::process::id(),
        FIXTURE.fetch_add(1, Ordering::Relaxed)
    )));
    std::fs::create_dir(&dir.0).unwrap();
    let bad = dir.0.join("bad.wav");
    let good = dir.0.join("good, with comma.wav");
    std::fs::write(&bad, b"invalid wave").unwrap();
    // One second, 8 kHz mono PCM16 also exercises the legal 6x decoding ratio.
    let data: Vec<u8> = (0..8_000)
        .flat_map(|frame| {
            let sample =
                (0.2 * 32767.0 * (frame as f32 * 440.0 * std::f32::consts::TAU / 8_000.0).sin())
                    as i16;
            sample.to_le_bytes()
        })
        .collect();
    let mut wave = b"RIFF".to_vec();
    wave.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    wave.extend_from_slice(b"WAVEfmt ");
    wave.extend_from_slice(&16u32.to_le_bytes());
    wave.extend_from_slice(&1u16.to_le_bytes());
    wave.extend_from_slice(&1u16.to_le_bytes());
    wave.extend_from_slice(&8_000u32.to_le_bytes());
    wave.extend_from_slice(&16_000u32.to_le_bytes());
    wave.extend_from_slice(&2u16.to_le_bytes());
    wave.extend_from_slice(&16u16.to_le_bytes());
    wave.extend_from_slice(b"data");
    wave.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wave.extend(data);
    std::fs::write(&good, wave).unwrap();
    let sound = SoundSpec {
        id: "stable-good-id".into(),
        file: good,
        name: "嘿,你好吗".into(),
        gain_db: -9.0,
        hotkey: "F2".into(),
        tags: vec!["B-only".into()],
    };
    let library = Library::load(
        &[
            SoundSpec {
                id: "bad-id".into(),
                file: bad,
                name: "bad".into(),
                gain_db: 12.0,
                hotkey: "F1".into(),
                tags: vec![],
            },
            sound.clone(),
        ],
        48_000,
        false,
        false,
    );
    assert_eq!(library.slots.len(), 1);
    assert_eq!(library.skipped.len(), 1);
    assert_eq!(library.meta[0].source_index, 1);
    assert_eq!(library.meta[0].spec.id, sound.id);
    assert_eq!(library.meta[0].spec.file, sound.file);
    assert_eq!(library.meta[0].spec.name, sound.name);
    assert_eq!(library.meta[0].spec.hotkey, "F2");
    assert_eq!(library.meta[0].spec.tags, vec!["B-only"]);
    assert_eq!(library.slots[0].gain_db, -9.0);
    assert_eq!(library.slots[0].samples.len(), 48_000 * 2);
    assert!((library.meta[0].duration_secs - 1.0).abs() < 1e-4);
    assert_eq!(
        library.monitor_slots(48_000, 44_100)[0].samples.len(),
        44_100 * 2
    );
}

#[derive(Default)]
struct WriteRecord {
    bytes: Vec<u8>,
    threads: Vec<ThreadId>,
}
struct RecordingWriter(Arc<Mutex<WriteRecord>>);
impl std::io::Write for RecordingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut record = self.0.lock().unwrap();
        record.bytes.extend_from_slice(bytes);
        record.threads.push(std::thread::current().id());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0
            .lock()
            .unwrap()
            .threads
            .push(std::thread::current().id());
        Ok(())
    }
}

fn open_dump(state: &Arc<Mutex<FakeState>>, backend: FakeBackend, dump: DumpRequest) -> Session {
    Session::open_backends(
        options(false),
        backend,
        FakeBackend::monitor(state),
        Arc::new(StreamHealth::default()),
        Arc::new(StreamHealth::default()),
        Some(dump),
        Duration::from_secs(2),
    )
    .unwrap()
}

#[test]
fn dump_pcm_matches_actual_output_and_all_writes_stay_on_stream_owner() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let writes = Arc::new(Mutex::new(WriteRecord::default()));
    let session = open_dump(
        &state,
        FakeBackend::primary(&state),
        DumpRequest::Writer {
            writer: Box::new(RecordingWriter(writes.clone())),
            max_frames: 1200,
        },
    );
    assert!(session.playback.play(0));
    let output = render(&state, "primary-output", 2048, 2);
    assert!(output.iter().any(|sample| sample.abs() > 0.05));
    drop(session);
    let expected: Vec<_> = output[..1200 * 2]
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect();
    let record = writes.lock().unwrap();
    assert_eq!(record.bytes, expected);
    let owner = state
        .lock()
        .unwrap()
        .events
        .iter()
        .find(|event| event.operation == "open")
        .unwrap()
        .thread;
    assert!(!record.threads.is_empty());
    assert!(record.threads.iter().all(|thread| *thread == owner));
    assert_ne!(owner, std::thread::current().id());
}

#[test]
fn dump_file_is_flushed_and_retained_native_callback_cannot_keep_engine_or_queue_active() {
    static FILE: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "mixer_dump_{}_{}.f32",
        std::process::id(),
        FILE.fetch_add(1, Ordering::Relaxed)
    ));
    let state = Arc::new(Mutex::new(FakeState::default()));
    let backend = FakeBackend {
        retain_callbacks: true,
        ..FakeBackend::primary(&state)
    };
    let session = open_dump(
        &state,
        backend,
        DumpRequest::File(DumpOptions {
            path: path.clone(),
            max_frames: 512,
        }),
    );
    assert!(session.playback.play(0));
    let output = render(&state, "primary-output", 2048, 2);
    drop(session);
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(bytes.len(), 512 * 2 * 4);
    let expected: Vec<_> = output[..512 * 2]
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect();
    assert_eq!(bytes, expected);
    assert!(render(&state, "primary-output", 2048, 2)
        .iter()
        .all(|sample| *sample == 0.0));
}

#[test]
fn diagnostics_off_has_no_dump_ring_and_full_ring_reports_exact_dropped_sample_count() {
    let dropped = Arc::new(AtomicU64::new(0));
    let (producer, drain) = DumpDrain::prepare(None, 2, dropped.clone()).unwrap();
    assert!(producer.is_none());
    assert!(drain.is_none());
    let writes = Arc::new(Mutex::new(WriteRecord::default()));
    let (producer, mut drain) = DumpDrain::prepare(
        Some(DumpRequest::Writer {
            writer: Box::new(RecordingWriter(writes.clone())),
            max_frames: 40_000,
        }),
        2,
        dropped.clone(),
    )
    .unwrap();
    let format = StreamFormat::stereo(48_000);
    let parts = EngineParts::build(format, format, vec![slot(48_000, true)], 20);
    parts.control.mic_gain_db.set(-100.0);
    assert!(parts.control.play(0));
    let mut render = RenderState {
        processor: parts.render,
        dump: producer,
    };
    let mut output = vec![0.0; 80_000];
    render.process(&mut output);
    assert_eq!(dropped.load(Ordering::Relaxed), 80_000 - 65_536);
    drop(render); // Detaching render state must also release the producer.
    drain.as_mut().unwrap().finish().unwrap();
    assert_eq!(writes.lock().unwrap().bytes.len(), 65_536 * 4);
}

#[test]
fn dump_write_failure_is_reported_and_session_playback_becomes_inactive() {
    struct FailingWriter;
    impl std::io::Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected full disk"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let state = Arc::new(Mutex::new(FakeState::default()));
    let session = open_dump(
        &state,
        FakeBackend::primary(&state),
        DumpRequest::Writer {
            writer: Box::new(FailingWriter),
            max_frames: 1200,
        },
    );
    assert!(session.playback.play(0));
    render(&state, "primary-output", 2048, 2);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let error = loop {
        if let Some(error) = session.health.take_error() {
            break error;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "dump I/O error must reach session health"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(error.contains("injected full disk"));
    assert!(!session.playback.active());
    assert!(!session.playback.play(0));
    drop(session);
    assert_streams_closed_on_owner(&state);
}

#[test]
fn timed_out_open_mutes_callbacks_immediately_and_late_native_return_closes_on_owner() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let backend = FakeBackend {
        input_gate: Some(gate.clone()),
        retain_callbacks: true,
        ..FakeBackend::primary(&state)
    };
    let mut opts = options(true);
    opts.mic_gain_db = 0.0;
    let result = Session::open_backends(
        opts,
        backend,
        FakeBackend::monitor(&state),
        Arc::new(StreamHealth::default()),
        Arc::new(StreamHealth::default()),
        None,
        Duration::from_millis(250),
    );
    assert!(result.is_err());
    let input = state
        .lock()
        .unwrap()
        .inputs
        .get("primary-input")
        .unwrap()
        .clone();
    // The input native call is still pending. Callbacks must already obey cancellation.
    input.lock().unwrap()(&[0.5; 4096]);
    assert!(render(&state, "primary-output", 2048, 2)
        .iter()
        .all(|sample| *sample == 0.0));
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let drops = state
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|event| event.operation == "drop")
            .count();
        if drops == 2 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "late native return must reclaim both streams"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(state
        .lock()
        .unwrap()
        .events
        .iter()
        .all(|event| event.route != "monitor-output"));
    assert_streams_closed_on_owner(&state);
}

#[test]
fn second_stream_open_panic_detaches_all_retained_callback_shells_and_returns_startup_error() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let backend = FakeBackend {
        panic_input: true,
        retain_callbacks: true,
        ..FakeBackend::primary(&state)
    };
    let opts = options(true);
    let primary_samples = Arc::downgrade(&opts.slots[0].samples);
    let monitor_samples = Arc::downgrade(&opts.monitor.as_ref().unwrap().slots[0].samples);
    let health = Arc::new(StreamHealth::default());
    let writes = Arc::new(Mutex::new(WriteRecord::default()));
    let result = Session::open_backends(
        opts,
        backend,
        FakeBackend::monitor(&state),
        health.clone(),
        Arc::new(StreamHealth::default()),
        Some(DumpRequest::Writer {
            writer: Box::new(RecordingWriter(writes)),
            max_frames: 1200,
        }),
        Duration::from_secs(2),
    );
    let error = match result {
        Ok(_) => panic!("backend panic must fail startup"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("injected input open panic"));
    assert!(health.panicked());
    assert!(health
        .take_error()
        .unwrap()
        .contains("injected input open panic"));
    assert!(primary_samples.upgrade().is_none());
    assert!(monitor_samples.upgrade().is_none());
    assert!(render(&state, "primary-output", 2048, 2)
        .iter()
        .all(|sample| *sample == 0.0));
    let input = state
        .lock()
        .unwrap()
        .inputs
        .get("primary-input")
        .unwrap()
        .clone();
    input.lock().unwrap()(&[0.5; 4096]);
    assert_streams_closed_on_owner(&state);
}

#[test]
fn diagnostic_writer_panic_is_reported_and_detaches_retained_render_state() {
    struct PanickingWriter;
    impl std::io::Write for PanickingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            panic!("injected dump writer panic");
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let state = Arc::new(Mutex::new(FakeState::default()));
    let backend = FakeBackend {
        retain_callbacks: true,
        ..FakeBackend::primary(&state)
    };
    let session = open_dump(
        &state,
        backend,
        DumpRequest::Writer {
            writer: Box::new(PanickingWriter),
            max_frames: 1200,
        },
    );
    assert!(session.playback.play(0));
    render(&state, "primary-output", 2048, 2);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let error = loop {
        if let Some(error) = session.health.take_error() {
            break error;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(error.contains("injected dump writer panic"));
    assert!(session.health.panicked());
    assert!(!session.playback.active());
    drop(session);
    assert!(render(&state, "primary-output", 2048, 2)
        .iter()
        .all(|sample| *sample == 0.0));
    assert_streams_closed_on_owner(&state);
}
