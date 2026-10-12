//! Only local loopback sockets are used. No LAN or external network is contacted.

use super::*;
use audio_core::device::StreamFormat;
use audio_core::engine::{EngineParts, SfxSlot};
use std::io::Read;
use std::sync::atomic::AtomicUsize;
use std::sync::{mpsc, TryLockError};

fn engine() -> EngineParts {
    let format = StreamFormat::stereo(48_000);
    EngineParts::build(
        format,
        format,
        vec![SfxSlot {
            name: "tone".into(),
            samples: Arc::new(vec![0.1; 2_000]),
            looping: false,
            gain_db: 0.0,
        }],
        20,
    )
}

fn start(shared: Arc<Shared>) -> Server {
    Server::start(
        shared,
        RemoteConfig {
            port: 0,
            bind: "127.0.0.1".into(),
        },
    )
    .unwrap()
}

fn get(port: u16, path: &str) -> String {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        socket,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn dropping_server_releases_listener_and_same_port_can_be_rebound() {
    let shared = Arc::new(Shared::default());
    let server = start(shared.clone());
    let port = server.port;
    assert_ne!(port, 0);
    assert_eq!(server.urls, vec![format!("http://127.0.0.1:{port}")]);
    assert!(get(port, "/").contains("音频未启动"));
    drop(server);
    let rebound = Server::start(
        shared,
        RemoteConfig {
            port,
            bind: "127.0.0.1".into(),
        },
    )
    .unwrap();
    assert_eq!(rebound.port, port);
    drop(rebound);
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    assert_eq!(listener.local_addr().unwrap().port(), port);
}

#[test]
fn http_uses_unified_handlers_and_clear_then_set_restores_a_new_session() {
    let parts = engine();
    let replacement = engine();
    let shared = Arc::new(Shared::default());
    let old_plays = Arc::new(AtomicUsize::new(0));
    let old_stops = Arc::new(AtomicUsize::new(0));
    let play_count = old_plays.clone();
    let play: PlayHandler = Arc::new(move |slot| {
        if slot != 0 {
            return false;
        }
        play_count.fetch_add(1, Ordering::Relaxed);
        true
    });
    let stop_count = old_stops.clone();
    let stop: StopHandler = Arc::new(move || {
        stop_count.fetch_add(1, Ordering::Relaxed);
    });
    shared.set_routed(parts.control, vec!["original".into()], play, stop);
    let server = start(shared.clone());
    assert!(get(server.port, "/play/1").contains("\"ok\":true"));
    assert!(get(server.port, "/stop").contains("\"action\":\"stop_all\""));
    assert_eq!(old_plays.load(Ordering::Relaxed), 1);
    assert_eq!(old_stops.load(Ordering::Relaxed), 1);

    shared.clear();
    assert!(get(server.port, "/play/1").contains("音频未启动"));
    assert!(get(server.port, "/stop").contains("音频未启动"));
    assert!(!shared.play(0));
    shared.stop_all();
    assert_eq!(old_plays.load(Ordering::Relaxed), 1);
    assert_eq!(old_stops.load(Ordering::Relaxed), 1);

    let new_plays = Arc::new(AtomicUsize::new(0));
    let new_stops = Arc::new(AtomicUsize::new(0));
    let play_count = new_plays.clone();
    let play: PlayHandler = Arc::new(move |_| {
        play_count.fetch_add(1, Ordering::Relaxed);
        true
    });
    let stop_count = new_stops.clone();
    let stop: StopHandler = Arc::new(move || {
        stop_count.fetch_add(1, Ordering::Relaxed);
    });
    shared.set_routed(replacement.control, vec!["replacement".into()], play, stop);
    assert!(get(server.port, "/").contains("replacement"));
    assert!(get(server.port, "/play/1").contains("\"ok\":true"));
    assert!(get(server.port, "/play/2").contains("\"ok\":false"));
    assert!(get(server.port, "/stop").contains("\"action\":\"stop_all\""));
    assert_eq!(new_plays.load(Ordering::Relaxed), 1);
    assert_eq!(new_stops.load(Ordering::Relaxed), 1);
    assert_eq!(old_plays.load(Ordering::Relaxed), 1);
    assert_eq!(old_stops.load(Ordering::Relaxed), 1);
}

#[test]
fn phone_http_page_and_routed_switch_stop_replay_use_actual_single_sound_pcm() {
    let format = StreamFormat::stereo(48_000);
    let mut parts = EngineParts::build(
        format,
        format,
        [([0.2, 0.0], "A"), ([0.0, 0.3], "B")]
            .into_iter()
            .map(|(frame, name)| SfxSlot {
                name: name.into(),
                samples: Arc::new(frame.repeat(8_000)),
                looping: false,
                gain_db: 0.0,
            })
            .collect(),
        20,
    );
    let shared = Arc::new(Shared::default());
    let play = parts.control.clone();
    let stop = parts.control.clone();
    shared.set_routed(
        parts.control.clone(),
        vec!["A".into(), "B".into()],
        Arc::new(move |slot| play.play(slot)),
        Arc::new(move || {
            stop.stop_all();
        }),
    );
    let server = start(shared);
    let page = get(server.port, "/");
    assert!(page.contains(include_str!("control-page.js")));
    let mut rendered = vec![0.0; 2_048];
    for looping in [false, true] {
        parts.control.set_looping(looping);
        assert!(get(server.port, "/play/1").contains("\"ok\":true"));
        parts.render.process(&mut rendered);
        assert!(rendered
            .as_chunks::<2>()
            .0
            .iter()
            .any(|frame| frame[0] > 0.01));
        assert!(get(server.port, "/play/2").contains("\"ok\":true"));
        parts.render.process(&mut rendered);
        assert!(
            rendered
                .as_chunks::<2>()
                .0
                .iter()
                .all(|frame| { frame[0].abs() < 1e-6 || frame[1].abs() < 1e-6 }),
            "phone switch must never mix A and B in one frame"
        );
        assert_eq!(parts.control.stats().active_slot, 1);
        assert!(get(server.port, "/play/999").contains("\"ok\":false"));
        parts.render.process(&mut rendered);
        assert!(rendered
            .as_chunks::<2>()
            .0
            .iter()
            .any(|frame| frame[1] > 0.01));
        assert!(get(server.port, "/stop").contains("\"ok\":true"));
        parts.render.process(&mut rendered);
        parts.render.process(&mut rendered);
        assert!(rendered.iter().all(|sample| sample.abs() < 1e-6));
        assert!(!parts.control.is_playing());
    }
    // A new play after stop remains valid, and the common loop preference is
    // applied to HTTP triggers just as it is to GUI and hotkey triggers.
    parts.control.set_looping(true);
    assert!(get(server.port, "/play/2").contains("\"ok\":true"));
    for _ in 0..20 {
        parts.render.process(&mut rendered);
    }
    assert!(parts.control.is_playing());
    assert!(rendered
        .as_chunks::<2>()
        .0
        .iter()
        .any(|frame| frame[1] > 0.01));
    parts.control.set_looping(false);
    for _ in 0..10 {
        parts.render.process(&mut rendered);
    }
    assert!(!parts.control.is_playing());
    assert!(rendered.iter().all(|sample| sample.abs() < 1e-6));
}

#[test]
fn clear_removes_legacy_hook_and_handler_references() {
    let parts = engine();
    let shared = Shared::default();
    shared.set(parts.control, vec!["one".into()]);
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let calls = hook_calls.clone();
    let hook: PlayHook = Arc::new(move |_| {
        calls.fetch_add(1, Ordering::Relaxed);
    });
    let weak_hook = Arc::downgrade(&hook);
    shared.set_hook(hook);
    assert!(shared.play(0));
    assert_eq!(hook_calls.load(Ordering::Relaxed), 1);
    let play: PlayHandler = Arc::new(|_| true);
    let weak_play = Arc::downgrade(&play);
    let stop: StopHandler = Arc::new(|| {});
    let weak_stop = Arc::downgrade(&stop);
    shared.set_play_handler(play);
    shared.set_stop_handler(stop);
    shared.clear();
    assert!(weak_hook.upgrade().is_none());
    assert!(weak_play.upgrade().is_none());
    assert!(weak_stop.upgrade().is_none());
    assert!(!shared.play(0));
    shared.stop_all();
    assert_eq!(hook_calls.load(Ordering::Relaxed), 1);
}

#[test]
fn request_started_before_standby_cannot_bypass_the_handler_gate() {
    let parts = engine();
    let shared = Arc::new(Shared::default());
    shared.set(parts.control, vec!["one".into()]);
    let active = Arc::new(AtomicBool::new(true));
    let played = Arc::new(AtomicUsize::new(0));
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let resume_rx = Mutex::new(resume_rx);
    let gate = active.clone();
    let play_count = played.clone();
    shared.set_play_handler(Arc::new(move |_| {
        // Prove the request already holds the old handler before clear removes it.
        entered_tx.send(()).unwrap();
        resume_rx.lock().unwrap().recv().unwrap();
        if !gate.load(Ordering::Acquire) {
            return false;
        }
        play_count.fetch_add(1, Ordering::Relaxed);
        true
    }));
    let server = start(shared.clone());
    let port = server.port;
    let request = std::thread::spawn(move || get(port, "/play/1"));
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    active.store(false, Ordering::Release);
    shared.clear();
    resume_tx.send(()).unwrap();
    let response = request.join().unwrap();
    assert!(response.contains("\"ok\":false"));
    assert_eq!(played.load(Ordering::Relaxed), 0);
}

#[test]
fn routed_rejection_and_stop_never_fall_back_to_the_engine_or_legacy_hook() {
    let mut parts = engine();
    let shared = Shared::default();
    let plays = Arc::new(AtomicUsize::new(0));
    let stops = Arc::new(AtomicUsize::new(0));
    let hooks = Arc::new(AtomicUsize::new(0));
    let calls = hooks.clone();
    shared.set_hook(Arc::new(move |_| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    let calls = plays.clone();
    let play: PlayHandler = Arc::new(move |_| {
        calls.fetch_add(1, Ordering::Relaxed);
        false
    });
    let calls = stops.clone();
    let stop: StopHandler = Arc::new(move || {
        calls.fetch_add(1, Ordering::Relaxed);
    });
    shared.set_routed(parts.control.clone(), vec!["routed".into()], play, stop);
    assert!(!shared.play(0));
    let mut out = vec![0.0; 128];
    parts.render.process(&mut out);
    assert!(
        !parts.control.is_playing(),
        "a rejecting host route must not enqueue Play"
    );
    assert_eq!(hooks.load(Ordering::Relaxed), 0);
    assert_eq!(plays.load(Ordering::Relaxed), 1);

    // A stop route which intentionally does not stop this engine proves there
    // was no extra direct stop beside the host callback.
    assert!(parts.control.play(0));
    parts.render.process(&mut out);
    shared.stop_all();
    for _ in 0..8 {
        parts.render.process(&mut out);
    }
    assert!(
        parts.control.is_playing(),
        "routed stop must not also stop the fallback control"
    );
    assert_eq!(stops.load(Ordering::Relaxed), 1);
    shared.clear();
    assert!(!shared.play(0));
    shared.stop_all();
    assert_eq!(plays.load(Ordering::Relaxed), 1);
    assert_eq!(stops.load(Ordering::Relaxed), 1);
    assert!(shared.slots.lock().unwrap().is_empty());
}

/// Pause a legacy request at its fallback snapshot while publishing a routed
/// session. The request must see the old empty state, never the new raw control.
fn fallback_snapshot_precedes_routed_publication(stopping: bool) {
    let shared = Arc::new(Shared::default());
    let mut parts = engine();
    if stopping {
        assert!(parts.control.play(0));
        parts.render.process(&mut vec![0.0; 128]);
        assert!(parts.control.is_playing());
    }
    let control = parts.control.clone();
    let plays = Arc::new(AtomicUsize::new(0));
    let stops = Arc::new(AtomicUsize::new(0));
    let held_control = shared.control.lock().unwrap();
    let reader_shared = shared.clone();
    let (reader_started_tx, reader_started_rx) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        reader_started_tx.send(()).unwrap();
        if stopping {
            reader_shared.stop_all();
            None
        } else {
            Some(reader_shared.play(0))
        }
    });
    reader_started_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    // Observe the paused snapshot without timing sleeps. In the buggy version
    // this guard was already released before waiting on `control`.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let blocked = if stopping {
            matches!(
                shared.stop_handler.try_lock(),
                Err(TryLockError::WouldBlock)
            )
        } else {
            matches!(
                shared.play_handler.try_lock(),
                Err(TryLockError::WouldBlock)
            )
        };
        if blocked {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fallback snapshot lost its handler guard"
        );
        std::thread::yield_now();
    }

    let publisher_shared = shared.clone();
    let play_calls = plays.clone();
    let stop_calls = stops.clone();
    let (published_tx, published_rx) = mpsc::sync_channel(1);
    let publisher = std::thread::spawn(move || {
        publisher_shared.set_routed(
            control,
            vec!["new session".into()],
            Arc::new(move |_| {
                play_calls.fetch_add(1, Ordering::Relaxed);
                false
            }),
            Arc::new(move || {
                stop_calls.fetch_add(1, Ordering::Relaxed);
            }),
        );
        published_tx.send(()).unwrap();
    });
    assert!(matches!(
        published_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    drop(held_control);
    assert_eq!(
        reader.join().unwrap(),
        if stopping { None } else { Some(false) }
    );
    published_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    publisher.join().unwrap();
    assert!(!shared.play(0));
    shared.stop_all();
    assert_eq!(plays.load(Ordering::Relaxed), 1);
    assert_eq!(stops.load(Ordering::Relaxed), 1);
    for _ in 0..8 {
        parts.render.process(&mut vec![0.0; 128]);
    }
    assert_eq!(
        parts.control.is_playing(),
        stopping,
        "publication must not leak direct fallback playback commands"
    );
}

#[test]
fn play_fallback_snapshot_cannot_read_a_new_routed_control() {
    fallback_snapshot_precedes_routed_publication(false);
}

#[test]
fn stop_fallback_snapshot_cannot_read_a_new_routed_control() {
    fallback_snapshot_precedes_routed_publication(true);
}

#[test]
fn routed_callbacks_and_legacy_hook_can_clear_shared_without_holding_state_locks() {
    let parts = engine();
    let shared = Arc::new(Shared::default());
    let play_shared = Arc::downgrade(&shared);
    let stop_shared = Arc::downgrade(&shared);
    let play: PlayHandler = Arc::new(move |_| {
        play_shared.upgrade().unwrap().clear();
        false
    });
    let stop: StopHandler = Arc::new(move || {
        stop_shared.upgrade().unwrap().clear();
    });
    shared.set_routed(
        parts.control.clone(),
        vec!["one".into()],
        play.clone(),
        stop.clone(),
    );
    assert!(!shared.play(0));
    assert!(shared.control.lock().unwrap().is_none());
    shared.set_routed(parts.control.clone(), vec!["one".into()], play, stop);
    shared.stop_all();
    assert!(shared.control.lock().unwrap().is_none());

    shared.set(parts.control, vec!["legacy".into()]);
    let hook_shared = Arc::downgrade(&shared);
    shared.set_hook(Arc::new(move |_| hook_shared.upgrade().unwrap().clear()));
    assert!(shared.play(0));
    assert!(shared.control.lock().unwrap().is_none());
    assert!(!shared.play(0));
}
