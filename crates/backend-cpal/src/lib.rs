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
        mut cb: CaptureCallback,
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
        let err_fn = |e| log::error!("[audio] 输入流错误: {e}");

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
        mut cb: RenderCallback,
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
        let err_fn = |e| log::error!("[audio] 输出流错误: {e}");

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
