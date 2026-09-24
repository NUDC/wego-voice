//! 特征对齐：把 f0、音量、内容特征放到**解码器的时间栅格**上。
//!
//! # 栅格是 block_size，不是编码器的帧率
//!
//! ⚠️ 这一条我一开始搞反了，值得写清楚。
//!
//! 内容编码器输出 50 fps（HuBERT 前端 320 倍下采样 @16 kHz），
//! 很自然会以为它是基准。但**解码器有它自己的栅格**：`block_size`
//! （DDSP-SVC 默认 512 @44.1 kHz = 86.13 fps），因为它每帧要合成
//! `block_size` 个样本。
//!
//! 所以基准是 block 栅格，三路都往它上面靠：
//!
//! | 特征 | 原生步进 | 怎么上栅格 |
//! |---|---|---|
//! | 内容 | 320 @16 kHz（50 fps） | **最近邻**取（见下） |
//! | f0 | 128 @48 kHz（375 fps） | 对数域插值 |
//! | 音量 | 直接从波形算 | 按 block 窗算 |
//!
//! # 内容特征用最近邻，不是插值
//!
//! 参考实现（`Units_Encoder.encode`）用的是 `gather(round(ratio · t))`。
//! 这是个**刻意的选择**而不是省事：内容特征是高维语义向量，
//! 两帧之间线性插值得到的是一个**两边都不是的中间态** ——
//! 就像把"啊"和"喔"的向量平均起来，得不到任何一个真实的音。
//!
//! 音高可以插值（它是连续量），内容不行。
//!
//! # 音量是线性 RMS，不是 dB
//!
//! 参考实现 `Volume_Extractor`：平方 → 反射填充 → 按 hop 窗取均值 → 开方。
//! 网络那边是 `volume_embed = Linear(1, 256)` 直接吃这个线性值。
//!
//! 我第一版写成了 80 ms 窗的 dBFS —— 数值范围和刻度都不一样。
//! 要用人家的预训练权重，这里就必须一模一样。

use anyhow::{bail, Result};

use crate::{ENCODER_HOP, ENCODER_RATE};

/// 一段音频的内容特征（「唱的是什么」，不含「谁在唱」）。
pub struct Features {
    /// 行优先：`data[t * dim + d]`。
    pub data: Vec<f32>,
    pub frames: usize,
    pub dim: usize,
}

impl Features {
    pub fn frame(&self, t: usize) -> &[f32] {
        &self.data[t * self.dim..(t + 1) * self.dim]
    }
}

/// 对齐之后的逐帧特征。四路等长 —— 这是本模块唯一的产出承诺。
pub struct Aligned {
    pub frames: usize,
    /// 解码器栅格的步进（样本）。
    pub block_size: usize,
    /// Hz。`voiced[t] == false` 时为 0。
    pub f0: Vec<f32>,
    pub voiced: Vec<bool>,
    /// 线性 RMS（不是 dB）—— 与参考实现一致。
    pub volume: Vec<f32>,
    /// 行优先内容特征，`frames * dim`。
    pub content: Vec<f32>,
    pub dim: usize,
}

impl Aligned {
    pub fn unit(&self, t: usize) -> &[f32] {
        &self.content[t * self.dim..(t + 1) * self.dim]
    }

    /// 音量的 dB 视图，只给界面用。训练与推理一律用线性值。
    pub fn volume_db(&self, t: usize) -> f32 {
        let v = self.volume[t];
        if v <= 1e-5 {
            -100.0
        } else {
            20.0 * v.log10()
        }
    }
}

/// 把 f0、音量、内容对齐到解码器的 block 栅格上。
pub fn align(
    track: &voice_core::PitchTrack,
    samples: &[f32],
    rate: u32,
    content: &Features,
    block_size: usize,
) -> Result<Aligned> {
    if content.frames == 0 {
        bail!("内容特征是空的");
    }
    if block_size == 0 {
        bail!("block_size 不能为 0");
    }
    if (track.sample_rate - rate as f32).abs() > 1.0 {
        bail!(
            "音高轨的采样率是 {} Hz，波形是 {rate} Hz —— 两者必须同源",
            track.sample_rate
        );
    }

    // 与参考实现一致：`n_frames = len // hop + 1`
    let frames = samples.len() / block_size + 1;

    // 内容帧 → block 帧 的比例。
    // `(block/rate) / (320/16000)`：两边都换算成秒再相除。
    let ratio =
        (block_size as f64 / rate as f64) / (ENCODER_HOP as f64 / ENCODER_RATE as f64);

    let mut f0 = Vec::with_capacity(frames);
    let mut voiced = Vec::with_capacity(frames);
    let mut volume = Vec::with_capacity(frames);
    let mut out = Vec::with_capacity(frames * content.dim);

    for t in 0..frames {
        let pos = t * block_size;
        let (hz, v) = sample_pitch(track, pos);
        f0.push(hz);
        voiced.push(v);
        volume.push(frame_volume(samples, t, block_size));

        // 最近邻，且钳在最后一帧上 —— block 栅格通常比内容栅格长一点
        let idx = ((ratio * t as f64).round() as usize).min(content.frames - 1);
        out.extend_from_slice(content.frame(idx));
    }

    Ok(Aligned {
        frames,
        block_size,
        f0,
        voiced,
        volume,
        content: out,
        dim: content.dim,
    })
}

/// 第 `t` 帧的线性 RMS 音量。
///
/// 与参考实现逐步对应：平方 → 反射填充 hop/2 → 取 hop 长的均值 → 开方。
/// 窗口正好**以 `t * hop` 为中心**。
fn frame_volume(x: &[f32], t: usize, hop: usize) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    let left = hop / 2;
    let mut acc = 0.0f64;
    for j in 0..hop {
        // 填充后的下标 j + t*hop 对应原始下标
        let orig = (t * hop + j) as isize - left as isize;
        let i = reflect(orig, x.len());
        acc += (x[i] as f64).powi(2);
    }
    ((acc / hop as f64).sqrt()) as f32
}

/// 反射索引（不重复边界值），对应 numpy 的 `mode='reflect'`。
///
/// `[a,b,c]` 左填 1 得到 `[b,a,b,c]` —— 边界值 `a` 只出现一次。
/// 用重复边界（`edge`）的话，静音起始处会多出一段直流，
/// 表现为第一帧音量偏高。
fn reflect(i: isize, n: usize) -> usize {
    if n == 1 {
        return 0;
    }
    let n = n as isize;
    let period = 2 * (n - 1);
    let mut k = i.rem_euclid(period);
    if k >= n {
        k = period - k;
    }
    k as usize
}

/// 在样本位置 `pos` 处取音高。
///
/// **在对数域插值，而且绝不跨过清音段插值。**
///
/// 线性插值 Hz 在大音程上就已经偏了（220 与 440 的中点是 311 不是 330）；
/// 而跨清音段插值更糟 —— 一边是 0、一边是 220，插出来的 110 是个
/// **凭空捏造的低八度**，正好落在换气的位置上。
fn sample_pitch(track: &voice_core::PitchTrack, pos: usize) -> (f32, bool) {
    let hop = track.hop.max(1);
    let x = pos as f32 / hop as f32;
    let i = x.floor() as usize;
    let frac = x - i as f32;

    let get = |k: usize| -> Option<f32> {
        track
            .frames
            .get(k)
            .filter(|f| f.voiced && f.f0 > 0.0)
            .map(|f| f.f0)
    };

    match (get(i), get(i + 1)) {
        (Some(a), Some(b)) => {
            // 对数域线性插值 = Hz 域的几何插值
            let v = (a.ln() * (1.0 - frac) + b.ln() * frac).exp();
            (v, true)
        }
        // 只有一侧有声：**用那一侧的值，不插值**。
        // 插值会把清音那一侧的 0 掺进来。
        (Some(a), None) => (a, true),
        (None, Some(b)) if frac > 0.5 => (b, true),
        _ => (0.0, false),
    }
}

impl std::fmt::Debug for Aligned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let v = self.voiced.iter().filter(|b| **b).count();
        write!(
            f,
            "Aligned {{ {} 帧 @ block {}, {v} 帧有声, 内容 {}×{} }}",
            self.frames, self.block_size, self.frames, self.dim
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const SR: u32 = 44_100;
    const BS: usize = 512;

    /// 造一段：前 `silence_secs` 秒静音，之后是 `hz` 的类人声。
    fn clip(silence_secs: f32, hz: f32, total_secs: f32, level: f32) -> Vec<f32> {
        let n = (total_secs * SR as f32) as usize;
        let start = (silence_secs * SR as f32) as usize;
        let mut phase = 0.0f32;
        (0..n)
            .map(|i| {
                if i < start {
                    return 0.0;
                }
                phase += TAU * hz / SR as f32;
                // 谐波堆，不是纯正弦 —— YIN 对纯正弦太容易
                (1..=12).map(|k| (phase * k as f32).sin() / k as f32).sum::<f32>() * level
            })
            .collect()
    }

    /// 造一个每帧都不一样的内容特征，方便查"取到了第几帧"。
    fn ramp_content(frames: usize, dim: usize) -> Features {
        let data = (0..frames * dim).map(|i| (i / dim) as f32).collect();
        Features { data, frames, dim }
    }

    #[test]
    fn frame_count_matches_the_reference_formula() {
        // n_frames = len // hop + 1
        let x = vec![0.0f32; BS * 10 + 7];
        let track = voice_core::track_pitch(&x, SR as f32, |_| true).unwrap();
        let c = ramp_content(200, 2);
        let a = align(&track, &x, SR, &c, BS).unwrap();
        assert_eq!(a.frames, 11);
    }

    /// ⚠️ 内容特征必须**最近邻**取，不是插值。
    ///
    /// 内容是高维语义向量，两帧之间插值得到的是一个两边都不是的中间态 ——
    /// 像把"啊"和"喔"的向量平均起来。这里用 ramp 内容查取到的下标：
    /// 每个值必须是**整数**（说明来自某一帧原样），而不是小数。
    #[test]
    fn units_are_gathered_not_interpolated() {
        let x = vec![0.0f32; BS * 40];
        let track = voice_core::track_pitch(&x, SR as f32, |_| true).unwrap();
        let c = ramp_content(60, 3);
        let a = align(&track, &x, SR, &c, BS).unwrap();

        for t in 0..a.frames {
            let u = a.unit(t);
            assert_eq!(u[0], u[0].round(), "第 {t} 帧内容被插值了：{}", u[0]);
            assert_eq!(u[0], u[1], "同一帧内三个维度应当来自同一源帧");
        }

        // 比例：block 11.6 ms / 编码器 20 ms ≈ 0.5805
        let ratio = (BS as f64 / SR as f64) / (ENCODER_HOP as f64 / ENCODER_RATE as f64);
        for t in [0usize, 1, 5, 17, 33] {
            let want = (ratio * t as f64).round() as f32;
            assert_eq!(a.unit(t)[0], want, "第 {t} 帧取错了源帧");
        }
    }

    /// 内容帧不够时钳在最后一帧，不越界、不 panic。
    #[test]
    fn short_content_is_clamped_not_out_of_bounds() {
        let x = vec![0.0f32; BS * 100];
        let track = voice_core::track_pitch(&x, SR as f32, |_| true).unwrap();
        let c = ramp_content(5, 2); // 远远不够
        let a = align(&track, &x, SR, &c, BS).unwrap();
        assert_eq!(a.frames, 101);
        assert_eq!(a.unit(100)[0], 4.0, "没有钳在最后一帧");
    }

    /// 四路必须等长。
    #[test]
    fn all_streams_have_the_same_length() {
        let x = clip(0.0, 220.0, 2.0, 0.3);
        let track = voice_core::track_pitch(&x, SR as f32, |_| true).unwrap();
        let c = ramp_content(200, 4);
        let a = align(&track, &x, SR, &c, BS).unwrap();
        assert_eq!(a.f0.len(), a.frames);
        assert_eq!(a.voiced.len(), a.frames);
        assert_eq!(a.volume.len(), a.frames);
        assert_eq!(a.content.len(), a.frames * a.dim);
    }

    /// ⚠️ 已知的事件必须落在已知的帧上。
    ///
    /// 前 1 秒静音。44.1 kHz / block 512 = 86.13 fps，所以边界在第 86 帧。
    /// 差几帧的错误在形状测试里完全看不出来。
    #[test]
    fn a_known_event_lands_on_the_known_frame() {
        let x = clip(1.0, 220.0, 2.0, 0.3);
        let track = voice_core::track_pitch(&x, SR as f32, |_| true).unwrap();
        let c = ramp_content(200, 2);
        let a = align(&track, &x, SR, &c, BS).unwrap();

        assert!(!a.voiced[40], "静音段第 40 帧被判成了有声");
        assert!(a.volume[40] < 1e-4, "静音段音量是 {}", a.volume[40]);

        assert!(a.voiced[140], "有声段第 140 帧被判成了清音");
        let cents = 1200.0 * (a.f0[140] / 220.0).log2();
        assert!(cents.abs() < 50.0, "第 140 帧音高 {} Hz（偏 {cents:.0} 音分）", a.f0[140]);

        // 音量窗只有一个 block 宽（11.6 ms），所以跃升很陡 —— 容差给 2 帧
        let expect = (SR as f32 / BS as f32).round() as i32; // 86
        let jump = (1..a.frames).find(|&t| a.volume[t] > 0.01).expect("整段没响起来");
        assert!(
            (jump as i32 - expect).abs() <= 2,
            "音量在第 {jump} 帧才起来，预期第 {expect} 帧附近"
        );
    }

    /// 音量是**线性 RMS**：增益 ×2，音量必须 ×2。
    ///
    /// 写成 dB 的话这条会变成 +6.02，而网络那边 `Linear(1,256)`
    /// 吃的是线性值 —— 刻度错了，预训练权重就全对不上。
    #[test]
    fn volume_is_linear_rms_not_db() {
        let x = clip(0.0, 220.0, 1.0, 0.2);
        let loud: Vec<f32> = x.iter().map(|v| v * 2.0).collect();
        let track = voice_core::track_pitch(&x, SR as f32, |_| true).unwrap();
        let c = ramp_content(100, 2);

        let a = align(&track, &x, SR, &c, BS).unwrap();
        let b = align(&track, &loud, SR, &c, BS).unwrap();

        for (t, (lo, hi)) in a.volume.iter().zip(&b.volume).enumerate().take(60).skip(10) {
            let r = hi / lo.max(1e-9);
            assert!((r - 2.0).abs() < 0.01, "第 {t} 帧增益 ×2，音量只涨了 {r:.3} 倍");
        }
    }

    /// 已知幅度的正弦：RMS 必须是 A/√2。
    #[test]
    fn volume_matches_the_textbook_value() {
        let n = BS * 40;
        let x: Vec<f32> = (0..n).map(|i| 0.5 * (TAU * 300.0 * i as f32 / SR as f32).sin()).collect();
        let track = voice_core::track_pitch(&x, SR as f32, |_| true).unwrap();
        let c = ramp_content(80, 2);
        let a = align(&track, &x, SR, &c, BS).unwrap();
        let want = 0.5 / 2f32.sqrt();
        for (t, v) in a.volume.iter().enumerate().take(35).skip(5) {
            assert!((v - want).abs() < 0.02, "第 {t} 帧 RMS = {v}，应当是 {want:.4}");
        }
    }

    /// 反射填充要和 numpy 的 `mode='reflect'` 一致（边界值不重复）。
    #[test]
    fn reflect_matches_numpy() {
        // [a,b,c,d] 下标 0..3
        assert_eq!(reflect(-1, 4), 1);
        assert_eq!(reflect(-2, 4), 2);
        assert_eq!(reflect(-3, 4), 3);
        assert_eq!(reflect(0, 4), 0);
        assert_eq!(reflect(3, 4), 3);
        assert_eq!(reflect(4, 4), 2);
        assert_eq!(reflect(5, 4), 1);
        assert_eq!(reflect(0, 1), 0);
    }

    /// 音高插值不许跨过清音段。
    #[test]
    fn pitch_is_never_interpolated_across_silence() {
        let x = clip(1.0, 220.0, 2.0, 0.3);
        let track = voice_core::track_pitch(&x, SR as f32, |_| true).unwrap();
        let c = ramp_content(200, 2);
        let a = align(&track, &x, SR, &c, BS).unwrap();

        for t in 0..a.frames {
            if a.voiced[t] {
                let cents = 1200.0 * (a.f0[t] / 220.0).log2();
                assert!(cents.abs() < 100.0, "第 {t} 帧有声却给出 {} Hz", a.f0[t]);
            } else {
                assert_eq!(a.f0[t], 0.0, "第 {t} 帧判为清音，f0 却是 {}", a.f0[t]);
            }
        }
    }

    #[test]
    fn everything_is_finite() {
        let x = vec![0.0f32; SR as usize];
        let track = voice_core::track_pitch(&x, SR as f32, |_| true).unwrap();
        let c = ramp_content(100, 2);
        let a = align(&track, &x, SR, &c, BS).unwrap();
        assert!(a.f0.iter().all(|v| v.is_finite()));
        assert!(a.volume.iter().all(|v| v.is_finite()));
        assert!(a.content.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn mismatched_sample_rates_are_refused() {
        let x = clip(0.0, 220.0, 1.0, 0.3);
        let track = voice_core::track_pitch(&x, 48_000.0, |_| true).unwrap();
        let c = ramp_content(100, 2);
        let e = align(&track, &x, SR, &c, BS).unwrap_err().to_string();
        assert!(e.contains("同源"), "{e}");
    }

    /// 同样的输入跑两遍必须逐位相同 —— 特征是要落盘缓存的。
    #[test]
    fn alignment_is_deterministic() {
        let x = clip(0.3, 196.0, 1.5, 0.25);
        let track = voice_core::track_pitch(&x, SR as f32, |_| true).unwrap();
        let c = ramp_content(150, 2);
        let a = align(&track, &x, SR, &c, BS).unwrap();
        let b = align(&track, &x, SR, &c, BS).unwrap();
        assert_eq!(a.f0, b.f0);
        assert_eq!(a.voiced, b.voiced);
        assert_eq!(a.volume, b.volume);
        assert_eq!(a.content, b.content);
    }
}
