//! Shared GUI/CLI library loading, playback routing and stream ownership.
//! Native streams stay on their owner thread. All trigger paths use Playback,
//! whose gate serializes standby transitions with command submission.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use audio_core::decode::decode_file;
use audio_core::device::{AudioBackend, DeviceId, DeviceInfo, Direction, StreamFormat};
use audio_core::engine::{EngineParts, EngineStats, PlaybackControl, RenderProcessor, SfxSlot};
use backend_cpal::{CpalBackend, StreamHealth};

pub fn resolve_device(
    devices: &[DeviceInfo],
    selector: &str,
    direction: Direction,
) -> Result<Option<DeviceId>> {
    if let Some(device) = devices
        .iter()
        .find(|d| d.direction == direction && d.id.as_str() == selector)
    {
        return Ok(Some(device.id.clone()));
    }
    let mut matches = devices
        .iter()
        .filter(|d| d.direction == direction && d.name == selector);
    let first = matches.next();
    anyhow::ensure!(
        matches.next().is_none(),
        "设备名「{selector}」对应多个设备，请用设备 ID 选择"
    );
    Ok(first.map(|d| d.id.clone()))
}

#[derive(Debug, Clone)]
pub struct SoundSpec {
    pub id: String,
    pub file: PathBuf,
    pub name: String,
    pub gain_db: f32,
    pub hotkey: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SoundMeta {
    pub source_index: usize,
    pub spec: SoundSpec,
    pub duration_secs: f32,
    pub peak_db: f32,
}

#[derive(Default)]
pub struct Library {
    pub slots: Vec<SfxSlot>,
    pub meta: Vec<SoundMeta>,
    pub skipped: Vec<(String, String)>,
}

impl Library {
    pub fn load(specs: &[SoundSpec], rate: u32, looping: bool, normalize: bool) -> Self {
        let mut library = Self::default();
        for (source_index, spec) in specs.iter().enumerate() {
            let name = if spec.name.trim().is_empty() {
                spec.file
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            } else {
                spec.name.trim().to_owned()
            };
            match decode_file(&spec.file, rate) {
                Ok(mut audio) if audio.frames() > 0 => {
                    if normalize {
                        audio.normalize_peak(0.708);
                    }
                    let peak_db = 20.0 * audio.peak().max(1e-9).log10();
                    let mut spec = spec.clone();
                    spec.name = name.clone();
                    library.meta.push(SoundMeta {
                        source_index,
                        spec: spec.clone(),
                        duration_secs: audio.duration_secs,
                        peak_db,
                    });
                    library.slots.push(SfxSlot {
                        name,
                        samples: Arc::new(audio.samples),
                        looping,
                        gain_db: spec.gain_db,
                    });
                }
                Ok(_) => library.skipped.push((name, "文件里没有音频数据".into())),
                Err(e) => library
                    .skipped
                    .push((name, e.chain().last().unwrap_or(e.as_ref()).to_string())),
            }
        }
        library
    }

    /// Prepare the monitor at its own sample rate, preserving slot identity.
    pub fn monitor_slots(&self, source_rate: u32, target_rate: u32) -> Vec<SfxSlot> {
        self.slots
            .iter()
            .map(|s| SfxSlot {
                samples: if source_rate == target_rate {
                    s.samples.clone()
                } else {
                    Arc::new(resample_stereo(&s.samples, source_rate, target_rate))
                },
                ..s.clone()
            })
            .collect()
    }
}

/// Non-realtime conversion with exact duration, including the final frame.
pub fn resample_stereo(samples: &[f32], source_rate: u32, target_rate: u32) -> Vec<f32> {
    assert!(source_rate > 0 && target_rate > 0);
    let frames = samples.len() / 2;
    if frames == 0 {
        return Vec::new();
    }
    let output_frames = ((frames as u64 * target_rate as u64 + source_rate as u64 / 2)
        / source_rate as u64) as usize;
    let mut output = Vec::with_capacity(output_frames * 2);
    for i in 0..output_frames {
        let position = i as f64 * source_rate as f64 / target_rate as f64;
        let left = (position as usize).min(frames - 1);
        let right = (left + 1).min(frames - 1);
        let fraction = (position - left as f64) as f32;
        for ch in 0..2 {
            output.push(
                samples[left * 2 + ch]
                    + (samples[right * 2 + ch] - samples[left * 2 + ch]) * fraction,
            );
        }
    }
    output
}

/// A format-aware cursor for preview output; extra speaker channels are silent.
pub struct SampleCursor {
    samples: Arc<Vec<f32>>,
    frame: usize,
    gain: f32,
}
impl SampleCursor {
    pub fn new(samples: Arc<Vec<f32>>, gain_db: f32) -> Self {
        Self {
            samples,
            frame: 0,
            gain: gain(gain_db),
        }
    }
    pub fn render(&mut self, output: &mut [f32], channels: usize) {
        output.fill(0.0);
        for frame in output.chunks_exact_mut(channels.max(1)) {
            if self.frame * 2 + 1 >= self.samples.len() {
                break;
            }
            let l = self.samples[self.frame * 2] * self.gain;
            let r = self.samples[self.frame * 2 + 1] * self.gain;
            if frame.len() == 1 {
                frame[0] = (l + r) * 0.5;
            } else {
                frame[0] = l;
                frame[1] = r;
            }
            self.frame += 1;
        }
    }
}
fn gain(db: f32) -> f32 {
    if db <= -60.0 {
        0.0
    } else {
        10.0f32.powf(db / 20.0)
    }
}

pub struct Playback {
    gate: Mutex<()>,
    active: AtomicBool,
    pub control: Arc<PlaybackControl>,
    monitor: Option<Arc<PlaybackControl>>,
    pub monitor_plays: AtomicU64,
}
impl Playback {
    pub fn new(control: Arc<PlaybackControl>, monitor: Option<Arc<PlaybackControl>>) -> Self {
        Self {
            gate: Mutex::new(()),
            active: AtomicBool::new(true),
            control,
            monitor,
            monitor_plays: AtomicU64::new(0),
        }
    }
    pub fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
    pub fn is_playing(&self) -> bool {
        self.control.is_playing() || self.monitor.as_ref().is_some_and(|m| m.is_playing())
    }
    /// 两个输出回调均完成最近的 stop_all，含音效淡出与限幅器尾音。
    /// 等待该确认不会中断人声透传，也不需要在回调中取控制锁。
    pub fn stop_all_finished(&self) -> bool {
        self.control.stop_all_finished()
            && self.monitor.as_ref().is_none_or(|m| m.stop_all_finished())
    }
    pub fn activate(&self) {
        let _guard = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        self.active.store(true, Ordering::Release);
    }
    pub fn standby(&self) {
        let _guard = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        self.active.store(false, Ordering::Release);
        self.stop_all_inner();
    }
    pub fn play(&self, slot: usize) -> bool {
        self.play_with_barrier(slot, || {})
    }
    /// 验证状态、槽位和两路容量后，在提交播放前执行一次控制线程屏障。
    /// 拒绝请求不会执行屏障。屏障持有 gate 和生产者锁，不得重新调用播放控制。
    pub fn play_with_barrier(&self, slot: usize, before_submit: impl FnOnce()) -> bool {
        self.play_with_checked_barrier(slot, || {
            before_submit();
            true
        })
    }
    /// 屏障完成后复核请求；返回 false 时主输出与监听均不接受播放。
    /// 状态、槽位和容量先检查。屏障持有 gate 和生产者锁，不得重入播放控制。
    pub fn play_with_checked_barrier(
        &self,
        slot: usize,
        before_submit: impl FnOnce() -> bool,
    ) -> bool {
        let _guard = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if !self.active() {
            return false;
        }
        match &self.monitor {
            Some(monitor) => {
                if !self
                    .control
                    .play_linked_with_checked_barrier(slot, monitor, before_submit)
                {
                    return false;
                }
                self.monitor_plays.fetch_add(1, Ordering::Relaxed);
            }
            None => {
                if !self.control.play_with_checked_barrier(slot, before_submit) {
                    return false;
                }
            }
        }
        true
    }
    pub fn stop_all(&self) {
        let _guard = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        self.stop_all_inner();
    }
    fn stop_all_inner(&self) {
        self.control.stop_all();
        if let Some(monitor) = &self.monitor {
            monitor.stop_all();
        }
    }
    pub fn set_slot_gain(&self, slot: usize, db: f32) -> bool {
        let ok = self.control.set_slot_gain(slot, db);
        if let Some(monitor) = &self.monitor {
            monitor.set_slot_gain(slot, db);
        }
        ok
    }
    pub fn set_gains(&self, mic: f32, sfx: f32) {
        self.control.mic_gain_db.set(mic);
        self.control.sfx_gain_db.set(sfx);
        if let Some(monitor) = &self.monitor {
            monitor.sfx_gain_db.set(sfx);
        }
    }
    /// 持久循环参数与播放/待机提交串行，主输出和监听同时更新。
    pub fn set_looping(&self, looping: bool) {
        let _guard = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        self.control.set_looping(looping);
        if let Some(monitor) = &self.monitor {
            monitor.set_looping(looping);
        }
    }
}

pub struct MonitorOptions {
    pub device: DeviceId,
    pub format: StreamFormat,
    pub slots: Vec<SfxSlot>,
}
pub struct SessionOptions {
    pub input: DeviceId,
    pub output: DeviceId,
    pub input_format: StreamFormat,
    pub output_format: StreamFormat,
    pub buffer_ms: u32,
    pub slots: Vec<SfxSlot>,
    pub mic_gain_db: f32,
    pub sfx_gain_db: f32,
    pub monitor: Option<MonitorOptions>,
}

/// One owner for the primary streams and the persistent monitor stream.
pub struct Session {
    pub playback: Arc<Playback>,
    pub stats: Arc<EngineStats>,
    pub output_format: StreamFormat,
    pub health: Arc<StreamHealth>,
    pub monitor_health: Arc<StreamHealth>,
    pub dump_dropped_samples: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
pub struct DumpOptions {
    pub path: PathBuf,
    pub max_frames: usize,
}

// Dump queues are opt-in and belong to the detachable render state, never the native shell.
enum DumpRequest {
    File(DumpOptions),
    #[cfg(test)]
    Writer {
        writer: Box<dyn std::io::Write + Send>,
        max_frames: usize,
    },
}
struct DumpProducer {
    queue: rtrb::Producer<f32>,
    remaining: usize,
    dropped: Arc<AtomicU64>,
}
struct DumpDrain {
    queue: rtrb::Consumer<f32>,
    writer: Box<dyn std::io::Write + Send>,
}
struct RenderState {
    processor: RenderProcessor,
    dump: Option<DumpProducer>,
}
impl RenderState {
    fn process(&mut self, output: &mut [f32]) {
        self.processor.process(output);
        if let Some(dump) = &mut self.dump {
            let count = output.len().min(dump.remaining);
            for &sample in &output[..count] {
                if dump.queue.push(sample).is_err() {
                    dump.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            dump.remaining -= count;
        }
    }
}
impl DumpDrain {
    fn prepare(
        request: Option<DumpRequest>,
        channels: usize,
        dropped: Arc<AtomicU64>,
    ) -> Result<(Option<DumpProducer>, Option<Self>)> {
        let Some(request) = request else {
            return Ok((None, None));
        };
        let (writer, max_frames): (Box<dyn std::io::Write + Send>, usize) = match request {
            DumpRequest::File(options) => (
                Box::new(std::io::BufWriter::new(
                    std::fs::File::create(options.path).context("创建音频诊断文件失败")?,
                )),
                options.max_frames,
            ),
            #[cfg(test)]
            DumpRequest::Writer { writer, max_frames } => (writer, max_frames),
        };
        let (producer, consumer) = rtrb::RingBuffer::<f32>::new(65_536);
        Ok((
            Some(DumpProducer {
                queue: producer,
                remaining: max_frames.saturating_mul(channels),
                dropped,
            }),
            Some(Self {
                queue: consumer,
                writer,
            }),
        ))
    }
    // Only called by the audio owner; callbacks only enqueue bounded PCM samples.
    fn drain(&mut self) -> Result<()> {
        let mut bytes = [0u8; 4096];
        loop {
            let mut count = 0;
            while count < bytes.len() {
                match self.queue.pop() {
                    Ok(sample) => {
                        bytes[count..count + 4].copy_from_slice(&sample.to_le_bytes());
                        count += 4;
                    }
                    Err(_) => break,
                }
            }
            if count == 0 {
                break;
            }
            self.writer
                .write_all(&bytes[..count])
                .context("写入音频诊断文件失败")?;
        }
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.drain()?;
        self.writer.flush().context("刷新音频诊断文件失败")
    }
}

impl Session {
    pub fn open(options: SessionOptions) -> Result<Self> {
        Self::open_with_dump(options, None)
    }
    pub fn open_with_dump(options: SessionOptions, dump: Option<DumpOptions>) -> Result<Self> {
        let health = Arc::new(StreamHealth::default());
        let monitor_health = Arc::new(StreamHealth::default());
        Self::open_backends(
            options,
            CpalBackend::with_health(health.clone()),
            CpalBackend::with_health(monitor_health.clone()),
            health,
            monitor_health,
            dump.map(DumpRequest::File),
            Duration::from_secs(8),
        )
    }
    pub fn open_with_backend<B: AudioBackend + 'static>(
        options: SessionOptions,
        backend: B,
        monitor_backend: B,
    ) -> Result<Self> {
        Self::open_backends(
            options,
            backend,
            monitor_backend,
            Arc::new(StreamHealth::default()),
            Arc::new(StreamHealth::default()),
            None,
            Duration::from_secs(8),
        )
    }
    fn open_backends<B: AudioBackend + 'static>(
        options: SessionOptions,
        backend: B,
        monitor_backend: B,
        health: Arc<StreamHealth>,
        monitor_health: Arc<StreamHealth>,
        dump: Option<DumpRequest>,
        startup_timeout: Duration,
    ) -> Result<Self> {
        let parts = EngineParts::build(
            options.input_format,
            options.output_format,
            options.slots,
            options.buffer_ms,
        );
        let stats = parts.stats.clone();
        let monitor_parts = options.monitor.as_ref().map(|m| {
            EngineParts::build(
                StreamFormat::stereo(m.format.sample_rate),
                m.format,
                m.slots.clone(),
                options.buffer_ms,
            )
        });
        let playback = Arc::new(Playback::new(
            parts.control.clone(),
            monitor_parts.as_ref().map(|p| p.control.clone()),
        ));
        playback.set_gains(options.mic_gain_db, options.sfx_gain_db);
        if let Some(m) = &monitor_parts {
            m.control.mic_gain_db.set(-100.0);
        }
        let shutdown = Arc::new(AtomicBool::new(false));
        let owner_shutdown = shutdown.clone();
        let output_format = options.output_format;
        let dump_dropped_samples = Arc::new(AtomicU64::new(0));
        let callback_dropped_samples = dump_dropped_samples.clone();
        let owner_health = health.clone();
        let owner_playback = playback.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("audio-session".into())
            .spawn(move || {
                // A driver may retain closures; only tiny detached shells survive session cleanup.
                let render = Arc::new(Mutex::new(None::<RenderState>));
                let capture = Arc::new(Mutex::new(Some(parts.capture)));
                let monitor_render = Arc::new(Mutex::new(monitor_parts.map(|p| p.render)));
                // Backend and diagnostic writers are extension boundaries. Unwinding must
                // return here so retained native callback shells are detached on every exit.
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        let (dump_producer, mut dump_drain) =
                            DumpDrain::prepare(dump, output_format.ch(), callback_dropped_samples)?;
                        *render.lock().unwrap_or_else(|e| e.into_inner()) = Some(RenderState {
                            processor: parts.render,
                            dump: dump_producer,
                        });
                        if owner_shutdown.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        let r = render.clone();
                        let output_shutdown = owner_shutdown.clone();
                        let output = backend
                            .open_output(
                                &options.output,
                                Some(options.output_format),
                                Box::new(move |data| {
                                    data.fill(0.0);
                                    if output_shutdown.load(Ordering::Acquire) {
                                        return;
                                    }
                                    if let Ok(mut guard) = r.try_lock() {
                                        if !output_shutdown.load(Ordering::Acquire) {
                                            if let Some(render) = guard.as_mut() {
                                                render.process(data);
                                            }
                                        }
                                    }
                                }),
                            )
                            .context("打开输出设备失败")?;
                        anyhow::ensure!(
                            output.format() == options.output_format,
                            "输出格式在开流时变化，请重新协商"
                        );
                        if owner_shutdown.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        let c = capture.clone();
                        let input_shutdown = owner_shutdown.clone();
                        let input = backend
                            .open_input(
                                &options.input,
                                Some(options.input_format),
                                Box::new(move |data| {
                                    if input_shutdown.load(Ordering::Acquire) {
                                        return;
                                    }
                                    if let Ok(mut guard) = c.try_lock() {
                                        if !input_shutdown.load(Ordering::Acquire) {
                                            if let Some(capture) = guard.as_mut() {
                                                capture.process(data);
                                            }
                                        }
                                    }
                                }),
                            )
                            .context("打开输入设备失败")?;
                        anyhow::ensure!(
                            input.format() == options.input_format,
                            "输入格式在开流时变化，请重新协商"
                        );
                        if owner_shutdown.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        let monitor_stream = if let Some(monitor) = options.monitor {
                            let r = monitor_render.clone();
                            let monitor_shutdown = owner_shutdown.clone();
                            let stream = monitor_backend
                                .open_output(
                                    &monitor.device,
                                    Some(monitor.format),
                                    Box::new(move |data| {
                                        data.fill(0.0);
                                        if monitor_shutdown.load(Ordering::Acquire) {
                                            return;
                                        }
                                        if let Ok(mut guard) = r.try_lock() {
                                            if !monitor_shutdown.load(Ordering::Acquire) {
                                                if let Some(render) = guard.as_mut() {
                                                    render.process(data);
                                                }
                                            }
                                        }
                                    }),
                                )
                                .context("打开监听设备失败")?;
                            anyhow::ensure!(
                                stream.format() == monitor.format,
                                "监听格式在开流时变化，请重新协商"
                            );
                            Some(stream)
                        } else {
                            None
                        };
                        if owner_shutdown.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        let _ = tx.send(Ok(()));
                        while !owner_shutdown.load(Ordering::Acquire) {
                            if let Some(dump) = &mut dump_drain {
                                dump.drain()?;
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        drop(monitor_stream);
                        drop(input);
                        drop(output);
                        if let Some(dump) = &mut dump_drain {
                            dump.finish()?;
                        }
                        Ok(())
                    }));
                let result = match result {
                    Ok(result) => result,
                    Err(payload) => {
                        owner_health.mark_panicked();
                        let detail = payload
                            .downcast_ref::<String>()
                            .map(String::as_str)
                            .or_else(|| payload.downcast_ref::<&str>().copied())
                            .unwrap_or("未知 panic");
                        Err(anyhow::anyhow!("音频会话线程 panic: {detail}"))
                    }
                };
                if let Err(error) = result {
                    owner_shutdown.store(true, Ordering::Release);
                    owner_playback.standby();
                    owner_health.report_error("输出", &format!("音频会话停止: {error:#}"));
                    let _ = tx.send(Err(format!("{error:#}")));
                }
                *render.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *capture.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *monitor_render.lock().unwrap_or_else(|e| e.into_inner()) = None;
            })?;
        match rx.recv_timeout(startup_timeout) {
            Ok(Ok(())) => Ok(Self {
                playback,
                stats,
                output_format,
                health,
                monitor_health,
                dump_dropped_samples,
                shutdown,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                shutdown.store(true, Ordering::Release);
                let _ = thread.join();
                anyhow::bail!(e)
            }
            Err(e) => {
                // Driver calls cannot be forcibly interrupted. A timed-out owner sees this
                // flag immediately after returning and closes its own streams.
                shutdown.store(true, Ordering::Release);
                anyhow::bail!("音频设备启动超时或线程中断: {e}")
            }
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.playback.standby();
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod regression_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn exclusive_pair() -> (EngineParts, EngineParts, Playback) {
        let a = SfxSlot {
            name: "A".into(),
            samples: Arc::new([0.25, 0.0].repeat(48_000)),
            looping: true,
            gain_db: 0.0,
        };
        let b = SfxSlot {
            name: "B".into(),
            samples: Arc::new([0.0, 0.4].repeat(48_000)),
            looping: true,
            gain_db: 0.0,
        };
        let format = StreamFormat::stereo(48_000);
        let monitor_format = StreamFormat::new(44_100, 6);
        let primary = EngineParts::build(format, format, vec![a.clone(), b.clone()], 20);
        let monitor = EngineParts::build(monitor_format, monitor_format, vec![a, b], 20);
        let playback = Playback::new(primary.control.clone(), Some(monitor.control.clone()));
        playback.set_gains(-100.0, 0.0);
        monitor.control.mic_gain_db.set(-100.0);
        (primary, monitor, playback)
    }

    #[test]
    fn exclusive_new_trigger_replaces_primary_and_monitor_pcm_together() {
        let (mut primary, mut monitor, playback) = exclusive_pair();
        let mut main_pcm = vec![0.0; 2_048 * 2];
        let mut monitor_pcm = vec![0.0; 2_048 * 6];
        assert!(playback.play(0));
        primary.render.process(&mut main_pcm);
        monitor.render.process(&mut monitor_pcm);
        assert!(playback.play(1));
        primary.render.process(&mut main_pcm);
        monitor.render.process(&mut monitor_pcm);
        let main = (
            main_pcm[1_500 * 2],
            main_pcm[1_500 * 2 + 1],
            primary.control.active_slot(),
        );
        let heard = (
            monitor_pcm[1_500 * 6],
            monitor_pcm[1_500 * 6 + 1],
            monitor.control.active_slot(),
        );
        assert!(
            main.0.abs() < 1e-6
                && heard.0.abs() < 1e-6
                && (main.1 - 0.4).abs() < 1e-6
                && (heard.1 - 0.4).abs() < 1e-6
                && main.2 == 1
                && heard.2 == 1,
            "primary and monitor must both select only B; got primary={main:?}, monitor={heard:?}"
        );
        for (pcm, channels) in [(&main_pcm, 2), (&monitor_pcm, 6)] {
            assert!(
                pcm.chunks_exact(channels)
                    .all(|f| f[0].abs() < 1e-6 || f[1].abs() < 1e-6),
                "主输出和监听每帧只能含一个音效"
            );
        }
    }

    #[test]
    fn linked_backpressure_rejects_both_outputs_then_latest_play_recovers() {
        for full_monitor in [false, true] {
            let (mut primary, mut monitor, playback) = exclusive_pair();
            let mut main_pcm = vec![0.0; 4096];
            let mut monitor_pcm = vec![0.0; 2048 * 6];
            assert!(playback.play(0));
            primary.render.process(&mut main_pcm);
            monitor.render.process(&mut monitor_pcm);
            let full = if full_monitor {
                &monitor.control
            } else {
                &primary.control
            };
            for _ in 0..256 {
                assert!(full.set_slot_gain(0, 0.0));
            }
            assert!(!playback.play(1), "一侧队列满时两侧都应拒绝切换");
            primary.render.process(&mut main_pcm);
            monitor.render.process(&mut monitor_pcm);
            assert_eq!(primary.control.active_slot(), 0);
            assert_eq!(monitor.control.active_slot(), 0);
            assert!(main_pcm.as_chunks::<2>().0.iter().all(|f| f[1] == 0.0));
            assert!(monitor_pcm.as_chunks::<6>().0.iter().all(|f| f[1] == 0.0));
            assert!(!playback.play(99));
            assert_eq!(playback.monitor_plays.load(Ordering::Relaxed), 1);
            assert!(playback.play(1));
            assert!(playback.play(0));
            assert!(playback.play(1));
            primary.render.process(&mut main_pcm);
            monitor.render.process(&mut monitor_pcm);
            assert_eq!(primary.control.active_slot(), 1);
            assert_eq!(monitor.control.active_slot(), 1);
            assert!(main_pcm[2000..]
                .as_chunks::<2>()
                .0
                .iter()
                .all(|f| f[0] == 0.0 && (f[1] - 0.4).abs() < 1e-6));
            assert!(monitor_pcm[6000..]
                .as_chunks::<6>()
                .0
                .iter()
                .all(|f| f[0] == 0.0 && (f[1] - 0.4).abs() < 1e-6));
        }
    }

    #[test]
    fn playback_barrier_preserves_current_pcm_on_either_queue_rejection() {
        for full_monitor in [false, true] {
            let (mut primary, mut monitor, playback) = exclusive_pair();
            let mut main_pcm = vec![0.0; 4096];
            let mut monitor_pcm = vec![0.0; 2048 * 6];
            let calls = std::cell::Cell::new(0);
            assert!(playback.play(0));
            primary.render.process(&mut main_pcm);
            monitor.render.process(&mut monitor_pcm);
            let full = if full_monitor {
                &monitor.control
            } else {
                &primary.control
            };
            for _ in 0..256 {
                assert!(full.set_slot_gain(0, 0.0));
            }
            assert!(!playback.play_with_barrier(1, || calls.set(calls.get() + 1)));
            assert!(!playback.play_with_barrier(99, || calls.set(calls.get() + 1)));
            assert_eq!(calls.get(), 0);
            primary.render.process(&mut main_pcm);
            monitor.render.process(&mut monitor_pcm);
            assert_eq!(primary.control.active_slot(), 0);
            assert_eq!(monitor.control.active_slot(), 0);
            assert!(playback.play_with_barrier(1, || {
                calls.set(calls.get() + 1);
                // 回调能在控制线程执行屏障时继续运行，但不能提前消费 B。
                primary.render.process(&mut main_pcm);
                monitor.render.process(&mut monitor_pcm);
                assert_eq!(primary.control.active_slot(), 0);
                assert_eq!(monitor.control.active_slot(), 0);
                assert!(main_pcm.as_chunks::<2>().0.iter().all(|f| f[1] == 0.0));
                assert!(monitor_pcm.as_chunks::<6>().0.iter().all(|f| f[1] == 0.0));
            }));
            assert_eq!(calls.get(), 1);
            primary.render.process(&mut main_pcm);
            monitor.render.process(&mut monitor_pcm);
            assert_eq!(primary.control.active_slot(), 1);
            assert_eq!(monitor.control.active_slot(), 1);
            assert!(main_pcm[3000].abs() < 1e-6 && (main_pcm[3001] - 0.4).abs() < 1e-6);
            assert!(monitor_pcm[9000].abs() < 1e-6 && (monitor_pcm[9001] - 0.4).abs() < 1e-6);
            playback.standby();
            assert!(!playback.play_with_barrier(0, || calls.set(calls.get() + 1)));
            assert_eq!(calls.get(), 1, "待机拒绝也不能执行屏障");
        }
        let format = StreamFormat::stereo(48_000);
        let primary = EngineParts::build(format, format, vec![], 20);
        let playback = Playback::new(primary.control, None);
        assert!(!playback.play_with_barrier(0, || panic!("无槽位不得执行屏障")));
    }

    #[test]
    fn checked_barrier_cancels_both_routes_without_interrupting_current_pcm() {
        for full_monitor in [false, true] {
            let (mut primary, mut monitor, playback) = exclusive_pair();
            let mut main_pcm = vec![0.0; 2048 * 2];
            let mut heard_pcm = vec![0.0; 2048 * 6];
            assert!(playback.play(0));
            primary.render.process(&mut main_pcm);
            monitor.render.process(&mut heard_pcm);
            let calls = std::cell::Cell::new(0);
            assert!(!playback.play_with_checked_barrier(1, || {
                calls.set(calls.get() + 1);
                // Drivers keep rendering while cancellation is decided. No
                // new command may be published before the decision is true.
                primary.render.process(&mut main_pcm[..128 * 2]);
                monitor.render.process(&mut heard_pcm[..117 * 6]);
                false
            }));
            assert_eq!(calls.get(), 1);
            assert_eq!(playback.monitor_plays.load(Ordering::Relaxed), 1);
            primary.render.process(&mut main_pcm);
            monitor.render.process(&mut heard_pcm);
            assert!(main_pcm
                .as_chunks::<2>()
                .0
                .iter()
                .all(|f| (f[0] - 0.25).abs() < 1e-6 && f[1] == 0.0));
            assert!(heard_pcm
                .as_chunks::<6>()
                .0
                .iter()
                .all(|f| (f[0] - 0.25).abs() < 1e-6 && f[1] == 0.0));
            assert_eq!(primary.control.playhead_frames(), 4224);
            assert_eq!(monitor.control.playhead_frames(), 4213);
            assert!(!playback.play_with_checked_barrier(99, || {
                calls.set(99);
                true
            }));
            let full = if full_monitor {
                &monitor.control
            } else {
                &primary.control
            };
            for _ in 0..256 {
                assert!(full.set_slot_gain(0, 0.0));
            }
            assert!(!playback.play_with_checked_barrier(1, || {
                calls.set(99);
                true
            }));
            assert_eq!(calls.get(), 1);
            primary.render.process(&mut main_pcm);
            monitor.render.process(&mut heard_pcm);
            assert_eq!(primary.control.active_slot(), 0);
            assert_eq!(monitor.control.active_slot(), 0);
            assert!(playback.play_with_checked_barrier(1, || {
                calls.set(calls.get() + 1);
                true
            }));
            assert_eq!(calls.get(), 2);
            assert_eq!(playback.monitor_plays.load(Ordering::Relaxed), 2);
            primary.render.process(&mut main_pcm);
            monitor.render.process(&mut heard_pcm);
            assert_exclusive_output(&main_pcm, 2);
            assert_exclusive_output(&heard_pcm, 6);
            assert_eq!(primary.control.active_slot(), 1);
            assert_eq!(monitor.control.active_slot(), 1);
            playback.standby();
            assert!(!playback.play_with_checked_barrier(0, || {
                calls.set(99);
                true
            }));
            assert_eq!(
                calls.get(),
                2,
                "standby must reject before entering the barrier"
            );
        }
    }

    #[test]
    fn stop_all_ack_requires_both_output_callbacks_and_cancels_pending_latest() {
        let (mut primary, mut monitor, playback) = exclusive_pair();
        let mut main_pcm = vec![0.0; 4096];
        let mut monitor_pcm = vec![0.0; 2048 * 6];
        assert!(playback.play(0));
        primary.render.process(&mut main_pcm);
        monitor.render.process(&mut monitor_pcm);
        assert!(playback.is_playing());
        assert!(playback.play(1));
        primary.render.process(&mut main_pcm[..64]);
        assert_eq!(primary.control.active_slot(), 1);
        playback.stop_all();
        assert!(!playback.stop_all_finished());
        primary.render.process(&mut main_pcm);
        assert!(primary.control.stop_all_finished());
        assert!(
            !playback.stop_all_finished(),
            "监听未观察停止代次不能放行新的试听"
        );
        monitor.render.process(&mut monitor_pcm);
        assert!(playback.stop_all_finished());
        assert!(!playback.is_playing());
        assert!(main_pcm[2000..].iter().all(|s| *s == 0.0));
        assert!(monitor_pcm[6000..].iter().all(|s| *s == 0.0));
        assert!(playback.play(1));
        primary.render.process(&mut main_pcm);
        monitor.render.process(&mut monitor_pcm);
        assert_eq!(primary.control.active_slot(), 1);
        assert_eq!(monitor.control.active_slot(), 1);
    }

    #[test]
    fn looping_static_false_finishes_once_on_primary_and_monitor() {
        let main_format = StreamFormat::stereo(48_000);
        let monitor_format = StreamFormat::new(44_100, 6);
        let mut primary = EngineParts::build(
            main_format,
            main_format,
            vec![SfxSlot {
                name: "one-shot".into(),
                samples: Arc::new(vec![0.3; 4800 * 2]),
                looping: false,
                gain_db: 0.0,
            }],
            20,
        );
        let mut monitor = EngineParts::build(
            monitor_format,
            monitor_format,
            vec![SfxSlot {
                name: "one-shot".into(),
                samples: Arc::new(vec![0.3; 4410 * 2]),
                looping: false,
                gain_db: 0.0,
            }],
            20,
        );
        let playback = Playback::new(primary.control.clone(), Some(monitor.control.clone()));
        playback.set_gains(-100.0, 0.0);
        let mut main_pcm = vec![0.0; 12_000 * 2];
        let mut monitor_pcm = vec![0.0; 11_025 * 6];
        assert!(playback.play(0));
        primary.render.process(&mut main_pcm);
        monitor.render.process(&mut monitor_pcm);
        assert!((main_pcm[3000 * 2] - 0.3).abs() < 1e-6);
        assert!((monitor_pcm[2756 * 6] - 0.3).abs() < 1e-6);
        assert!(main_pcm[5500 * 2..].iter().all(|s| *s == 0.0));
        assert!(monitor_pcm[5000 * 6..].iter().all(|s| *s == 0.0));
        assert!(!playback.is_playing());
    }

    #[test]
    fn looping_runtime_is_shared_across_formats_and_persistent_through_standby() {
        let main_format = StreamFormat::stereo(48_000);
        let monitor_format = StreamFormat::new(44_100, 6);
        let make_slot = |frames: usize| SfxSlot {
            name: "runtime-loop".into(),
            samples: Arc::new(vec![0.3; frames * 2]),
            looping: true,
            gain_db: 0.0,
        };
        let mut primary = EngineParts::build(main_format, main_format, vec![make_slot(4800)], 20);
        let mut monitor =
            EngineParts::build(monitor_format, monitor_format, vec![make_slot(4410)], 20);
        let playback = Playback::new(primary.control.clone(), Some(monitor.control.clone()));
        playback.set_gains(-100.0, 0.0);
        let mut main_pcm = vec![0.0; 12_000 * 2];
        let mut monitor_pcm = vec![0.0; 11_025 * 6];
        assert!(playback.play(0));
        primary.render.process(&mut main_pcm[..1000 * 2]);
        monitor.render.process(&mut monitor_pcm[..919 * 6]);
        for control in [&primary.control, &monitor.control] {
            for _ in 0..256 {
                assert!(control.set_slot_gain(0, 0.0));
            }
        }
        playback.set_looping(false);
        primary.render.process(&mut main_pcm);
        monitor.render.process(&mut monitor_pcm);
        assert!(main_pcm[4500 * 2..].iter().all(|s| *s == 0.0));
        assert!(monitor_pcm[4100 * 6..].iter().all(|s| *s == 0.0));
        assert!(!playback.is_playing());
        assert!(playback.play(0));
        primary.render.process(&mut main_pcm[..1000 * 2]);
        monitor.render.process(&mut monitor_pcm[..919 * 6]);
        playback.set_looping(true);
        primary.render.process(&mut main_pcm[..128 * 2]);
        monitor.render.process(&mut monitor_pcm[..118 * 6]);
        assert_eq!(primary.control.playhead_frames(), 1128);
        assert_eq!(monitor.control.playhead_frames(), 1037);
        primary.render.process(&mut main_pcm);
        monitor.render.process(&mut monitor_pcm);
        assert!((main_pcm[10_000 * 2] - 0.3).abs() < 1e-6);
        assert!((monitor_pcm[9000 * 6] - 0.3).abs() < 1e-6);
        assert!(playback.is_playing());
        playback.standby();
        playback.set_looping(true);
        playback.set_looping(false);
        primary.render.process(&mut main_pcm);
        monitor.render.process(&mut monitor_pcm);
        assert!(playback.stop_all_finished());
        assert!(!playback.is_playing());
        assert!(!playback.play(0));
        playback.activate();
        assert!(playback.play(0));
        primary.render.process(&mut main_pcm);
        monitor.render.process(&mut monitor_pcm);
        assert!(main_pcm[5500 * 2..].iter().all(|s| *s == 0.0));
        assert!(monitor_pcm[5000 * 6..].iter().all(|s| *s == 0.0));
        assert!(
            !playback.is_playing(),
            "待机中的最后设置应作用于重新激活的播放"
        );
    }

    fn assert_exclusive_output(pcm: &[f32], channels: usize) {
        for frame in pcm.chunks_exact(channels) {
            assert!(frame.iter().all(|s| s.is_finite()));
            assert!(
                frame[0].abs() < 1e-6 || frame[1].abs() < 1e-6,
                "A and B must not share an output frame: {frame:?}"
            );
            for extra in &frame[2..] {
                assert!(
                    (*extra - (frame[0] + frame[1]) * 0.5).abs() < 1e-6,
                    "surround channels retain the stereo fold-down"
                );
            }
        }
    }

    fn combination_pair() -> (EngineParts, EngineParts, Playback) {
        let make_slots = |frames: usize| {
            vec![
                SfxSlot {
                    name: "A".into(),
                    samples: Arc::new([0.25, 0.0].repeat(frames)),
                    looping: false,
                    gain_db: 0.0,
                },
                SfxSlot {
                    name: "B".into(),
                    samples: Arc::new([0.0, 0.4].repeat(frames)),
                    looping: false,
                    gain_db: 0.0,
                },
            ]
        };
        let main = StreamFormat::stereo(48_000);
        let heard = StreamFormat::new(44_100, 6);
        let primary = EngineParts::build(main, main, make_slots(4800), 20);
        let monitor = EngineParts::build(heard, heard, make_slots(4410), 20);
        let playback = Playback::new(primary.control.clone(), Some(monitor.control.clone()));
        playback.set_gains(-100.0, 0.0);
        monitor.control.mic_gain_db.set(-100.0);
        (primary, monitor, playback)
    }

    #[test]
    fn combination_all_three_action_sequences_agree_across_rates_and_callback_schedules() {
        // Actions 0/1 select A/B, 2 stops, 3/4 choose looping off/on. Test
        // deferred callbacks and either endpoint running ahead; control calls
        // must preserve the same choice without requiring synchronized drivers.
        let mut main_pcm = vec![0.0; 9000 * 2];
        let mut heard_pcm = vec![0.0; 8269 * 6];
        for initial in [None, Some(0), Some(1)] {
            for initial_loop in [false, true] {
                for schedule in 0..3 {
                    for encoded in 0..125 {
                        let sequence = [encoded % 5, encoded / 5 % 5, encoded / 25];
                        let (mut primary, mut monitor, playback) = combination_pair();
                        playback.set_looping(initial_loop);
                        let mut selected = initial;
                        let mut looping = initial_loop;
                        if let Some(slot) = initial {
                            assert!(playback.play(slot));
                            primary.render.process(&mut main_pcm[..1000 * 2]);
                            monitor.render.process(&mut heard_pcm[..919 * 6]);
                        }
                        for action in sequence {
                            match action {
                                slot @ (0 | 1) => {
                                    assert!(playback.play(slot));
                                    selected = Some(slot);
                                }
                                2 => {
                                    playback.stop_all();
                                    selected = None;
                                }
                                3 => {
                                    playback.set_looping(false);
                                    looping = false;
                                }
                                4 => {
                                    playback.set_looping(true);
                                    looping = true;
                                }
                                _ => unreachable!(),
                            }
                            if schedule == 1 {
                                primary.render.process(&mut main_pcm[..997 * 2]);
                                assert_exclusive_output(&main_pcm[..997 * 2], 2);
                            }
                            if schedule == 2 {
                                monitor.render.process(&mut heard_pcm[..916 * 6]);
                                assert_exclusive_output(&heard_pcm[..916 * 6], 6);
                            }
                        }
                        primary.render.process(&mut main_pcm);
                        monitor.render.process(&mut heard_pcm);
                        assert_exclusive_output(&main_pcm, 2);
                        assert_exclusive_output(&heard_pcm, 6);
                        let expected = selected.filter(|_| looping);
                        for control in [&primary.control, &monitor.control] {
                            assert_eq!(control.active_slot(), expected.map_or(-1, |s| s as i32),
                                "initial={initial:?}, initial_loop={initial_loop}, schedule={schedule}, sequence={sequence:?}");
                            assert_eq!(control.is_playing(), expected.is_some());
                        }
                        for (pcm, channels) in [(&main_pcm, 2), (&heard_pcm, 6)] {
                            let tail = &pcm[7000 * channels..];
                            match expected {
                                Some(0) => {
                                    assert!(tail
                                        .chunks_exact(channels)
                                        .all(|f| (f[0] - 0.25).abs() < 1e-6 && f[1] == 0.0))
                                }
                                Some(1) => assert!(tail
                                    .chunks_exact(channels)
                                    .all(|f| f[0] == 0.0 && (f[1] - 0.4).abs() < 1e-6)),
                                None => assert!(tail.iter().all(|s| *s == 0.0)),
                                _ => unreachable!(),
                            }
                        }
                        if expected.is_none() {
                            assert!(playback.stop_all_finished());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn combination_backpressure_stop_loop_and_reactivation_keep_both_outputs_consistent() {
        for full_monitor in [false, true] {
            let (mut primary, mut monitor, playback) = combination_pair();
            let mut main_pcm = vec![0.0; 9000 * 2];
            let mut heard_pcm = vec![0.0; 8269 * 6];
            playback.set_looping(true);
            assert!(playback.play(0));
            primary.render.process(&mut main_pcm[..1000 * 2]);
            monitor.render.process(&mut heard_pcm[..919 * 6]);
            let full = if full_monitor {
                &monitor.control
            } else {
                &primary.control
            };
            for _ in 0..256 {
                assert!(full.set_slot_gain(0, 0.0));
            }
            let barriers = std::cell::Cell::new(0);
            assert!(!playback.play_with_barrier(1, || barriers.set(barriers.get() + 1)));
            assert_eq!(barriers.get(), 0);
            // A rejected switch preserves A on both routes; the loop parameter
            // and high priority stop must still work with a full endpoint.
            playback.set_looping(false);
            playback.stop_all();
            playback.set_looping(true);
            primary.render.process(&mut main_pcm);
            monitor.render.process(&mut heard_pcm);
            assert_exclusive_output(&main_pcm, 2);
            assert_exclusive_output(&heard_pcm, 6);
            assert!(main_pcm[1000 * 2..].iter().all(|s| *s == 0.0));
            assert!(heard_pcm[919 * 6..].iter().all(|s| *s == 0.0));
            assert!(playback.stop_all_finished());
            assert!(!playback.is_playing());
            playback.standby();
            playback.set_looping(false);
            playback.set_looping(true);
            playback.set_looping(false);
            assert!(!playback.play_with_barrier(1, || barriers.set(barriers.get() + 1)));
            playback.activate();
            assert!(playback.play_with_barrier(1, || barriers.set(barriers.get() + 1)));
            assert_eq!(barriers.get(), 1);
            primary.render.process(&mut main_pcm);
            monitor.render.process(&mut heard_pcm);
            assert_exclusive_output(&main_pcm, 2);
            assert_exclusive_output(&heard_pcm, 6);
            assert!(main_pcm.as_chunks::<2>().0.iter().all(|f| f[0] == 0.0));
            assert!(heard_pcm.as_chunks::<6>().0.iter().all(|f| f[0] == 0.0));
            assert!((main_pcm[2000 * 2 + 1] - 0.4).abs() < 1e-6);
            assert!((heard_pcm[1800 * 6 + 1] - 0.4).abs() < 1e-6);
            assert!(main_pcm[5500 * 2..].iter().all(|s| *s == 0.0));
            assert!(heard_pcm[5000 * 6..].iter().all(|s| *s == 0.0));
            assert!(!playback.is_playing());
        }
    }

    #[test]
    fn standby_blocks_every_trigger_and_reactivation_works() {
        let f = StreamFormat::stereo(44_100);
        let slot = SfxSlot {
            name: "test".into(),
            samples: Arc::new(vec![0.1; 200]),
            looping: false,
            gain_db: 0.0,
        };
        let parts = EngineParts::build(f, f, vec![slot.clone()], 20);
        let monitor = EngineParts::build(f, f, vec![slot], 20);
        let playback = Playback::new(parts.control, Some(monitor.control));
        playback.standby();
        assert!(!playback.play(0));
        assert_eq!(playback.monitor_plays.load(Ordering::Relaxed), 0);
        playback.activate();
        assert!(playback.play(0));
        assert_eq!(playback.monitor_plays.load(Ordering::Relaxed), 1);
        playback.stop_all();
    }
    #[test]
    fn monitor_conversion_preserves_one_second_and_pitch() {
        let source: Vec<_> = (0..48_000)
            .flat_map(|i| {
                let x = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin();
                [x, x]
            })
            .collect();
        let output = resample_stereo(&source, 48_000, 44_100);
        assert_eq!(output.len(), 88_200);
        let crossings = output
            .as_chunks::<2>()
            .0
            .iter()
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|w| w[0][0] <= 0.0 && w[1][0] > 0.0)
            .count();
        assert_eq!(crossings, 440);
    }
    #[test]
    fn preview_maps_mono_stereo_and_surround_without_changing_speed() {
        for channels in [1, 2, 6] {
            let mut cursor = SampleCursor::new(Arc::new(vec![0.2, 0.4, 0.6, 0.8]), 0.0);
            let mut output = vec![9.0; channels * 3 + 1];
            cursor.render(&mut output, channels);
            if channels == 1 {
                assert!((output[0] - 0.3).abs() < 1e-6);
            } else {
                assert_eq!(&output[..2], &[0.2, 0.4]);
            }
            assert!(output[channels * 2..].iter().all(|s| *s == 0.0));
            if channels > 2 {
                assert!(output[2..channels].iter().all(|s| *s == 0.0));
            }
        }
    }
}
