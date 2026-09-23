//! 极简 WAV 读取。
//!
//! 只为一件事服务：把用户拖进来的参考音频读成单声道 f32。
//!
//! # 为什么自己写
//!
//! 需求很窄（读进内存、降混、几种常见位深），而 `voice-audio` 每多一个依赖，
//! 将来做 CLAP 插件时就多一份审计。写入侧（`recorder.rs`）同理。
//!
//! # 刻意不做重采样
//!
//! 声线分析（`voice_core::timbre`）的栅格是按 **Hz** 定义的，
//! 44.1k 和 48k 的素材可以直接比。重采样只会引入不必要的失真，
//! 还得为它写一套滤波器。所以这里原样返回，采样率随数据一起交出去。

use std::path::Path;

use anyhow::{bail, Context, Result};

/// 读进来的音频。
#[derive(Debug)]
pub struct Audio {
    /// 单声道 f32。多声道会被降混。
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    /// 原始声道数，UI 上如实显示。
    pub channels: u16,
}

impl Audio {
    pub fn duration_secs(&self) -> f32 {
        self.samples.len() as f32 / self.sample_rate.max(1) as f32
    }
}

/// 参考音频允许读入的最大时长。
///
/// 参考音频用不着几十分钟，而误拖一个大文件进来会直接吃光内存。
/// 超长的只取前面这一段 —— 声线是稳定属性，不需要整首。
///
/// ⚠️ **这个上限只适用于参考音频**。用户自己的录音必须整条读
/// （见 `read`），否则超过两分钟的歌会被悄悄砍掉后半段。
pub const MAX_SECS: f32 = 120.0;

/// 读一个 WAV 文件，**不截断**。
///
/// 用户自己的录音走这条：一首歌五分钟很正常，
/// 截断的后果是离线处理产出的文件比原录音短一截 ——
/// 而且是**静默**发生的，用户只会以为软件坏了。
pub fn read(path: impl AsRef<Path>) -> Result<Audio> {
    read_limited(path, None)
}

/// 读一个 WAV 文件，最多 `max_secs` 秒。
///
/// 外来素材（用户拖进来的参考音频）走这条：那是我们无法预期大小的输入，
/// 而截断对"算声线"这件事没有损害。
pub fn read_capped(path: impl AsRef<Path>, max_secs: f32) -> Result<Audio> {
    read_limited(path, Some(max_secs))
}

fn read_limited(path: impl AsRef<Path>, max_secs: Option<f32>) -> Result<Audio> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)
        .with_context(|| format!("读取失败：{}", path.display()))?;
    parse_limited(&bytes, max_secs).with_context(|| format!("解析失败：{}", path.display()))
}

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// 只读文件头，拿到采样率与时长。
///
/// 列目录时用：为了显示一行"3.2 秒"而把整个文件读进内存是不划算的，
/// 十几条录音就是几百 MB。
pub fn probe(path: impl AsRef<Path>) -> Result<(u32, f32)> {
    use std::io::Read;
    let path = path.as_ref();
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("打开失败：{}", path.display()))?;
    let total = f.metadata().map(|m| m.len()).unwrap_or(0);

    let mut head = vec![0u8; 4096.min(total as usize).max(12)];
    let n = f.read(&mut head).context("读取文件头失败")?;
    head.truncate(n);

    if head.len() < 12 || &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        bail!("不是 WAV 文件");
    }

    let mut pos = 12usize;
    while pos + 8 <= head.len() {
        let id = &head[pos..pos + 4];
        let size = u32le(&head, pos + 4) as usize;
        let body = pos + 8;
        if id == b"fmt " && size >= 16 && body + 16 <= head.len() {
            let ch = u16le(&head, body + 2).max(1) as u64;
            let rate = u32le(&head, body + 4).max(1);
            let bits = u16le(&head, body + 14).max(8) as u64;
            // 用文件总长减去头部估时长：不必找到 data chunk 的确切长度字段，
            // 而且对长度字段写错的文件（录制中途崩溃）反而更准
            let frame = ch * bits / 8;
            let data = total.saturating_sub(body as u64 + size as u64 + 8);
            return Ok((rate, data as f32 / frame.max(1) as f32 / rate as f32));
        }
        if body + size > head.len() {
            break;
        }
        pos = body + size + (size & 1);
    }
    bail!("找不到 fmt chunk")
}

/// 解析 WAV 字节流。
///
/// 按 chunk 遍历而不是假定固定偏移：真实世界的 WAV 常常在 `fmt ` 和 `data`
/// 之间夹着 `LIST`、`fact`、`bext` 等 chunk（DAW 导出的尤其如此），
/// 写死 44 字节偏移的读法会在这些文件上读出噪声。
pub fn parse(b: &[u8]) -> Result<Audio> {
    parse_limited(b, None)
}

/// 解析 WAV 字节流，最多收下 `max_secs` 秒。`None` = 整条收下。
pub fn parse_limited(b: &[u8], max_secs: Option<f32>) -> Result<Audio> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        bail!("不是 WAV 文件（RIFF/WAVE 头不匹配）");
    }

    let mut pos = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // (format, channels, rate, bits)
    let mut data: Option<(usize, usize)> = None;

    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let size = u32le(b, pos + 4) as usize;
        let body = pos + 8;
        if body + size > b.len() {
            // 长度字段超出文件：录制中途崩溃的文件常这样。
            // 按实际可用长度收下，而不是整个拒掉 —— 用户的素材更重要。
            if id == b"data" {
                data = Some((body, b.len() - body));
            }
            break;
        }

        match id {
            b"fmt " => {
                if size < 16 {
                    bail!("fmt chunk 太短");
                }
                let mut format = u16le(b, body);
                let channels = u16le(b, body + 2);
                let rate = u32le(b, body + 4);
                let bits = u16le(b, body + 14);
                // WAVE_FORMAT_EXTENSIBLE：真正的格式在 SubFormat 的头两字节里
                if format == 0xFFFE && size >= 40 {
                    format = u16le(b, body + 24);
                }
                fmt = Some((format, channels, rate, bits));
            }
            b"data" => data = Some((body, size)),
            _ => {}
        }

        // chunk 按偶数字节对齐
        pos = body + size + (size & 1);
    }

    let (format, channels, rate, bits) = fmt.context("缺少 fmt chunk")?;
    let (off, len) = data.context("缺少 data chunk")?;
    if channels == 0 {
        bail!("声道数为 0");
    }
    if rate == 0 {
        bail!("采样率为 0");
    }

    let bytes_per = match (format, bits) {
        (1, 16) | (1, 24) | (1, 32) => (bits / 8) as usize,
        (3, 32) => 4,
        (3, 64) => 8,
        _ => bail!("不支持的格式：format={format} bits={bits}（支持 PCM 16/24/32 与 float 32/64）"),
    };

    let frame_bytes = bytes_per * channels as usize;
    let mut frames = len / frame_bytes.max(1);
    if let Some(cap) = max_secs {
        frames = frames.min((cap * rate as f32) as usize);
    }

    let mut out = Vec::with_capacity(frames);
    let inv_ch = 1.0 / channels as f32;
    for f in 0..frames {
        let base = off + f * frame_bytes;
        let mut acc = 0.0f32;
        for c in 0..channels as usize {
            let p = base + c * bytes_per;
            acc += match (format, bits) {
                (1, 16) => i16::from_le_bytes([b[p], b[p + 1]]) as f32 / 32768.0,
                (1, 24) => {
                    // 24-bit 小端有符号：补上第 4 字节做符号扩展
                    let v = i32::from_le_bytes([0, b[p], b[p + 1], b[p + 2]]) >> 8;
                    v as f32 / 8_388_608.0
                }
                (1, 32) => {
                    i32::from_le_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]) as f32 / 2_147_483_648.0
                }
                (3, 32) => f32::from_le_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]),
                (3, 64) => f64::from_le_bytes([
                    b[p], b[p + 1], b[p + 2], b[p + 3], b[p + 4], b[p + 5], b[p + 6], b[p + 7],
                ]) as f32,
                _ => unreachable!("格式已在上面校验过"),
            };
        }
        // 降混取平均而不是求和：求和会让立体声素材削顶
        out.push(acc * inv_ch);
    }

    Ok(Audio {
        samples: out,
        sample_rate: rate,
        channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 拼一个 WAV 字节流。`extra` 为 fmt 与 data 之间插入的额外 chunk。
    fn build(format: u16, bits: u16, ch: u16, rate: u32, data: &[u8], extra: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(b"WAVE");

        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&format.to_le_bytes());
        v.extend_from_slice(&ch.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * ch as u32 * bits as u32 / 8).to_le_bytes());
        v.extend_from_slice(&(ch * bits / 8).to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());

        v.extend_from_slice(extra);

        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);

        let total = (v.len() - 8) as u32;
        v[4..8].copy_from_slice(&total.to_le_bytes());
        v
    }

    #[test]
    fn reads_float32_mono() {
        let src = [0.0f32, 0.5, -0.25, 1.0];
        let mut data = Vec::new();
        for s in src {
            data.extend_from_slice(&s.to_le_bytes());
        }
        let a = parse(&build(3, 32, 1, 48_000, &data, &[])).unwrap();
        assert_eq!(a.sample_rate, 48_000);
        assert_eq!(a.channels, 1);
        assert_eq!(a.samples, src);
    }

    #[test]
    fn reads_pcm16_and_scales() {
        let data: Vec<u8> = [0i16, 16384, -16384, 32767]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let a = parse(&build(1, 16, 1, 44_100, &data, &[])).unwrap();
        assert_eq!(a.sample_rate, 44_100);
        assert!((a.samples[1] - 0.5).abs() < 1e-6);
        assert!((a.samples[2] + 0.5).abs() < 1e-6);
        assert!(a.samples.iter().all(|s| s.abs() <= 1.0));
    }

    /// 24-bit 的符号扩展最容易写错：漏了移位就会把负值读成很大的正值。
    #[test]
    fn reads_pcm24_with_correct_sign() {
        // -8388608（最负）、0、+8388607（最正）
        let vals: [i32; 3] = [-8_388_608, 0, 8_388_607];
        let mut data = Vec::new();
        for v in vals {
            let b = v.to_le_bytes();
            data.extend_from_slice(&b[0..3]);
        }
        let a = parse(&build(1, 24, 1, 48_000, &data, &[])).unwrap();
        assert!((a.samples[0] + 1.0).abs() < 1e-5, "最负值读成了 {}", a.samples[0]);
        assert!(a.samples[1].abs() < 1e-6);
        assert!((a.samples[2] - 1.0).abs() < 1e-5);
    }

    /// 立体声降混取**平均**。求和的话满幅立体声会削顶。
    #[test]
    fn downmixes_stereo_by_averaging() {
        let data: Vec<u8> = [1.0f32, 0.0, 1.0, 1.0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let a = parse(&build(3, 32, 2, 48_000, &data, &[])).unwrap();
        assert_eq!(a.channels, 2);
        assert_eq!(a.samples.len(), 2);
        assert!((a.samples[0] - 0.5).abs() < 1e-6, "降混应取平均");
        assert!((a.samples[1] - 1.0).abs() < 1e-6);
    }

    /// DAW 导出的 WAV 常在 fmt 与 data 之间夹 LIST/fact 等 chunk。
    /// 写死 44 字节偏移的读法会在这些文件上读出噪声。
    #[test]
    fn skips_unknown_chunks_between_fmt_and_data() {
        let mut extra = Vec::new();
        extra.extend_from_slice(b"LIST");
        extra.extend_from_slice(&10u32.to_le_bytes());
        extra.extend_from_slice(b"INFOhello");
        extra.push(0); // 奇数长度要补齐对齐字节

        let data: Vec<u8> = [0.25f32, -0.75]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let a = parse(&build(3, 32, 1, 48_000, &data, &extra)).unwrap();
        assert_eq!(a.samples.len(), 2);
        assert!((a.samples[0] - 0.25).abs() < 1e-6, "夹了 LIST chunk 就读错了");
    }

    /// data 长度字段大于实际字节数 —— 录制中途崩溃的文件常这样。
    /// 应当按实际长度收下，而不是整个拒掉：用户的素材比格式洁癖重要。
    #[test]
    fn tolerates_truncated_data_chunk() {
        let data: Vec<u8> = [0.5f32, 0.5].iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut w = build(3, 32, 1, 48_000, &data, &[]);
        // 把 data 的长度字段改大
        let n = w.len();
        w[n - 8 - 4..n - 8].copy_from_slice(&9999u32.to_le_bytes());
        let a = parse(&w).unwrap();
        assert_eq!(a.samples.len(), 2);
    }

    #[test]
    fn rejects_non_wav() {
        assert!(parse(b"not a wav file at all").is_err());
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn rejects_unsupported_bit_depth() {
        let e = parse(&build(1, 8, 1, 48_000, &[0, 0, 0, 0], &[])).unwrap_err();
        assert!(e.to_string().contains("不支持"));
    }

    /// 给了上限就只取前面一段，避免误拖一个大文件把内存吃光。
    #[test]
    fn caps_overlong_input_when_asked() {
        let rate = 8_000u32;
        let frames = (MAX_SECS * rate as f32) as usize + 5_000;
        let data: Vec<u8> = (0..frames).flat_map(|_| 0.1f32.to_le_bytes()).collect();
        let a = parse_limited(&build(3, 32, 1, rate, &data, &[]), Some(MAX_SECS)).unwrap();
        assert_eq!(a.samples.len(), (MAX_SECS * rate as f32) as usize);
        assert!((a.duration_secs() - MAX_SECS).abs() < 0.01);
    }

    /// ⚠️ 回归测试：**默认不截断**。
    ///
    /// 这个上限原本是给"用户拖进来的参考音频"设的，但 `job.rs` 也用同一个
    /// `read` 读用户自己的录音 —— 于是超过两分钟的歌被悄悄砍掉后半段，
    /// 离线产物比原录音短一截，而且没有任何提示。
    ///
    /// 一首歌五分钟很正常。默认不截断，要截断的地方自己说。
    #[test]
    fn does_not_cap_by_default() {
        let rate = 8_000u32;
        let frames = (MAX_SECS * rate as f32) as usize + 5_000;
        let data: Vec<u8> = (0..frames).flat_map(|_| 0.1f32.to_le_bytes()).collect();
        let a = parse(&build(3, 32, 1, rate, &data, &[])).unwrap();
        assert_eq!(a.samples.len(), frames, "默认读入被截断了");
    }
}

/// 写一个 32-bit float 单声道 WAV。
///
/// 离线产物要落盘。和 `recorder.rs` 里那个流式写入器不同 ——
/// 这里数据已经全在内存里，一次写完即可，不需要回填长度字段。
pub fn write(path: impl AsRef<Path>, samples: &[f32], sample_rate: u32) -> Result<()> {
    use std::io::Write;
    let path = path.as_ref();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).ok();
    }
    let data_bytes = (samples.len() * 4) as u32;
    let mut v = Vec::with_capacity(44 + data_bytes as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    v.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    v.extend_from_slice(&sample_rate.to_le_bytes());
    v.extend_from_slice(&(sample_rate * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&32u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_bytes.to_le_bytes());
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::File::create(path)
        .and_then(|mut f| f.write_all(&v))
        .with_context(|| format!("写入失败：{}", path.display()))
}

#[cfg(test)]
mod write_tests {
    use super::*;

    /// 写出去再读回来必须逐位一致 —— 离线产物是后续处理的输入，
    /// 这里有任何损耗都会一路传下去。
    #[test]
    fn write_read_roundtrip_is_bit_exact() {
        let path = std::env::temp_dir().join("wego-wav-rt.wav");
        let src: Vec<f32> = (0..1000).map(|i| (i as f32 / 500.0 - 1.0) * 0.8).collect();
        write(&path, &src, 48_000).unwrap();
        let back = read(&path).unwrap();
        assert_eq!(back.sample_rate, 48_000);
        assert_eq!(back.channels, 1);
        assert_eq!(back.samples, src);
        let _ = std::fs::remove_file(&path);
    }
}
