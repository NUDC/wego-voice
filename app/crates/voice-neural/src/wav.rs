//! 最小 WAV 读写。
//!
//! 刻意**不依赖 `voice-audio`**：那个 crate 拖着 WASAPI 和整条实时链路，
//! 而伴生程序只需要把一段波形读进来、再写出去。
//! 伴生程序是按需下载的，它的体积也是要付的。

use anyhow::{bail, Context, Result};
use std::path::Path;

pub struct Mono {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

/// 读一个 WAV，降混成单声道 f32。
pub fn read(path: &Path) -> Result<Mono> {
    let b = std::fs::read(path).with_context(|| format!("读取失败：{}", path.display()))?;
    if b.len() < 44 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        bail!("不是 WAV 文件：{}", path.display());
    }
    let u16le = |at: usize| u16::from_le_bytes([b[at], b[at + 1]]);
    let u32le = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);

    let (mut fmt, mut data) = (None, None);
    let mut pos = 12usize;
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let size = u32le(pos + 4) as usize;
        let body = pos + 8;
        if id == b"fmt " && body + 16 <= b.len() {
            fmt = Some((u16le(body), u16le(body + 2), u32le(body + 4), u16le(body + 14)));
        } else if id == b"data" {
            data = Some((body, size.min(b.len().saturating_sub(body))));
        }
        if body + size > b.len() {
            break;
        }
        pos = body + size + (size & 1);
    }
    let (format, ch, rate, bits) = fmt.context("缺少 fmt chunk")?;
    let (off, len) = data.context("缺少 data chunk")?;
    let ch = ch.max(1) as usize;
    let bytes = (bits / 8).max(1) as usize;

    let frames = len / (bytes * ch);
    let mut out = Vec::with_capacity(frames);
    for f in 0..frames {
        let mut acc = 0.0f32;
        for c in 0..ch {
            let p = off + (f * ch + c) * bytes;
            acc += match (format, bits) {
                (1, 16) => i16::from_le_bytes([b[p], b[p + 1]]) as f32 / 32768.0,
                (1, 24) => (i32::from_le_bytes([0, b[p], b[p + 1], b[p + 2]]) >> 8) as f32
                    / 8_388_608.0,
                (3, 32) => f32::from_le_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]),
                _ => bail!("不支持的 WAV 格式：format={format} bits={bits}"),
            };
        }
        out.push(acc / ch as f32);
    }
    Ok(Mono { samples: out, sample_rate: rate })
}

/// 写 32-bit float 单声道 WAV。
pub fn write(path: &Path, x: &[f32], rate: u32) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).ok();
    }
    let bytes = (x.len() * 4) as u32;
    let mut v = Vec::with_capacity(44 + bytes as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + bytes).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&32u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&bytes.to_le_bytes());
    for s in x {
        v.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, &v).with_context(|| format!("写入失败：{}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_bit_exact() {
        let p = std::env::temp_dir().join("wego-neural-wav.wav");
        let src: Vec<f32> = (0..1000).map(|i| (i as f32 / 500.0 - 1.0) * 0.8).collect();
        write(&p, &src, 44_100).unwrap();
        let got = read(&p).unwrap();
        assert_eq!(got.sample_rate, 44_100);
        assert_eq!(got.samples, src, "写出去再读回来必须逐位一致");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn rejects_non_wav() {
        let p = std::env::temp_dir().join("wego-neural-notwav.bin");
        std::fs::write(&p, b"this is not a wav at all........").unwrap();
        assert!(read(&p).is_err());
        let _ = std::fs::remove_file(&p);
    }
}
