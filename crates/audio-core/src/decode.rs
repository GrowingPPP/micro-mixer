//! 音频文件解码 —— 统一解码为引擎内部格式。
//!
//! 内部格式约定：**f32 / 交错 / 立体声 / 目标采样率**。
//! 所有解码、声道映射、重采样都在**非实时线程**完成；
//! 实时线程只读取已经准备好的 PCM。

use anyhow::{anyhow, Context, Result};
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymErr;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::resample::LinearResampler;

/// 解码结果：交错立体声 f32，已重采样到目标采样率。
#[derive(Debug, Clone)]
pub struct DecodedAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub duration_secs: f32,
}

impl DecodedAudio {
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    pub fn peak(&self) -> f32 {
        self.samples.iter().fold(0f32, |m, s| m.max(s.abs()))
    }

    /// 峰值归一化到 `target_peak`（线性值）。原地修改。
    pub fn normalize_peak(&mut self, target_peak: f32) {
        let p = self.peak();
        if p > 1e-6 {
            let g = target_peak / p;
            for s in self.samples.iter_mut() {
                *s *= g;
            }
        }
    }
}

/// 解码任意受支持格式为内部格式。
///
/// 支持 mp3 / aac(m4a) / flac / ogg-vorbis / wav / alac。
pub fn decode_file(path: &Path, target_rate: u32) -> Result<DecodedAudio> {
    if target_rate == 0 {
        return Err(anyhow!("目标采样率必须大于零"));
    }
    let file =
        std::fs::File::open(path).with_context(|| format!("打不开文件: {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .with_context(|| format!("无法识别音频格式: {}", path.display()))?;

    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("文件里没有可解码的音轨"))?;
    let track_id = track.id;
    let src_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| anyhow!("音轨缺少采样率信息"))?;
    if src_rate == 0 {
        return Err(anyhow!("音轨采样率必须大于零"));
    }

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .context("找不到对应的解码器")?;

    let mut mono: Vec<f32> = Vec::new(); // 先收成单声道（人声/音效都按单声道安全处理）

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymErr::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymErr::ResetRequired) => break,
            Err(e) => return Err(anyhow!("读取数据包失败: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // 单个包解码失败不应中断整个文件
            Err(SymErr::DecodeError(_)) => continue,
            Err(SymErr::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(anyhow!("解码失败: {e}")),
        };

        let spec = *decoded.spec();
        let ch = spec.channels.count().max(1);
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        let interleaved = buf.samples();

        // 下混为单声道（等功率：除以声道数）
        let inv = 1.0 / ch as f32;
        for frame in interleaved.chunks_exact(ch) {
            mono.push(frame.iter().sum::<f32>() * inv);
        }
    }

    if mono.is_empty() {
        return Err(anyhow!("解码后没有任何音频数据"));
    }

    // 重采样到目标采样率（非实时线程，可用简单插值）
    let mono = if src_rate != target_rate {
        let ratio = target_rate as f64 / src_rate as f64;
        let mut rs = LinearResampler::new(ratio);
        let frames = (mono.len() as f64 * ratio).ceil() as usize;
        // 实时重采样需要等待下一块完成末样本插值；文件已结束，复制末样本补齐时长。
        let mut padded = mono;
        padded.push(*padded.last().expect("已检查非空"));
        let mut out = vec![0f32; frames + ratio.ceil() as usize + 8];
        let n = rs.process(&padded, &mut out);
        out.truncate(n.min(frames));
        out
    } else {
        mono
    };

    // 单声道 → 立体声（左右等幅，保证单声道下混安全）
    let mut samples = Vec::with_capacity(mono.len() * 2);
    for s in &mono {
        samples.push(*s);
        samples.push(*s);
    }

    let duration_secs = mono.len() as f32 / target_rate as f32;
    Ok(DecodedAudio {
        samples,
        sample_rate: target_rate,
        duration_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 低采样率文件解码应保持一秒时长和音高() {
        // 独立生成标准 PCM WAV，覆盖文件解码 → 重采样整个实际路径。
        let mut bytes = Vec::new();
        let frames = 8000u32;
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + frames * 2).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8000u32.to_le_bytes());
        bytes.extend_from_slice(&16000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(frames * 2).to_le_bytes());
        for i in 0..frames {
            let s = (std::f32::consts::TAU * 440.0 * i as f32 / 8000.0).sin();
            bytes.extend_from_slice(&((s * 16000.0) as i16).to_le_bytes());
        }
        let path =
            std::env::temp_dir().join(format!("mixer-decode-low-rate-{}.wav", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        let decoded = decode_file(&path, 48_000);
        let _ = std::fs::remove_file(path);
        let audio = decoded.unwrap();
        assert_eq!(audio.frames(), 48_000, "一秒文件不得缩短");
        let mono: Vec<f32> = audio
            .samples
            .as_chunks::<2>()
            .0
            .iter()
            .map(|f| f[0])
            .collect();
        let crossings = mono
            .windows(2)
            .filter(|w| w[0] <= 0.0 && w[1] > 0.0)
            .count();
        assert!(
            crossings.abs_diff(440) <= 1,
            "一秒内应约 440 个周期，实际 {crossings}"
        );
    }

    #[test]
    fn 零目标采样率应返回错误() {
        let error = decode_file(Path::new("unused.wav"), 0).unwrap_err();
        assert!(error.to_string().contains("采样率"));
    }

    #[test]
    fn 峰值归一化正确() {
        let mut a = DecodedAudio {
            samples: vec![0.25, -0.25, 0.5, -0.5],
            sample_rate: 48_000,
            duration_secs: 0.0,
        };
        a.normalize_peak(0.9);
        assert!((a.peak() - 0.9).abs() < 1e-6);
    }

    #[test]
    fn 全零信号归一化不崩溃() {
        let mut a = DecodedAudio {
            samples: vec![0.0; 10],
            sample_rate: 48_000,
            duration_secs: 0.0,
        };
        a.normalize_peak(0.9);
        assert_eq!(a.peak(), 0.0);
    }

    #[test]
    fn 帧数计算正确() {
        let a = DecodedAudio {
            samples: vec![0.0; 960],
            sample_rate: 48_000,
            duration_secs: 0.01,
        };
        assert_eq!(a.frames(), 480);
    }
}
