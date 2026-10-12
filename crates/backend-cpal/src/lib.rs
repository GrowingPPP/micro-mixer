//! cpal 音频后端 —— 一套代码同时覆盖 **macOS(CoreAudio)** 与 **Windows(WASAPI)**。
//!
//! 这是"纯环回架构"带来的最大红利：换平台只需要换这个 crate，
//! 混音引擎、重采样、限幅、UI 全部复用。

use anyhow::{anyhow, Context, Result};
pub use audio_core::device::StreamHealth;
use std::sync::Arc;
mod native;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use audio_core::device::{
    AudioBackend, AudioStream, CaptureCallback, DeviceId, DeviceInfo, Direction, RenderCallback,
    StreamFormat,
};

// Legacy test hooks are isolated from production streams. New code consumes its
// session's StreamHealth; keeping these wrappers avoids breaking external tools.
static LEGACY_HEALTH: std::sync::OnceLock<Arc<StreamHealth>> = std::sync::OnceLock::new();
fn legacy_health() -> &'static Arc<StreamHealth> {
    LEGACY_HEALTH.get_or_init(|| Arc::new(StreamHealth::default()))
}
pub fn take_stream_error() -> Option<String> {
    legacy_health().take_error()
}
#[doc(hidden)]
pub fn report_stream_error(direction: &str, msg: &str) {
    legacy_health().report_error(direction, msg);
}
pub fn engine_panicked() -> bool {
    legacy_health().panicked()
}
pub fn clear_engine_panic() {
    legacy_health().clear();
}

fn note_callback_panic(health: &StreamHealth) {
    if health.mark_panicked() {
        log::error!("[audio] 音频回调 panic 已隔离，所属会话需要重启");
    }
}
fn guard_input(mut cb: CaptureCallback, health: Arc<StreamHealth>) -> CaptureCallback {
    Box::new(move |data| {
        if health.panicked() {
            return;
        }
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(data))).is_err() {
            note_callback_panic(&health);
        }
    })
}
fn guard_output(mut cb: RenderCallback, health: Arc<StreamHealth>) -> RenderCallback {
    Box::new(move |data| {
        if health.panicked() {
            data.fill(0.0);
            return;
        }
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(data))).is_err() {
            data.fill(0.0);
            note_callback_panic(&health);
        }
    })
}

pub struct CpalBackend {
    health: Arc<StreamHealth>,
}

impl Default for CpalBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl CpalBackend {
    pub fn new() -> Self {
        Self::with_health(Arc::new(StreamHealth::default()))
    }
    pub fn with_health(health: Arc<StreamHealth>) -> Self {
        Self { health }
    }
    pub fn health(&self) -> Arc<StreamHealth> {
        self.health.clone()
    }

    /// Passive native inventory for hotplug checks. Does not create an AudioUnit
    /// or activate a WASAPI audio client, including for unsupported directions.
    pub fn available_device_ids() -> Result<Vec<DeviceId>> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        return Ok(native::available_ids()?
            .into_iter()
            .map(DeviceId::new)
            .collect());
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let mut ids: Vec<_> = Self::new().devices()?.into_iter().map(|d| d.id).collect();
            ids.sort();
            ids.dedup();
            Ok(ids)
        }
    }

    fn host() -> cpal::Host {
        cpal::default_host()
    }

    fn inventory(host: &cpal::Host) -> Result<Vec<Binding>> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            for _ in 0..3 {
                let before = native::inventory()?;
                let devices: Vec<_> = host.devices()?.collect();
                let after = native::inventory()?;
                if before == after && devices.len() == before.len() {
                    return Ok(devices
                        .into_iter()
                        .zip(before)
                        .map(|(device, endpoint)| Binding { device, endpoint })
                        .collect());
                }
            }
            Err(anyhow!("音频设备正在变化，无法确认端点标识；请重试"))
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        host.devices()?
            .enumerate()
            .map(|(index, device)| {
                let name = device.name()?;
                let endpoint = native::Endpoint {
                    id: format!("cpal:{name}:{index}"),
                    name,
                    default_input: false,
                    default_output: false,
                    input: device
                        .default_input_config()
                        .ok()
                        .map(|c| StreamFormat::new(c.sample_rate().0, c.channels())),
                    output: device
                        .default_output_config()
                        .ok()
                        .map(|c| StreamFormat::new(c.sample_rate().0, c.channels())),
                };
                Ok(Binding { device, endpoint })
            })
            .collect()
    }

    fn resolve(host: &cpal::Host, id: &DeviceId, dir: Direction) -> Result<Binding> {
        let devices = Self::inventory(host)?;
        let available: Vec<_> = devices.into_iter().filter(|d| d.supports(dir)).collect();
        let names: Vec<_> = available
            .iter()
            .map(|d| (d.endpoint.id.as_str(), d.endpoint.name.as_str()))
            .collect();
        let index = resolve_identity(&names, id.as_str())?;
        Ok(available.into_iter().nth(index).expect("resolved index"))
    }
}

struct Binding {
    device: cpal::Device,
    endpoint: native::Endpoint,
}
impl std::ops::Deref for Binding {
    type Target = cpal::Device;
    fn deref(&self) -> &Self::Target {
        &self.device
    }
}
impl Binding {
    fn name(&self) -> Result<String> {
        Ok(self.endpoint.name.clone())
    }
    fn supports(&self, dir: Direction) -> bool {
        self.endpoint.format(dir).is_some()
    }
}

fn device_infos(endpoints: impl IntoIterator<Item = native::Endpoint>) -> Vec<DeviceInfo> {
    let mut out = Vec::new();
    for endpoint in endpoints {
        for direction in [Direction::Input, Direction::Output] {
            let Some(format) = endpoint.format(direction) else {
                continue;
            };
            out.push(DeviceInfo {
                id: DeviceId::new(&endpoint.id),
                name: endpoint.name.clone(),
                direction,
                is_default: match direction {
                    Direction::Input => endpoint.default_input,
                    Direction::Output => endpoint.default_output,
                },
                max_channels: format.channels,
                default_sample_rate: format.sample_rate,
            });
        }
    }
    out.sort_by(|a, b| {
        a.direction
            .label()
            .cmp(b.direction.label())
            .then_with(|| b.is_default.cmp(&a.is_default))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

#[cfg(target_os = "macos")]
fn native_format(id: &DeviceId, direction: Direction) -> Result<StreamFormat> {
    let available: Vec<_> = native::inventory()?
        .into_iter()
        .filter(|d| d.format(direction).is_some())
        .collect();
    let names: Vec<_> = available
        .iter()
        .map(|d| (d.id.as_str(), d.name.as_str()))
        .collect();
    let index = resolve_identity(&names, id.as_str())?;
    let format = available[index]
        .format(direction)
        .expect("filtered direction");
    anyhow::ensure!(
        format.sample_rate > 0 && format.channels > 0,
        "设备当前格式不可用: {format}"
    );
    Ok(format)
}
fn resolve_identity(devices: &[(&str, &str)], wanted: &str) -> Result<usize> {
    if let Some(index) = devices.iter().position(|(id, _)| *id == wanted) {
        return Ok(index);
    }
    // Stable IDs must never fall back to substring matching a different endpoint.
    anyhow::ensure!(
        !wanted.starts_with("coreaudio:") && !wanted.starts_with("wasapi:"),
        "设备端点「{wanted}」已不可用"
    );
    let exact: Vec<_> = devices
        .iter()
        .enumerate()
        .filter(|(_, (_, name))| name.eq_ignore_ascii_case(wanted))
        .map(|(i, _)| i)
        .collect();
    let matches: Vec<_> = if exact.is_empty() {
        let wanted = wanted.to_lowercase();
        devices
            .iter()
            .enumerate()
            .filter(|(_, (_, name))| name.to_lowercase().contains(&wanted))
            .map(|(i, _)| i)
            .collect()
    } else {
        exact
    };
    match matches.as_slice() {
        [index] => Ok(*index),
        [] => Err(anyhow!("找不到设备「{wanted}」")),
        _ => Err(anyhow!("设备名「{wanted}」有多个匹配，请使用稳定设备标识")),
    }
}

/// 为方向 `dir` 挑选最接近 `prefer` 的配置。
/// 设备的默认格式能不能直接用？
///
/// 抽出来是为了能测 —— "优先用默认格式"这条策略的边界条件
/// （单声道设备、0 采样率）不该只靠读代码确认。
fn acceptable_default(cfg: &cpal::SupportedStreamConfig, prefer: StreamFormat) -> bool {
    cfg.sample_rate().0 > 0
        && cfg.channels() > 0
        && cfg.channels() >= prefer.channels
        && supported_sample_format(cfg.sample_format())
}

fn pick_config(
    dev: &cpal::Device,
    dir: Direction,
    prefer: StreamFormat,
) -> Result<cpal::SupportedStreamConfig> {
    // ── ① 优先用设备**自己的默认格式** ──────────────────────────────
    //
    // 为什么不直接用我们想要的 48000：
    //
    // · **Windows / WASAPI**：共享模式下只有一个格式是**保证**支持的，就是
    //   `GetMixFormat()` 返回的那个。`supported_output_configs()` 是拿 mix format
    //   去"变造"出各种采样率（cpal 源码里的注释明说了），所以请求 48000 而设备
    //   混音格式是 44100 时，系统的音频引擎会**隐式重采样** —— 多一层延迟。
    //   这正是 M3-02 要避免的。
    //
    // · **macOS / CoreAudio**：同理。实测这台机器的默认输出就是 44100，
    //   而我们一直在请求 48000 —— 也就是说 macOS 上其实也一直在重采样，
    //   只是 CoreAudio 不报错、听感上也听不出来，所以一直没发现。
    //
    // 设备默认格式拿不到时，再退回下面的挑挑选选。
    let default_cfg = match dir {
        Direction::Input => dev.default_input_config().ok(),
        Direction::Output => dev.default_output_config().ok(),
    };
    if let Some(d) = default_cfg {
        if acceptable_default(&d, prefer) {
            return Ok(d);
        }
    }

    // ── ② 回退：在设备支持的配置里挑 ──
    let mut exact: Option<cpal::SupportedStreamConfig> = None;
    let mut relaxed: Option<cpal::SupportedStreamConfig> = None;

    let mut consider = |range: cpal::SupportedStreamConfigRange| {
        if !supported_sample_format(range.sample_format()) || range.channels() == 0 {
            return;
        }
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

fn supported_sample_format(format: cpal::SampleFormat) -> bool {
    matches!(
        format,
        cpal::SampleFormat::I8
            | cpal::SampleFormat::I16
            | cpal::SampleFormat::I32
            | cpal::SampleFormat::I64
            | cpal::SampleFormat::U8
            | cpal::SampleFormat::U16
            | cpal::SampleFormat::U32
            | cpal::SampleFormat::U64
            | cpal::SampleFormat::F32
            | cpal::SampleFormat::F64
    )
}

fn pick_exact_config(
    dev: &cpal::Device,
    dir: Direction,
    format: StreamFormat,
) -> Result<cpal::SupportedStreamConfig> {
    anyhow::ensure!(
        format.sample_rate > 0 && format.channels > 0,
        "音频格式无效: {format}"
    );
    let default = match dir {
        Direction::Input => dev.default_input_config().ok(),
        Direction::Output => dev.default_output_config().ok(),
    };
    if let Some(cfg) = default {
        if cfg.sample_rate().0 == format.sample_rate
            && cfg.channels() == format.channels
            && supported_sample_format(cfg.sample_format())
        {
            return Ok(cfg);
        }
    }
    let ranges: Vec<_> = match dir {
        Direction::Input => dev.supported_input_configs()?.collect(),
        Direction::Output => dev.supported_output_configs()?.collect(),
    };
    select_exact_config(ranges, format)
}

fn select_exact_config(
    ranges: Vec<cpal::SupportedStreamConfigRange>,
    format: StreamFormat,
) -> Result<cpal::SupportedStreamConfig> {
    ranges
        .into_iter()
        .find(|r| {
            r.channels() == format.channels
                && r.min_sample_rate().0 <= format.sample_rate
                && format.sample_rate <= r.max_sample_rate().0
                && supported_sample_format(r.sample_format())
        })
        .map(|r| r.with_sample_rate(cpal::SampleRate(format.sample_rate)))
        .ok_or_else(|| anyhow!("设备格式已改变或不支持 {format}，请重新协商"))
}

struct PcmConverter {
    scratch: Box<[f32]>,
}
impl PcmConverter {
    fn new(channels: usize) -> Self {
        Self {
            scratch: vec![0.0; (8192 / channels.max(1)).max(1) * channels.max(1)]
                .into_boxed_slice(),
        }
    }
    fn capture<T>(&mut self, data: &[T], cb: &mut CaptureCallback)
    where
        T: cpal::Sample,
        f32: cpal::FromSample<T>,
    {
        for chunk in data.chunks(self.scratch.len()) {
            for (out, input) in self.scratch.iter_mut().zip(chunk) {
                *out = (*input).to_sample::<f32>();
            }
            cb(&self.scratch[..chunk.len()]);
        }
    }
    fn render<T>(&mut self, data: &mut [T], cb: &mut RenderCallback)
    where
        T: cpal::Sample + cpal::FromSample<f32>,
    {
        for chunk in data.chunks_mut(self.scratch.len()) {
            let scratch = &mut self.scratch[..chunk.len()];
            scratch.fill(0.0);
            cb(scratch);
            for (out, input) in chunk.iter_mut().zip(scratch) {
                *out = T::from_sample(input.clamp(-1.0, 1.0));
            }
        }
    }
}
fn build_input<T>(
    dev: &cpal::Device,
    config: &cpal::StreamConfig,
    mut cb: CaptureCallback,
    health: Arc<StreamHealth>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let mut converter = PcmConverter::new(config.channels as usize);
    Ok(dev.build_input_stream(
        config,
        move |data: &[T], _| converter.capture(data, &mut cb),
        move |e| health.report_error("输入", &e.to_string()),
        None,
    )?)
}
fn build_output<T>(
    dev: &cpal::Device,
    config: &cpal::StreamConfig,
    mut cb: RenderCallback,
    health: Arc<StreamHealth>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let mut converter = PcmConverter::new(config.channels as usize);
    Ok(dev.build_output_stream(
        config,
        move |data: &mut [T], _| converter.render(data, &mut cb),
        move |e| health.report_error("输出", &e.to_string()),
        None,
    )?)
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
    health: Arc<StreamHealth>,
}

impl AudioStream for CpalStream {
    fn format(&self) -> StreamFormat {
        self.format
    }
    fn device_name(&self) -> &str {
        &self.name
    }
    fn health(&self) -> Option<Arc<StreamHealth>> {
        Some(self.health.clone())
    }
}

// ─────────────────────────────────────────── 后端实现

impl AudioBackend for CpalBackend {
    fn backend_name(&self) -> &'static str {
        "cpal"
    }

    fn devices(&self) -> Result<Vec<DeviceInfo>> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        return Ok(device_infos(native::inventory()?));
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        Ok(device_infos(
            Self::inventory(&Self::host())?
                .into_iter()
                .map(|b| b.endpoint),
        ))
    }

    fn default_input(&self) -> Option<DeviceId> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        return native::default_id(Direction::Input)
            .ok()
            .flatten()
            .map(DeviceId::new);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        self.devices()
            .ok()?
            .into_iter()
            .find(|d| d.direction == Direction::Input && d.is_default)
            .map(|d| d.id)
    }
    fn default_output(&self) -> Option<DeviceId> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        return native::default_id(Direction::Output)
            .ok()
            .flatten()
            .map(DeviceId::new);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        self.devices()
            .ok()?
            .into_iter()
            .find(|d| d.direction == Direction::Output && d.is_default)
            .map(|d| d.id)
    }

    fn input_format(&self, id: &DeviceId, prefer: Option<StreamFormat>) -> Result<StreamFormat> {
        #[cfg(target_os = "macos")]
        {
            let _ = prefer;
            native_format(id, Direction::Input)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let host = Self::host();
            let dev = Self::resolve(&host, id, Direction::Input)?;
            let cfg = pick_config(
                &dev,
                Direction::Input,
                prefer.unwrap_or(StreamFormat::new(48_000, 1)),
            )?;
            Ok(StreamFormat::new(cfg.sample_rate().0, cfg.channels()))
        }
    }

    fn output_format(&self, id: &DeviceId, prefer: Option<StreamFormat>) -> Result<StreamFormat> {
        #[cfg(target_os = "macos")]
        {
            let _ = prefer;
            native_format(id, Direction::Output)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let host = Self::host();
            let dev = Self::resolve(&host, id, Direction::Output)?;
            let cfg = pick_config(
                &dev,
                Direction::Output,
                prefer.unwrap_or(StreamFormat::new(48_000, 2)),
            )?;
            Ok(StreamFormat::new(cfg.sample_rate().0, cfg.channels()))
        }
    }

    fn open_input(
        &self,
        id: &DeviceId,
        prefer: Option<StreamFormat>,
        cb: CaptureCallback,
    ) -> Result<Box<dyn AudioStream>> {
        let host = Self::host();
        let dev = Self::resolve(&host, id, Direction::Input)?;
        let cfg = match prefer {
            Some(fmt) => pick_exact_config(&dev, Direction::Input, fmt)?,
            None => pick_config(&dev, Direction::Input, StreamFormat::new(48_000, 1))?,
        };
        let fmt = StreamFormat::new(cfg.sample_rate().0, cfg.channels());
        let sc = stream_config(&cfg);
        let name = dev.name().unwrap_or_else(|_| id.to_string());
        // 用设备默认格式（不重采样）还是我们挑的格式？出问题时这行很关键
        let is_default = match dev.default_input_config() {
            Ok(d) => d.sample_rate().0 == cfg.sample_rate().0 && d.channels() == cfg.channels(),
            Err(_) => false,
        };
        log::info!(
            "[audio] 输入流 {}Hz/{}ch（{}）",
            cfg.sample_rate().0,
            cfg.channels(),
            if is_default {
                "设备默认格式，无重采样"
            } else {
                "协商后的格式"
            }
        );
        let cb = guard_input(cb, self.health.clone());
        let stream = match cfg.sample_format() {
            cpal::SampleFormat::I8 => build_input::<i8>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::I16 => build_input::<i16>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::I32 => build_input::<i32>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::I64 => build_input::<i64>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::U8 => build_input::<u8>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::U16 => build_input::<u16>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::U32 => build_input::<u32>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::U64 => build_input::<u64>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::F32 => build_input::<f32>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::F64 => build_input::<f64>(&dev, &sc, cb, self.health.clone())?,
            other => anyhow::bail!("不支持的采样格式: {other:?}"),
        };
        stream.play().context("启动输入流失败")?;
        Ok(Box::new(CpalStream {
            _stream: stream,
            format: fmt,
            name,
            health: self.health.clone(),
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
        let cfg = match prefer {
            Some(fmt) => pick_exact_config(&dev, Direction::Output, fmt)?,
            None => pick_config(&dev, Direction::Output, StreamFormat::new(48_000, 2))?,
        };
        let fmt = StreamFormat::new(cfg.sample_rate().0, cfg.channels());
        let sc = stream_config(&cfg);
        let name = dev.name().unwrap_or_else(|_| id.to_string());
        let is_default = match dev.default_output_config() {
            Ok(d) => d.sample_rate().0 == cfg.sample_rate().0 && d.channels() == cfg.channels(),
            Err(_) => false,
        };
        log::info!(
            "[audio] 输出流 {}Hz/{}ch（{}）",
            cfg.sample_rate().0,
            cfg.channels(),
            if is_default {
                "设备默认格式，无重采样"
            } else {
                "协商后的格式"
            }
        );
        let cb = guard_output(cb, self.health.clone());
        let stream = match cfg.sample_format() {
            cpal::SampleFormat::I8 => build_output::<i8>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::I16 => build_output::<i16>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::I32 => build_output::<i32>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::I64 => build_output::<i64>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::U8 => build_output::<u8>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::U16 => build_output::<u16>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::U32 => build_output::<u32>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::U64 => build_output::<u64>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::F32 => build_output::<f32>(&dev, &sc, cb, self.health.clone())?,
            cpal::SampleFormat::F64 => build_output::<f64>(&dev, &sc, cb, self.health.clone())?,
            other => anyhow::bail!("不支持的采样格式: {other:?}"),
        };
        stream.play().context("启动输出流失败")?;
        Ok(Box::new(CpalStream {
            _stream: stream,
            format: fmt,
            name,
            health: self.health.clone(),
        }))
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    fn cfg(rate: u32, ch: u16) -> cpal::SupportedStreamConfig {
        cpal::SupportedStreamConfig::new(
            ch,
            cpal::SampleRate(rate),
            cpal::SupportedBufferSize::Range { min: 0, max: 0 },
            cpal::SampleFormat::F32,
        )
    }

    #[test]
    fn opening_uses_probed_format_even_when_the_default_changes() {
        let range = |rate, channels| {
            cpal::SupportedStreamConfigRange::new(
                channels,
                cpal::SampleRate(rate),
                cpal::SampleRate(rate),
                cpal::SupportedBufferSize::Unknown,
                cpal::SampleFormat::I32,
            )
        };
        let requested = StreamFormat::new(48_000, 2);
        let chosen = select_exact_config(
            vec![range(44_100, 2), range(48_000, 1), range(48_000, 2)],
            requested,
        )
        .unwrap();
        assert_eq!(chosen.sample_rate().0, 48_000);
        assert_eq!(chosen.channels(), 2);
        assert!(select_exact_config(vec![range(44_100, 2), range(48_000, 1)], requested).is_err());
    }

    #[test]
    fn duplicate_names_resolve_only_by_stable_identity() {
        let devices = [
            ("coreaudio:one", "USB microphone"),
            ("coreaudio:two", "USB microphone"),
        ];
        assert_eq!(resolve_identity(&devices, "coreaudio:two").unwrap(), 1);
        assert!(resolve_identity(&devices, "USB microphone").is_err());
        assert!(resolve_identity(&devices, "USB").is_err());
        assert!(resolve_identity(&devices, "coreaudio:missing").is_err());
        assert_eq!(
            resolve_identity(&[("wasapi:id", "CABLE Input")], "cable input").unwrap(),
            0
        );
    }

    #[test]
    fn passive_inventory_preserves_same_names_direction_and_native_format() {
        let endpoints = vec![
            native::Endpoint {
                id: "coreaudio:input".into(),
                name: "USB Audio".into(),
                default_input: true,
                default_output: false,
                input: Some(StreamFormat::new(44_100, 1)),
                output: None,
            },
            native::Endpoint {
                id: "coreaudio:output".into(),
                name: "USB Audio".into(),
                default_input: false,
                default_output: true,
                input: None,
                output: Some(StreamFormat::new(96_000, 6)),
            },
            native::Endpoint {
                id: "coreaudio:duplex".into(),
                name: "Loopback".into(),
                default_input: false,
                default_output: false,
                input: Some(StreamFormat::new(48_000, 2)),
                output: Some(StreamFormat::new(48_000, 2)),
            },
        ];
        let devices = device_infos(endpoints);
        assert_eq!(devices.len(), 4);
        let inputs: Vec<_> = devices
            .iter()
            .filter(|d| d.direction == Direction::Input)
            .collect();
        let outputs: Vec<_> = devices
            .iter()
            .filter(|d| d.direction == Direction::Output)
            .collect();
        assert_eq!(inputs.len(), 2);
        assert_eq!(outputs.len(), 2);
        assert_eq!(inputs[0].id, DeviceId::new("coreaudio:input"));
        assert_eq!(inputs[0].default_sample_rate, 44_100);
        assert_eq!(inputs[0].max_channels, 1);
        assert_eq!(outputs[0].id, DeviceId::new("coreaudio:output"));
        assert_eq!(outputs[0].default_sample_rate, 96_000);
        assert_eq!(outputs[0].max_channels, 6);
        assert_eq!(inputs[0].name, outputs[0].name);
        assert!(inputs[0].is_default && outputs[0].is_default);
    }

    #[test]
    fn callback_panic_and_device_errors_are_isolated_by_session() {
        let primary = Arc::new(StreamHealth::default());
        let preview = Arc::new(StreamHealth::default());
        preview.report_error("输出", "preview disconnected");
        assert!(primary.take_error().is_none());
        assert!(preview
            .take_error()
            .unwrap()
            .contains("preview disconnected"));
        let mut render = guard_output(Box::new(|_| panic!("isolated test panic")), preview.clone());
        let mut output = [1.0; 16];
        render(&mut output);
        assert_eq!(output, [0.0; 16]);
        assert!(preview.panicked());
        assert!(!primary.panicked());
        preview.clear();
        assert!(!preview.panicked());
        let mut capture = guard_input(Box::new(|_| panic!("input test panic")), primary.clone());
        capture(&[0.0; 8]);
        assert!(primary.panicked());
        assert!(!preview.panicked());
    }

    #[test]
    fn oversized_integer_pcm_buffers_keep_all_frames_without_growing_scratch() {
        let mut converter = PcmConverter::new(6);
        let capacity = converter.scratch.len();
        let samples = 30_000; // well beyond the old 8192/16384 scratch allocations
        let frames = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let frames_c = frames.clone();
        let mut capture: CaptureCallback = Box::new(move |data| {
            assert_eq!(data.len() % 6, 0);
            assert!(data.iter().all(|s| (*s - 0.5).abs() < 1e-6));
            frames_c.fetch_add(data.len() / 6, std::sync::atomic::Ordering::Relaxed);
        });
        converter.capture(&vec![16_384i16; samples], &mut capture);
        assert_eq!(
            frames.load(std::sync::atomic::Ordering::Relaxed),
            samples / 6
        );
        let mut render: RenderCallback = Box::new(|out| {
            assert_eq!(out.len() % 6, 0);
            out.fill(0.25);
        });
        let mut output = vec![0i32; samples];
        converter.render(&mut output, &mut render);
        assert!(output
            .iter()
            .all(|s| (*s as f64 / i32::MAX as f64 - 0.25).abs() < 1e-8));
        assert_eq!(converter.scratch.len(), capacity);
    }

    #[test]
    fn unsigned_pcm_silence_and_64_bit_formats_are_converted_correctly() {
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let received_c = received.clone();
        let mut capture: CaptureCallback =
            Box::new(move |samples| received_c.lock().unwrap().extend_from_slice(samples));
        let mut converter = PcmConverter::new(1);
        converter.capture(&[128u8, 0, 255], &mut capture);
        converter.capture(&[0i64, i64::MAX], &mut capture);
        assert_eq!(
            &*received.lock().unwrap(),
            &[0.0, -1.0, 127.0 / 128.0, 0.0, 1.0]
        );
        let mut render: RenderCallback = Box::new(|out| out.fill(0.0));
        let mut output = [0u64; 3];
        converter.render(&mut output, &mut render);
        assert_eq!(output, [1u64 << 63; 3]);
    }

    #[test]
    fn 默认格式够用就直接用_避免系统重采样() {
        // 立体声输出设备给 44100 —— 我们要的就是它，不该硬要 48000
        assert!(acceptable_default(
            &cfg(44100, 2),
            StreamFormat::new(48000, 2)
        ));
        // 单声道输入设备
        assert!(acceptable_default(
            &cfg(48000, 1),
            StreamFormat::new(48000, 1)
        ));
    }

    #[test]
    fn 声道不够就退回协商() {
        // 输出要立体声，设备默认只有单声道 → 不能用
        assert!(!acceptable_default(
            &cfg(48000, 1),
            StreamFormat::new(48000, 2)
        ));
    }

    #[test]
    fn 零采样率必须拒掉() {
        // 有些驱动在设备刚插上时会返回 0，直接拿去开流会失败得很莫名
        assert!(!acceptable_default(&cfg(0, 2), StreamFormat::new(48000, 2)));
    }
}
