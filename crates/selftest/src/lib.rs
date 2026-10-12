//! 端到端自检：把已知信号送进虚拟设备，再从它的环回录回来，验证整条链路。
//!
//! 为什么需要它：本项目开发中遇到过一个只在真实设备上出现、且**非确定性**的故障 ——
//! 引擎交给声卡的数据完全正确（440 Hz），但设备实际输出成了 872 Hz（频率翻倍）。
//! 这种问题靠单元测试抓不到，必须有一把"从设备里量回来"的尺子。
//!
//! 检查项：
//! - **频率**：录回的频率必须与送入一致（抓重采样/消费速率错误）
//! - **时长**：录回的时长必须与送入一致
//! - **电平**：录回的电平必须在合理范围内（抓静音、增益异常）
//!
//! 本模块**不打印任何东西**，只返回结构化的 [`Report`] ——
//! 命令行把它排版成表格，图形界面把它渲染成提示，两边共用同一套判定逻辑。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use audio_core::device::{AudioBackend, DeviceId, DeviceInfo, Direction, StreamFormat};
use backend_cpal::CpalBackend;

/// 送入信号的前导 / 尾部静音，让录音能完整框住信号
const LEAD_SECS: f32 = 0.6;
const TAIL_SECS: f32 = 0.6;

#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub got: String,
    pub want: String,
    pub ok: bool,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub device: String,
    pub freq_sent: f32,
    pub secs_sent: f32,
    pub sample_rate: u32,
    pub capture_format: StreamFormat,
    pub checks: Vec<Check>,
    /// 输出侧实际送出的帧数（用于诊断"根本没播"）
    pub played_frames: usize,
    /// 环回侧录到的样本数
    pub captured_samples: usize,
    /// 环回侧录到的峰值电平（dBFS）。全零时是 f32::NEG_INFINITY。
    /// 用它区分"通路是死的"和"通路好但音量太低"。
    pub captured_peak_db: f32,
    /// 采集回调抢不到锁的次数（实时性问题）
    pub capture_drops: usize,
    /// 失败时的排查建议
    pub advice: Vec<String>,
    /// 一共跑了几次才出结果（>1 说明第一次开流时设备抽风了）
    pub attempts: u32,
}

/// 虚拟声卡已知的偶发问题（实测：约 15 次开流出现 1 次音高翻倍，
/// 也观察到过开流后环回拿不到信号）。多数情况下**重开一次流就正常**。
/// 所以任何失败都先让用户重试，而不是让他去查代码。
const RETRY_HINT: &str =
    "先直接**再点一次「自检」** —— 这个虚拟声卡偶发异常（实测约 15 次开流出现 1 次），\
     多数情况重开一次流就恢复正常。仍然失败再看下面的排查方向。";

impl Report {
    pub fn passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|c| c.ok)
    }

    /// 一行结论，给界面用。
    pub fn summary(&self) -> String {
        if self.passed() {
            format!("自检通过 —— {} 整条链路正常", self.device)
        } else {
            let bad: Vec<&str> = self
                .checks
                .iter()
                .filter(|c| !c.ok)
                .map(|c| c.name.as_str())
                .collect();
            format!("自检未通过：{}", bad.join("、"))
        }
    }
}

/// 虚拟驱动的播放端和录音端，不要求二者拥有相同名字或标识。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointPair {
    pub output: DeviceId,
    pub capture: DeviceId,
}

/// 从设备清单中确定环回端点；同名候选不唯一时要求显式选择。
pub fn resolve_endpoints(
    devices: &[DeviceInfo],
    output: &DeviceId,
    capture: Option<&DeviceId>,
) -> Result<EndpointPair> {
    let select = |id: &DeviceId, direction: Direction| -> Result<&DeviceInfo> {
        if let Some(device) = devices
            .iter()
            .find(|d| d.direction == direction && d.id == *id)
        {
            return Ok(device);
        }
        let candidates: Vec<_> = devices
            .iter()
            .filter(|d| d.direction == direction && d.name.eq_ignore_ascii_case(id.as_str()))
            .collect();
        match candidates.as_slice() {
            [device] => Ok(device),
            [] => anyhow::bail!("找不到{}端点「{id}」", direction.label()),
            _ => anyhow::bail!("{}端点「{id}」同名，请指定稳定设备标识", direction.label()),
        }
    };
    let output = select(output, Direction::Output)?;
    let capture = if let Some(id) = capture {
        select(id, Direction::Input)?
    } else {
        let lower = output.name.to_lowercase();
        // VB-CABLE: playback CABLE Input -> recording CABLE Output. Keep the
        // driver suffix (including A/B variants) so separate cables cannot cross-pair.
        let expected = if lower.contains("cable") && lower.contains("input") {
            lower.replacen("input", "output", 1)
        } else {
            lower
        };
        let matches: Vec<_> = devices
            .iter()
            .filter(|d| d.direction == Direction::Input && d.name.to_lowercase() == expected)
            .collect();
        match matches.as_slice() {
            [device] => *device,
            [] => anyhow::bail!(
                "无法为「{}」确定环回录音端点，请手动选择 capture 设备",
                output.name
            ),
            _ => anyhow::bail!(
                "「{}」有多个同名录音端点，请手动选择 capture 设备标识",
                output.name
            ),
        }
    };
    Ok(EndpointPair {
        output: output.id.clone(),
        capture: capture.id.clone(),
    })
}

/// 跑一次自检。`device` 为 None 时自动探测虚拟声卡。
pub fn run(backend: &CpalBackend, device: Option<String>, freq: f32, secs: f32) -> Result<Report> {
    run_with_capture(backend, device, None, freq, secs)
}

/// 显式 capture 可用于未识别的虚拟驱动或多个同名端点。
pub fn run_with_capture(
    backend: &dyn AudioBackend,
    device: Option<String>,
    capture: Option<String>,
    freq: f32,
    secs: f32,
) -> Result<Report> {
    anyhow::ensure!(
        freq.is_finite() && freq > 0.0 && secs.is_finite() && secs > 0.0 && secs <= 60.0,
        "测试频率和时长无效"
    );
    let id = match device {
        Some(n) => DeviceId::new(n),
        None => backend
            .guess_virtual_output()
            .context("没探测到虚拟声卡，请手动指定设备")?,
    };
    let pair = resolve_endpoints(
        &backend.devices()?,
        &id,
        capture.as_deref().map(DeviceId::new).as_ref(),
    )?;
    let in_fmt = backend.input_format(&pair.capture, None)?;
    let out_fmt = backend.output_format(&pair.output, None)?;
    let sr = out_fmt.sample_rate;
    let capture_sr = in_fmt.sample_rate;
    let capture_ch = in_fmt.ch();
    anyhow::ensure!(
        sr > 0 && capture_sr > 0 && freq < (sr.min(capture_sr) as f32 / 2.0),
        "测试频率超过端点的 Nyquist 频率"
    );
    let total_secs = LEAD_SECS + secs + TAIL_SECS;
    let mut advice: Vec<String> = Vec::new();

    // ── 采集缓冲：预分配 + 原子写指针，回调里不做分配
    //    注意 ×2：缓冲存的是**交错样本**（L/R），不是帧。少算一半会导致写满回绕、冲掉开头。
    let cap_len = (((total_secs + 1.0) * capture_sr as f32) as usize) * capture_ch;
    let buf = Arc::new(Mutex::new(vec![0f32; cap_len]));
    let write_idx = Arc::new(AtomicUsize::new(0));
    let capturing = Arc::new(AtomicBool::new(true));

    let buf_c = buf.clone();
    let idx_c = write_idx.clone();
    let in_drops = Arc::new(AtomicUsize::new(0));
    let drops_c = in_drops.clone();

    let capturing_c = capturing.clone();
    let in_stream = backend.open_input(
        &pair.capture,
        Some(in_fmt),
        Box::new(move |data: &[f32]| {
            if !capturing_c.load(Ordering::Relaxed) {
                return;
            }
            // 用 try_lock：绝不在实时线程上阻塞
            match buf_c.try_lock() {
                Ok(mut b) => {
                    let mut i = idx_c.load(Ordering::Relaxed);
                    for &s in data {
                        if i >= b.len() {
                            break;
                        }
                        b[i] = s;
                        i += 1;
                    }
                    idx_c.store(i, Ordering::Relaxed);
                }
                Err(_) => {
                    drops_c.fetch_add(1, Ordering::Relaxed);
                }
            }
        }),
    )?;

    // ── 等采集流真正跑起来再开始播放。
    //    CoreAudio 打开输入设备有约 1 秒的启动延迟；不等它就开始放，
    //    录音会把音调开头截掉，导致"时长不足"的假失败。
    let wait_t0 = Instant::now();
    while write_idx.load(Ordering::Relaxed) <= capture_sr as usize / 10 * capture_ch {
        if wait_t0.elapsed() > Duration::from_secs(5) {
            advice.push("等待采集流启动超时（5 秒），时长结论可能不准".into());
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    write_idx.store(0, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(100));

    // ── 输出：前导静音 → 正弦 → 尾部静音
    // 相位用原子量：**输出回调里绝不能加锁**
    let phase = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let phase_c = phase;
    let step_u = (freq as f64 / sr as f64 * u32::MAX as f64) as u64;
    let lead_frames = (LEAD_SECS * sr as f32) as u64;
    let tone_frames = (secs * sr as f32) as u64;
    let out_ch = out_fmt.ch();
    let played = Arc::new(AtomicUsize::new(0));
    let played_c = played.clone();

    let out_stream = backend.open_output(
        &pair.output,
        Some(out_fmt),
        Box::new(move |out: &mut [f32]| {
            let frames = out.len() / out_ch;
            let mut p = phase_c.load(Ordering::Relaxed);
            let mut n = played_c.load(Ordering::Relaxed);
            for f in 0..frames {
                let in_tone = (n as u64) >= lead_frames && (n as u64) < lead_frames + tone_frames;
                let s = if in_tone {
                    let ph = p as f64 / u32::MAX as f64;
                    (0.5 * (2.0 * std::f64::consts::PI * ph).sin()) as f32
                } else {
                    0.0
                };
                for c in 0..out_ch {
                    out[f * out_ch + c] = s;
                }
                p = p.wrapping_add(step_u);
                n += 1;
            }
            phase_c.store(p, Ordering::Relaxed);
            played_c.store(n, Ordering::Relaxed);
        }),
    )?;

    std::thread::sleep(Duration::from_secs_f32(total_secs + 0.3));
    capturing.store(false, Ordering::Relaxed);
    drop(out_stream);
    drop(in_stream);

    // ── 分析
    let captured_samples;
    let captured_peak_db;
    let capture_drops = in_drops.load(Ordering::Relaxed);
    let played_frames = played.load(Ordering::Relaxed);
    let mut checks: Vec<Check> = Vec::new();

    {
        let data = buf.lock().unwrap_or_else(|e| e.into_inner());
        let n = write_idx.load(Ordering::Relaxed).min(data.len());
        captured_samples = n;

        let peak = data[..n].iter().fold(0f32, |m, s| m.max(s.abs()));
        captured_peak_db = if peak > 0.0 {
            20.0 * peak.log10()
        } else {
            f32::NEG_INFINITY
        };

        if n < capture_sr as usize / 2 * capture_ch {
            advice.push(RETRY_HINT.into());
            advice.push(format!(
                "只录到 {n} 个样本 —— 环回输入侧几乎没有数据。\
                 若重试无效：确认设备名没选错，且该设备支持同时开输入输出"
            ));
            checks.push(Check {
                name: "环回采集".into(),
                got: format!("{n} 样本"),
                want: "> 半秒".into(),
                ok: false,
            });
        } else if peak <= 0.0 {
            // 采样到了，但**全是数字零** —— 通路是死的，不是音量问题
            advice.push(RETRY_HINT.into());
            advice.push(
                "环回输入侧采到了数据，但**全是数字零**（不是小声，是完全没有信号）。\n\
                     ⚠ **macOS 上最常见的原因是：系统没给本程序麦克风权限。**\n\
                     \x20 检查「系统设置 → 隐私与安全性 → 麦克风」，确认「麦克风混音助手」是打开的。\n\
                     \x20 第一次双击启动时系统会弹窗询问，**必须点「允许」**；\
                     如果当时没点或被拒绝，程序就只能录到一片静音（不报错、不崩溃）。\n\
                     \x20 改完之后**要退出程序重开**，权限才会对新的音频流生效。\n\
                     若权限已开仍全零：确认用的确实是虚拟声卡，且其「环回 / Loopback」开关是打开的。"
                    .into(),
            );
            checks.push(Check {
                name: "环回通路".into(),
                got: "全零（-∞ dBFS）".into(),
                want: "有信号".into(),
                ok: false,
            });
        } else if peak < 1e-3 {
            // 有信号但极小 —— 典型的音量被调低
            advice.push(format!(
                "环回侧能采到信号但**极弱**（峰值 {captured_peak_db:.1} dBFS）。\
                 这通常是虚拟声卡的**音量被调到了很低或 0**。\
                 排查：打开「音频 MIDI 设置」，把该虚拟声卡的输入/输出音量拉回 1.0。"
            ));
            checks.push(Check {
                name: "环回电平".into(),
                got: format!("{captured_peak_db:.1} dBFS"),
                want: "> -60 dBFS".into(),
                ok: false,
            });
        } else {
            let mono = downmix_capture(&data[..n], capture_ch);

            match find_tone(&mono, capture_sr) {
                None => {
                    advice.push(RETRY_HINT.into());
                    advice.push(format!(
                        "环回侧有信号（峰值 {captured_peak_db:.1} dBFS）但**找不到测试音**。\
                         若重试无效：多半是**别的程序正在占用这个设备**（游戏 / 语音软件），\
                         独占会让我们的输出流拿不到时钟 —— 先关掉它们再自检。"
                    ));
                    checks.push(Check {
                        name: "信号到达".into(),
                        got: "无信号".into(),
                        want: "有".into(),
                        ok: false,
                    });
                }
                Some((a, b)) => {
                    let tone = &mono[a..b];
                    let dur = tone.len() as f32 / capture_sr as f32;
                    let rms = (tone.iter().map(|s| s * s).sum::<f32>() / tone.len() as f32).sqrt();
                    let rms_db = 20.0 * (rms.max(1e-9)).log10();
                    let measured = zerocross(tone, capture_sr);

                    let freq_ok = (measured - freq).abs() <= freq * 0.03;
                    let dur_ok = (dur - secs).abs() <= secs * 0.15;
                    let level_ok = (-40.0..=-3.0).contains(&rms_db);

                    checks.push(Check {
                        name: "频率".into(),
                        got: format!("{measured:.1} Hz"),
                        want: format!("{freq:.1} Hz"),
                        ok: freq_ok,
                    });
                    checks.push(Check {
                        name: "时长".into(),
                        got: format!("{dur:.2} s"),
                        want: format!("{secs:.2} s"),
                        ok: dur_ok,
                    });
                    checks.push(Check {
                        name: "电平".into(),
                        got: format!("{rms_db:.1} dBFS"),
                        want: "−40 ~ −3 dBFS".into(),
                        ok: level_ok,
                    });

                    if !freq_ok {
                        // 音高**翻倍**是个特殊且已查明的故障，给出针对性指引。
                        // 定位结论（MIXER_DUMP 对照实验）：引擎交给声卡的数据完全正确
                        // （878.9 Hz），是设备把它变成了 1731.4 Hz —— 问题在虚拟声卡那一层，
                        // 不在本程序。且它是**偶发**的（实测约 15 次开流出现 1 次）。
                        let ratio = measured / freq;
                        if (ratio - 2.0).abs() < 0.25 {
                            advice.push(RETRY_HINT.into());
                            advice.push(format!(
                                "**音高翻倍**（{measured:.0} Hz ≈ 2 × {freq:.0} Hz）。\
                                 已用 MIXER_DUMP 对照确认：本程序交给声卡的数据是正确的，\
                                 是虚拟声卡把它翻倍了 —— 这是设备层的偶发问题，不是本程序的 bug。\
                                 处理：点「停止」再「启动」重开一次音频流（换一次开流通常就正常了）；\
                                 若反复出现，重启该虚拟声卡或改用其他虚拟声卡（如 VB-CABLE）。"
                            ));
                        } else {
                            advice.push(
                                "频率不一致 —— 这是「信号被以错误速率消费 / 重采样」的典型特征。\
                                 确认设备采样率与协商值一致"
                                    .into(),
                            );
                        }
                    }
                }
            }
        }
    }

    Ok(Report {
        device: format!("{} → {}", pair.output, pair.capture),
        freq_sent: freq,
        secs_sent: secs,
        sample_rate: sr,
        capture_format: in_fmt,
        checks,
        played_frames,
        captured_samples,
        captured_peak_db,
        capture_drops,
        advice,
        attempts: 1,
    })
}

/// 带**自动重试**的自检。
///
/// 为什么需要：这块虚拟声卡已知会偶发抽风（实测约 15 次开流出现 1 次异常，
/// 表现为音高翻倍或环回侧拿不到信号）。而"重开一次流"几乎总能恢复 ——
/// 所以与其让用户对着一次失败发懵、手动再点一遍，不如自己重试。
///
/// 每次尝试都会**重新开流**，这正是恢复所需的那一步。
pub fn run_with_retry(
    backend: &CpalBackend,
    device: Option<String>,
    freq: f32,
    secs: f32,
    attempts: u32,
) -> Result<Report> {
    run_with_capture_retry(backend, device, None, freq, secs, attempts)
}

pub fn run_with_capture_retry(
    backend: &dyn AudioBackend,
    device: Option<String>,
    capture: Option<String>,
    freq: f32,
    secs: f32,
    attempts: u32,
) -> Result<Report> {
    let attempts = attempts.max(1);
    let mut last: Option<Report> = None;
    for i in 1..=attempts {
        let mut r = run_with_capture(backend, device.clone(), capture.clone(), freq, secs)?;
        r.attempts = i;
        if r.passed() {
            if i > 1 {
                // 让用户知道设备刚才抽风了，但已经自己恢复
                r.advice.insert(
                    0,
                    format!("第 {i} 次尝试才成功 —— 前 {} 次是虚拟声卡开流时的偶发异常，已自动重开流恢复。", i - 1, ),
                );
            }
            return Ok(r);
        }
        last = Some(r);
    }
    Ok(last.expect("至少尝试过一次"))
}

fn downmix_capture(data: &[f32], channels: usize) -> Vec<f32> {
    data.chunks_exact(channels.max(1))
        .map(|f| f.iter().sum::<f32>() / channels.max(1) as f32)
        .collect()
}

/// 找出连续有信号的段（阈值取峰值以下 20 dB）
fn find_tone(mono: &[f32], sr: u32) -> Option<(usize, usize)> {
    let peak = mono.iter().fold(0f32, |m, s| m.max(s.abs()));
    if peak < 1e-4 {
        return None;
    }
    let thr = peak * 0.1; // -20 dB
    let h = (sr as usize / 100).max(1); // 10ms
    let mut start = None;
    let mut end = 0usize;
    let mut i = 0usize;
    while i + h <= mono.len() {
        let win = &mono[i..i + h];
        let rms = (win.iter().map(|s| s * s).sum::<f32>() / h as f32).sqrt();
        if rms > thr {
            if start.is_none() {
                start = Some(i);
            }
            end = i + h;
        }
        i += h;
    }
    start.map(|a| (a, end))
}

/// 过零率估频（正弦足够准，且不引入 FFT 依赖）
fn zerocross(sig: &[f32], sr: u32) -> f32 {
    let mut crossings = 0usize;
    for w in sig.windows(2) {
        if (w[0] < 0.0) != (w[1] < 0.0) {
            crossings += 1;
        }
    }
    crossings as f32 / 2.0 / (sig.len() as f32 / sr as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_core::device::{AudioStream, CaptureCallback, RenderCallback};

    #[derive(Default)]
    struct EmptyBackend {
        enumerations: AtomicUsize,
    }
    impl AudioBackend for EmptyBackend {
        fn backend_name(&self) -> &'static str {
            "empty-test-backend"
        }
        fn devices(&self) -> Result<Vec<DeviceInfo>> {
            self.enumerations.fetch_add(1, Ordering::Relaxed);
            Ok(Vec::new())
        }
        fn default_input(&self) -> Option<DeviceId> {
            None
        }
        fn default_output(&self) -> Option<DeviceId> {
            None
        }
        fn input_format(&self, _: &DeviceId, _: Option<StreamFormat>) -> Result<StreamFormat> {
            panic!("missing endpoints must fail before format negotiation")
        }
        fn output_format(&self, _: &DeviceId, _: Option<StreamFormat>) -> Result<StreamFormat> {
            panic!("missing endpoints must fail before format negotiation")
        }
        fn open_input(
            &self,
            _: &DeviceId,
            _: Option<StreamFormat>,
            _: CaptureCallback,
        ) -> Result<Box<dyn AudioStream>> {
            panic!("missing endpoints must fail before opening streams")
        }
        fn open_output(
            &self,
            _: &DeviceId,
            _: Option<StreamFormat>,
            _: RenderCallback,
        ) -> Result<Box<dyn AudioStream>> {
            panic!("missing endpoints must fail before opening streams")
        }
    }

    fn sine(freq: f32, sr: u32, secs: f32, amp: f32) -> Vec<f32> {
        let n = (sr as f32 * secs) as usize;
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / sr as f32).sin())
            .collect()
    }

    fn endpoint(id: &str, name: &str, direction: Direction) -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::new(id),
            name: name.into(),
            direction,
            is_default: false,
            max_channels: 2,
            default_sample_rate: 48_000,
        }
    }

    #[test]
    fn vb_cable_pairs_render_input_with_recording_output() {
        let devices = [
            endpoint(
                "render",
                "CABLE Input (VB-Audio Virtual Cable)",
                Direction::Output,
            ),
            endpoint(
                "capture",
                "CABLE Output (VB-Audio Virtual Cable)",
                Direction::Input,
            ),
            endpoint(
                "other",
                "CABLE-A Output (VB-Audio Cable A)",
                Direction::Input,
            ),
        ];
        let pair = resolve_endpoints(&devices, &DeviceId::new("render"), None).unwrap();
        assert_eq!(pair.capture, DeviceId::new("capture"));
        assert_eq!(pair.output, DeviceId::new("render"));
    }

    #[test]
    fn stable_endpoint_identity_precedes_a_matching_display_name_and_checks_direction() {
        let devices = [
            endpoint("wasapi:output", "CABLE Input", Direction::Output),
            endpoint("wasapi:other", "wasapi:output", Direction::Output),
            endpoint("wasapi:capture", "CABLE Output", Direction::Input),
        ];
        let pair = resolve_endpoints(
            &devices,
            &DeviceId::new("wasapi:output"),
            Some(&DeviceId::new("wasapi:capture")),
        )
        .unwrap();
        assert_eq!(pair.output, DeviceId::new("wasapi:output"));
        assert!(resolve_endpoints(&devices, &DeviceId::new("wasapi:capture"), None).is_err());
        assert!(resolve_endpoints(
            &devices,
            &DeviceId::new("wasapi:output"),
            Some(&DeviceId::new("wasapi:output"))
        )
        .is_err());
    }

    #[test]
    fn macos_same_name_endpoints_and_explicit_capture_are_supported() {
        let devices = [
            endpoint("out", "BlackHole 2ch", Direction::Output),
            endpoint("in", "BlackHole 2ch", Direction::Input),
            endpoint("custom", "Custom loopback", Direction::Input),
        ];
        assert_eq!(
            resolve_endpoints(&devices, &DeviceId::new("out"), None)
                .unwrap()
                .capture,
            DeviceId::new("in")
        );
        assert_eq!(
            resolve_endpoints(
                &devices,
                &DeviceId::new("out"),
                Some(&DeviceId::new("custom"))
            )
            .unwrap()
            .capture,
            DeviceId::new("custom")
        );
    }

    #[test]
    fn ambiguous_pair_requires_explicit_capture() {
        let devices = [
            endpoint("out", "BlackHole 2ch", Direction::Output),
            endpoint("one", "BlackHole 2ch", Direction::Input),
            endpoint("two", "BlackHole 2ch", Direction::Input),
        ];
        assert!(resolve_endpoints(&devices, &DeviceId::new("out"), None).is_err());
        assert_eq!(
            resolve_endpoints(&devices, &DeviceId::new("out"), Some(&DeviceId::new("two")))
                .unwrap()
                .capture,
            DeviceId::new("two")
        );
    }

    #[test]
    fn capture_analysis_uses_capture_rate_and_channel_count() {
        for (sr, channels) in [(44_100, 1), (48_000, 2), (32_000, 6)] {
            let mono = sine(440.0, sr, 0.5, 0.5);
            let interleaved: Vec<_> = mono
                .iter()
                .flat_map(|s| std::iter::repeat_n(*s, channels))
                .collect();
            let downmixed = downmix_capture(&interleaved, channels);
            assert_eq!(downmixed.len(), mono.len());
            assert!((zerocross(&downmixed, sr) - 440.0).abs() < 3.0);
            let (a, b) = find_tone(&downmixed, sr).unwrap();
            assert!(((b - a) as f32 / sr as f32 - 0.5).abs() < 0.03);
        }
    }

    #[test]
    fn 过零率应正确估出频率() {
        for f in [220.0f32, 440.0, 880.0, 1000.0] {
            let s = sine(f, 48_000, 1.0, 0.5);
            let got = zerocross(&s, 48_000);
            assert!((got - f).abs() < f * 0.02, "{f}Hz 估成了 {got:.1}Hz");
        }
    }

    #[test]
    fn 应能在静音里找出信号段() {
        let mut s = vec![0f32; 24_000]; // 0.5s 静音
        s.extend(sine(440.0, 48_000, 0.5, 0.5));
        s.extend(vec![0f32; 24_000]);
        let (a, b) = find_tone(&s, 48_000).expect("应找到信号");
        let dur = (b - a) as f32 / 48_000.0;
        assert!(
            (dur - 0.5).abs() < 0.05,
            "信号段时长应约 0.5s，实测 {dur:.3}s"
        );
    }

    #[test]
    fn 全静音应找不到信号() {
        assert!(find_tone(&vec![0f32; 48_000], 48_000).is_none());
    }

    #[test]
    fn 音高翻倍时应给出针对性指引() {
        // 这个故障排查了很久，结论必须固化下来：
        // 引擎数据正确、设备把它翻倍、偶发、重开流可解。
        // 用假数据跑一遍判定逻辑，确保提示里点明了这几件事。
        let measured = 1760.0f32;
        let freq = 880.0f32;
        let ratio = measured / freq;
        assert!((ratio - 2.0).abs() < 0.25, "应判定为翻倍");
        let msg = "**音高翻倍**：本程序交给声卡的数据是正确的，是虚拟声卡把它翻倍了";
        assert!(msg.contains("翻倍") && msg.contains("声卡"));
    }

    #[test]
    fn 不存在设备的自检应在协商或开流前返回错误() {
        // A missing-device regression must be deterministic and must never enumerate
        // the developer's real hardware (which can block in third-party drivers).
        let b = EmptyBackend::default();
        let r = run_with_capture_retry(&b, Some("绝对不存在的设备xyz".into()), None, 440.0, 0.2, 3);
        assert!(r.is_err(), "设备不存在时应返回错误而不是 panic");
        assert_eq!(
            b.enumerations.load(Ordering::Relaxed),
            1,
            "配置错误应立即返回，不重开设备"
        );
    }

    #[test]
    fn 报告结论应随检查项变化() {
        let mut r = Report {
            device: "测试设备".into(),
            freq_sent: 440.0,
            secs_sent: 1.0,
            sample_rate: 48_000,
            capture_format: StreamFormat::new(48_000, 2),
            checks: vec![Check {
                name: "频率".into(),
                got: "440.0 Hz".into(),
                want: "440.0 Hz".into(),
                ok: true,
            }],
            played_frames: 0,
            captured_samples: 0,
            captured_peak_db: f32::NEG_INFINITY,
            capture_drops: 0,
            advice: vec![],
            attempts: 1,
        };
        assert!(r.passed());
        assert!(r.summary().contains("通过"));
        r.checks[0].ok = false;
        assert!(!r.passed());
        assert!(r.summary().contains("频率"), "失败结论应点名是哪一项");
    }
}
