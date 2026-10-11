//! cpal 音频后端 —— 一套代码同时覆盖 **macOS(CoreAudio)** 与 **Windows(WASAPI)**。
//!
//! 这是"纯环回架构"带来的最大红利：换平台只需要换这个 crate，
//! 混音引擎、重采样、限幅、UI 全部复用。

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use audio_core::device::{
    AudioBackend, AudioStream, CaptureCallback, DeviceId, DeviceInfo, Direction, RenderCallback,
    StreamFormat,
};

/// 音频回调里发生过 panic。
///
/// 一旦置位，说明**引擎内部状态已不可信** —— 光把它兜住还不够，
/// 上层（看门狗）应当把音频重启一遍。
static CALLBACK_PANICKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 最近一次**音频流错误**。
///
/// 为什么需要它：设备被拔掉时，最可靠的信号不是"去枚举一遍设备列表"
/// （那样既慢又要另起一个后端，实测会和主线程开流撞死），
/// 而是**流自己报的错** —— CoreAudio 和 WASAPI 都会在设备失效时回调错误。
///
/// 这个信号是平台无关的，看门狗读它就能知道"哪个方向断了"。
static LAST_STREAM_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 取走最近一次流错误（读一次就清）。返回值形如 `"输入: 设备已失效"`。
pub fn take_stream_error() -> Option<String> {
    LAST_STREAM_ERROR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

/// 手动上报一次流错误。**仅用于自测** —— 真机上这条路是
/// CoreAudio / WASAPI 在设备失效时回调进来的，没法在自动化测试里真的拔设备。
///
/// 不做成测试钩子的话，"设备掉线检测"就只是"接上了但没验过"。
#[doc(hidden)]
pub fn report_stream_error(direction: &str, msg: &str) {
    record_stream_error(direction, msg);
}

fn record_stream_error(direction: &str, msg: &str) {
    let mut g = LAST_STREAM_ERROR.lock().unwrap_or_else(|e| e.into_inner());
    // 只留第一条 —— 设备掉了之后错误会连着刷，后面的没有信息量
    if g.is_none() {
        *g = Some(format!("{direction}: {msg}"));
    }
}

/// 音频回调里是否 panic 过。看门狗据此决定要不要重启音频。
pub fn engine_panicked() -> bool {
    CALLBACK_PANICKED.load(std::sync::atomic::Ordering::Relaxed)
}

/// 重启音频成功后清除标记。
pub fn clear_engine_panic() {
    CALLBACK_PANICKED.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// 记一次回调 panic。**只报一次** —— 音频回调每秒被调上百次，
/// 真出问题会连续 panic，不节流的话日志会被刷爆，反而看不到第一条。
fn note_callback_panic() {
    use std::sync::atomic::Ordering;
    if !CALLBACK_PANICKED.swap(true, Ordering::SeqCst) {
        log::error!(
            "[audio] ⚠ 音频回调里发生了 panic —— 已就地兜住（不让它跨 FFI 边界，那是 UB）\
             ，但引擎状态已不可信，应重启音频"
        );
    }
}

/// 给**输入**回调套一层 panic 兜底。
///
/// ⚠️ **这不是可选优化，是必须的。** cpal 的回调是从 `extern "C" fn` 里调过来的
/// （coreaudio-rs 的 `input_proc` / render proc），panic 一旦 unwind 出去就是
/// **跨 FFI 边界 = 未定义行为**，可能表现为莫名其妙的崩溃或死锁。
/// 所以必须在**我们这一侧**就地兜住。
fn guard_input(mut cb: CaptureCallback) -> CaptureCallback {
    Box::new(move |data: &[f32]| {
        // AssertUnwindSafe：回调持有 `&mut` 状态，编译器无法证明它 unwind-safe。
        // 接受这个风险，但**不当作没事发生** —— 见 note_callback_panic。
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(data)));
        if r.is_err() {
            note_callback_panic();
        }
    })
}

/// 给**输出**回调套一层 panic 兜底。
///
/// 比输入侧多一件事：**必须把缓冲清零**。输出缓冲是复用的，
/// panic 之后里面是上一次的残留 —— 不清零的话，游戏会收到一段垃圾音频。
fn guard_output(mut cb: RenderCallback) -> RenderCallback {
    Box::new(move |data: &mut [f32]| {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(data)));
        if r.is_err() {
            data.fill(0.0); // 宁可静音，也不能把残留样本送出去
            note_callback_panic();
        }
    })
}

pub struct CpalBackend;

impl Default for CpalBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl CpalBackend {
    pub fn new() -> Self {
        Self
    }

    fn host() -> cpal::Host {
        cpal::default_host()
    }

    fn input_devices(host: &cpal::Host) -> Vec<cpal::Device> {
        host.input_devices()
            .map(|it| it.collect())
            .unwrap_or_default()
    }

    fn output_devices(host: &cpal::Host) -> Vec<cpal::Device> {
        host.output_devices()
            .map(|it| it.collect())
            .unwrap_or_default()
    }

    fn resolve(host: &cpal::Host, id: &DeviceId, dir: Direction) -> Result<cpal::Device> {
        let want = id.as_str().to_lowercase();
        let devs = match dir {
            Direction::Input => Self::input_devices(host),
            Direction::Output => Self::output_devices(host),
        };
        // 先精确匹配，再包含匹配
        devs.iter()
            .find(|d| d.name().map(|n| n.to_lowercase() == want).unwrap_or(false))
            .or_else(|| {
                devs.iter().find(|d| {
                    d.name()
                        .map(|n| {
                            let n = n.to_lowercase();
                            n.contains(&want) || want.contains(&n)
                        })
                        .unwrap_or(false)
                })
            })
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "找不到{}设备「{}」。用 `list` 命令查看可用设备。",
                    dir.label(),
                    id
                )
            })
    }
}

/// 为方向 `dir` 挑选最接近 `prefer` 的配置。
fn pick_config(
    dev: &cpal::Device,
    dir: Direction,
    prefer: StreamFormat,
) -> Result<cpal::SupportedStreamConfig> {
    let mut exact: Option<cpal::SupportedStreamConfig> = None;
    let mut relaxed: Option<cpal::SupportedStreamConfig> = None;

    let mut consider = |range: cpal::SupportedStreamConfigRange| {
        let (min, max) = (range.min_sample_rate().0, range.max_sample_rate().0);
        let cfg = if prefer.sample_rate >= min && prefer.sample_rate <= max {
            range.with_sample_rate(cpal::SampleRate(prefer.sample_rate))
        } else {
            range.with_max_sample_rate()
        };
        if prefer.sample_rate >= min
            && prefer.sample_rate <= max
            && cfg.channels() >= prefer.channels
            && exact.is_none()
        {
            exact = Some(cfg);
        } else if relaxed.is_none() {
            relaxed = Some(cfg);
        }
    };

    match dir {
        Direction::Input => {
            for r in dev.supported_input_configs().context("读取输入配置失败")? {
                consider(r);
            }
        }
        Direction::Output => {
            for r in dev.supported_output_configs().context("读取输出配置失败")? {
                consider(r);
            }
        }
    }

    exact
        .or(relaxed)
        .ok_or_else(|| anyhow!("设备没有可用的音频配置"))
}

fn stream_config(cfg: &cpal::SupportedStreamConfig) -> cpal::StreamConfig {
    cpal::StreamConfig {
        channels: cfg.channels(),
        sample_rate: cfg.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    }
}

// ─────────────────────────────────────────── 流句柄

struct CpalStream {
    _stream: cpal::Stream,
    format: StreamFormat,
    name: String,
}

impl AudioStream for CpalStream {
    fn format(&self) -> StreamFormat {
        self.format
    }
    fn device_name(&self) -> &str {
        &self.name
    }
}

// ─────────────────────────────────────────── 后端实现

impl AudioBackend for CpalBackend {
    fn backend_name(&self) -> &'static str {
        "cpal"
    }

    fn devices(&self) -> Result<Vec<DeviceInfo>> {
        let host = Self::host();
        let default_in = host.default_input_device().and_then(|d| d.name().ok());
        let default_out = host.default_output_device().and_then(|d| d.name().ok());
        let mut out: Vec<DeviceInfo> = Vec::new();

        let mut push = |dev: &cpal::Device, dir: Direction, default: Option<&String>| {
            let Ok(name) = dev.name() else { return };
            // 同一设备在输入/输出两侧都会出现，按 (name, direction) 去重
            if out.iter().any(|d| d.name == name && d.direction == dir) {
                return;
            }
            let (max_ch, rate) = match pick_config(dev, dir, StreamFormat::new(48_000, 1)) {
                Ok(c) => (c.channels(), c.sample_rate().0),
                Err(_) => (0, 0),
            };
            out.push(DeviceInfo {
                id: DeviceId::new(name.clone()),
                is_default: default.map(|d| *d == name).unwrap_or(false),
                name,
                direction: dir,
                max_channels: max_ch,
                default_sample_rate: rate,
            });
        };

        for d in Self::input_devices(&host) {
            push(&d, Direction::Input, default_in.as_ref());
        }
        for d in Self::output_devices(&host) {
            push(&d, Direction::Output, default_out.as_ref());
        }
        out.sort_by(|a, b| {
            a.direction
                .label()
                .cmp(b.direction.label())
                .then_with(|| b.is_default.cmp(&a.is_default))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(out)
    }

    fn default_input(&self) -> Option<DeviceId> {
        Self::host()
            .default_input_device()
            .and_then(|d| d.name().ok())
            .map(DeviceId::new)
    }

    fn default_output(&self) -> Option<DeviceId> {
        Self::host()
            .default_output_device()
            .and_then(|d| d.name().ok())
            .map(DeviceId::new)
    }

    fn input_format(&self, id: &DeviceId, prefer: Option<StreamFormat>) -> Result<StreamFormat> {
        let host = Self::host();
        let dev = Self::resolve(&host, id, Direction::Input)?;
        let cfg = pick_config(
            &dev,
            Direction::Input,
            prefer.unwrap_or(StreamFormat::new(48_000, 1)),
        )?;
        Ok(StreamFormat::new(cfg.sample_rate().0, cfg.channels()))
    }

    fn output_format(&self, id: &DeviceId, prefer: Option<StreamFormat>) -> Result<StreamFormat> {
        let host = Self::host();
        let dev = Self::resolve(&host, id, Direction::Output)?;
        let cfg = pick_config(
            &dev,
            Direction::Output,
            prefer.unwrap_or(StreamFormat::new(48_000, 2)),
        )?;
        Ok(StreamFormat::new(cfg.sample_rate().0, cfg.channels()))
    }

    fn open_input(
        &self,
        id: &DeviceId,
        prefer: Option<StreamFormat>,
        cb: CaptureCallback,
    ) -> Result<Box<dyn AudioStream>> {
        let host = Self::host();
        let dev = Self::resolve(&host, id, Direction::Input)?;
        let cfg = pick_config(
            &dev,
            Direction::Input,
            prefer.unwrap_or(StreamFormat::new(48_000, 1)),
        )?;
        let fmt = StreamFormat::new(cfg.sample_rate().0, cfg.channels());
        let sc = stream_config(&cfg);
        let name = dev.name().unwrap_or_else(|_| id.to_string());
        let err_fn = |e: cpal::StreamError| {
            log::error!("[audio] 输入流错误: {e}");
            record_stream_error("输入", &e.to_string());
        };

        // 在分派到各采样格式之前先包一层 —— 6 条分支就都受保护了
        let mut cb = guard_input(cb);

        let stream = match cfg.sample_format() {
            cpal::SampleFormat::F32 => dev.build_input_stream(
                &sc,
                move |data: &[f32], _: &cpal::InputCallbackInfo| cb(data),
                err_fn,
                None,
            )?,
            cpal::SampleFormat::I16 => {
                let mut scratch: Vec<f32> = Vec::with_capacity(8192);
                dev.build_input_stream(
                    &sc,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        scratch.clear();
                        scratch.extend(data.iter().map(|&s| s as f32 / 32768.0));
                        cb(&scratch);
                    },
                    err_fn,
                    None,
                )?
            }
            cpal::SampleFormat::U16 => {
                let mut scratch: Vec<f32> = Vec::with_capacity(8192);
                dev.build_input_stream(
                    &sc,
                    move |data: &[u16], _: &cpal::InputCallbackInfo| {
                        scratch.clear();
                        scratch.extend(data.iter().map(|&s| (s as f32 - 32768.0) / 32768.0));
                        cb(&scratch);
                    },
                    err_fn,
                    None,
                )?
            }
            other => bail!("暂不支持的采样格式: {other:?}"),
        };
        stream.play().context("启动输入流失败")?;
        Ok(Box::new(CpalStream {
            _stream: stream,
            format: fmt,
            name,
        }))
    }

    fn open_output(
        &self,
        id: &DeviceId,
        prefer: Option<StreamFormat>,
        cb: RenderCallback,
    ) -> Result<Box<dyn AudioStream>> {
        let host = Self::host();
        let dev = Self::resolve(&host, id, Direction::Output)?;
        let cfg = pick_config(
            &dev,
            Direction::Output,
            prefer.unwrap_or(StreamFormat::new(48_000, 2)),
        )?;
        let fmt = StreamFormat::new(cfg.sample_rate().0, cfg.channels());
        let sc = stream_config(&cfg);
        let name = dev.name().unwrap_or_else(|_| id.to_string());
        let err_fn = |e: cpal::StreamError| {
            log::error!("[audio] 输出流错误: {e}");
            record_stream_error("输出", &e.to_string());
        };

        // 同上：先包一层 panic 兜底
        let mut cb = guard_output(cb);

        // 先建一个 f32 暂存区，再由回调填满（回调必须写满整个 buffer）
        let mut scratch: Vec<f32> = Vec::with_capacity(16384);

        let stream = match cfg.sample_format() {
            cpal::SampleFormat::F32 => dev.build_output_stream(
                &sc,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| cb(data),
                err_fn,
                None,
            )?,
            cpal::SampleFormat::I16 => dev.build_output_stream(
                &sc,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    scratch.resize(data.len(), 0.0);
                    cb(&mut scratch);
                    for (o, s) in data.iter_mut().zip(scratch.iter()) {
                        *o = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                    }
                },
                err_fn,
                None,
            )?,
            cpal::SampleFormat::U16 => dev.build_output_stream(
                &sc,
                move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                    scratch.resize(data.len(), 0.0);
                    cb(&mut scratch);
                    for (o, s) in data.iter_mut().zip(scratch.iter()) {
                        *o = ((s.clamp(-1.0, 1.0) * 32767.0) + 32768.0) as u16;
                    }
                },
                err_fn,
                None,
            )?,
            other => bail!("暂不支持的采样格式: {other:?}"),
        };
        stream.play().context("启动输出流失败")?;
        Ok(Box::new(CpalStream {
            _stream: stream,
            format: fmt,
            name,
        }))
    }
}
