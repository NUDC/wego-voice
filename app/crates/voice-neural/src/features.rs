//! 特征对齐：把 f0、响度、内容特征放到**同一个时间栅格**上。
//!
//! # 为什么这一步值得单独一个模块，还配这么多测试
//!
//! 三路特征来自三条不同的路径，各有各的步进：
//!
//! | 特征 | 来源 | 原生步进 |
//! |---|---|---|
//! | 内容 | ContentVec @16 kHz | 320 样本 = **20 ms**（50 fps）|
//! | f0 | `voice-core::track_pitch` @48 kHz | 128 样本 = 2.67 ms（375 fps）|
//! | 响度 | 直接从波形算 | 我们自己定 |
//!
//! 对不齐的后果是**模型学到的是垃圾**，而这件事**要等训练跑完才发现** ——
//! 那时候你只知道"不像"，不知道是素材不够、超参不对、还是第 t 帧的音高
//! 配到了第 t+3 帧的音色上。
//!
//! 这类错误没有听感上的特征，只有不变量能抓住它。所以这个模块的测试
//! 不是"跑通了"，而是「已知的事件必须落在已知的帧上」。
//!
//! # 栅格由内容特征说了算
//!
//! 内容特征的帧率是模型定死的（HuBERT 卷积前端 320 倍下采样），改不了。
//! 所以它是基准，f0 和响度往它上面靠 —— 反过来做需要重训模型。
//!
//! 第 `t` 帧对应源采样率下的位置 `t * ENCODER_HOP * rate / ENCODER_RATE`，
//! 48 kHz 下就是 `t * 960`。

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

impl std::fmt::Debug for Aligned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let v = self.voiced.iter().filter(|b| **b).count();
        write!(
            f,
            "Aligned {{ {} 帧, {v} 帧有声, 内容 {}×{} }}",
            self.frames, self.frames, self.dim
        )
    }
}

/// 对齐之后的逐帧特征。三路等长 —— 这是本模块唯一的产出承诺。
///
/// `Debug` 刻意不打印 `content`：那是几十万个浮点数，
/// 一次 `unwrap` 失败就能把终端刷爆，真正的错误反而被冲掉。
pub struct Aligned {
    pub frames: usize,
    /// Hz。`voiced[t] == false` 时为 0。
    pub f0: Vec<f32>,
    pub voiced: Vec<bool>,
    /// dBFS，下限 [`LOUDNESS_FLOOR_DB`]。
    pub loudness_db: Vec<f32>,
    /// 行优先内容特征，`frames * dim`。
    pub content: Vec<f32>,
    pub dim: usize,
}

/// 响度下限。静音段给一个**有限**的值而不是 `-inf` ——
/// 后者会在训练里变成 NaN，而 NaN 一旦出现就再也查不回是哪一帧带进来的。
pub const LOUDNESS_FLOOR_DB: f32 = -80.0;

/// 算响度用的窗长（毫秒）。
///
/// 80 ms ≈ 4 帧。比一帧宽是刻意的：逐帧 RMS 会跟着基频周期抖，
/// 而我们要的是"这一刻唱得多响"，不是"这 20 ms 里波形多大"。
pub const LOUDNESS_WINDOW_MS: f32 = 80.0;

/// 内容特征第 `t` 帧在源采样率下对应的样本位置。
pub fn frame_pos(t: usize, rate: u32) -> usize {
    t * ENCODER_HOP * rate as usize / ENCODER_RATE as usize
}

/// 把 f0 轨与响度对齐到内容特征的栅格上。
pub fn align(
    track: &voice_core::PitchTrack,
    samples: &[f32],
    rate: u32,
    content: &Features,
) -> Result<Aligned> {
    if content.frames == 0 {
        bail!("内容特征是空的");
    }
    if (track.sample_rate - rate as f32).abs() > 1.0 {
        bail!(
            "音高轨的采样率是 {} Hz，波形是 {rate} Hz —— 两者必须同源",
            track.sample_rate
        );
    }

    let n = content.frames;
    let mut f0 = Vec::with_capacity(n);
    let mut voiced = Vec::with_capacity(n);
    let mut loudness_db = Vec::with_capacity(n);

    let half = (LOUDNESS_WINDOW_MS / 1000.0 * rate as f32 / 2.0) as usize;

    for t in 0..n {
        let pos = frame_pos(t, rate);
        let (hz, v) = sample_pitch(track, pos);
        f0.push(hz);
        voiced.push(v);
        loudness_db.push(rms_db(samples, pos, half));
    }

    Ok(Aligned {
        frames: n,
        f0,
        voiced,
        loudness_db,
        content: content.data.clone(),
        dim: content.dim,
    })
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

/// 以 `pos` 为中心、半宽 `half` 的 RMS（dBFS）。
fn rms_db(x: &[f32], pos: usize, half: usize) -> f32 {
    if x.is_empty() {
        return LOUDNESS_FLOOR_DB;
    }
    let lo = pos.saturating_sub(half);
    let hi = (pos + half).min(x.len());
    if hi <= lo {
        return LOUDNESS_FLOOR_DB;
    }
    let seg = &x[lo..hi];
    let ms = seg.iter().map(|v| v * v).sum::<f32>() / seg.len() as f32;
    if ms <= 0.0 {
        return LOUDNESS_FLOOR_DB;
    }
    (10.0 * ms.log10()).max(LOUDNESS_FLOOR_DB)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const SR: u32 = 48_000;

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

    fn fake_content(frames: usize) -> Features {
        Features { data: vec![0.0; frames * 4], frames, dim: 4 }
    }

    #[test]
    fn frame_pos_is_twenty_milliseconds() {
        assert_eq!(frame_pos(0, 48_000), 0);
        assert_eq!(frame_pos(1, 48_000), 960); // 20 ms
        assert_eq!(frame_pos(50, 48_000), 48_000); // 整 1 秒
        assert_eq!(frame_pos(1, 16_000), 320);
    }

    /// 三路必须等长 —— 这是本模块唯一的产出承诺。
    #[test]
    fn all_three_streams_have_the_same_length() {
        let x = clip(0.0, 220.0, 2.0, 0.3);
        let track = voice_core::track_pitch(&x, SR as f32, |_| true).unwrap();
        let c = fake_content(100);
        let a = align(&track, &x, SR, &c).unwrap();
        assert_eq!(a.frames, 100);
        assert_eq!(a.f0.len(), 100);
        assert_eq!(a.voiced.len(), 100);
        assert_eq!(a.loudness_db.len(), 100);
    }

    /// ⚠️ 真正的对齐测试：**已知的事件必须落在已知的帧上。**
    ///
    /// 前 1 秒静音、之后有声。50 fps 下，第 50 帧就是那个边界。
    /// 差几帧的错误在形状测试里完全看不出来，只有这种测试抓得住。
    #[test]
    fn a_known_event_lands_on_the_known_frame() {
        let x = clip(1.0, 220.0, 2.0, 0.3);
        let track = voice_core::track_pitch(&x, SR as f32, |_| true).unwrap();
        let c = fake_content(100);
        let a = align(&track, &x, SR, &c).unwrap();

        // 静音段中部：必须清音、必须在地板上
        assert!(!a.voiced[20], "静音段第 20 帧被判成了有声");
        assert_eq!(a.loudness_db[20], LOUDNESS_FLOOR_DB, "静音段响度不在地板上");

        // 有声段中部：必须有声，且音高对
        assert!(a.voiced[75], "有声段第 75 帧被判成了清音");
        let cents = 1200.0 * (a.f0[75] / 220.0).log2();
        assert!(cents.abs() < 50.0, "第 75 帧音高是 {} Hz，偏 {cents:.0} 音分", a.f0[75]);
        assert!(a.loudness_db[75] > -40.0, "有声段响度只有 {}", a.loudness_db[75]);

        // 边界：响度的跃升必须发生在第 50 帧附近（±3 帧 = ±60 ms）。
        // 响度窗是 80 ms，所以过渡本身就有几帧宽，这个容差是它带来的。
        let jump = (1..100)
            .find(|&t| a.loudness_db[t] > -40.0)
            .expect("整段都没有响起来");
        assert!(
            (jump as i32 - 50).abs() <= 3,
            "响度在第 {jump} 帧才起来，预期第 50 帧附近 —— 对齐差了 {} 帧",
            jump as i32 - 50
        );
    }

    /// 音高插值不许跨过清音段。
    ///
    /// 一边 0 一边 220，线性插出来的 110 是个**凭空捏造的低八度**，
    /// 而它恰好落在换气的位置上 —— 模型会把"换气"学成"降八度"。
    #[test]
    fn pitch_is_never_interpolated_across_silence() {
        let x = clip(1.0, 220.0, 2.0, 0.3);
        let track = voice_core::track_pitch(&x, SR as f32, |_| true).unwrap();
        let c = fake_content(100);
        let a = align(&track, &x, SR, &c).unwrap();

        for t in 0..100 {
            if a.voiced[t] {
                let cents = 1200.0 * (a.f0[t] / 220.0).log2();
                assert!(
                    cents.abs() < 100.0,
                    "第 {t} 帧判为有声却给出 {} Hz（偏 {cents:.0} 音分）—— 多半是跨清音插值",
                    a.f0[t]
                );
            } else {
                assert_eq!(a.f0[t], 0.0, "第 {t} 帧判为清音，f0 却是 {}", a.f0[t]);
            }
        }
    }

    /// 整体加 6 dB，响度必须整体 +6 dB。
    ///
    /// 这条能抓住"忘了开方""log 底写错""窗口归一化漏了"这一类错 ——
    /// 它们都不会让数值变得离谱，只会让刻度悄悄变形。
    #[test]
    fn six_db_of_gain_shows_up_as_six_db() {
        let x = clip(0.0, 220.0, 1.5, 0.2);
        let loud: Vec<f32> = x.iter().map(|v| v * 2.0).collect();
        let track = voice_core::track_pitch(&x, SR as f32, |_| true).unwrap();
        let c = fake_content(60);

        let a = align(&track, &x, SR, &c).unwrap();
        let b = align(&track, &loud, SR, &c).unwrap();

        for t in 10..50 {
            let d = b.loudness_db[t] - a.loudness_db[t];
            assert!(
                (d - 6.02).abs() < 0.1,
                "第 {t} 帧加了 6 dB，响度只涨了 {d:.2} dB"
            );
        }
    }

    /// 静音给有限值，不给 -inf —— NaN 一旦进了训练就再也查不回源头。
    #[test]
    fn silence_gives_a_finite_floor_not_negative_infinity() {
        let x = vec![0.0f32; SR as usize];
        let track = voice_core::track_pitch(&x, SR as f32, |_| true).unwrap();
        let c = fake_content(40);
        let a = align(&track, &x, SR, &c).unwrap();
        assert!(a.loudness_db.iter().all(|v| v.is_finite()), "响度里有非有限值");
        assert!(a.f0.iter().all(|v| v.is_finite()));
        assert_eq!(a.loudness_db[10], LOUDNESS_FLOOR_DB);
    }

    /// 采样率不一致要**当场**报错，而不是安静地算错。
    #[test]
    fn mismatched_sample_rates_are_refused() {
        let x = clip(0.0, 220.0, 1.0, 0.3);
        let track = voice_core::track_pitch(&x, 44_100.0, |_| true).unwrap();
        let c = fake_content(40);
        let e = align(&track, &x, 48_000, &c).unwrap_err().to_string();
        assert!(e.contains("同源"), "{e}");
    }

    /// 同样的输入跑两遍必须逐位相同。
    ///
    /// 特征是要落盘缓存的。不确定的话，缓存命中与否会产出不同的训练结果，
    /// 而这种 bug 的表现是"有时候训出来好有时候不好"。
    #[test]
    fn alignment_is_deterministic() {
        let x = clip(0.3, 196.0, 1.5, 0.25);
        let track = voice_core::track_pitch(&x, SR as f32, |_| true).unwrap();
        let c = fake_content(60);
        let a = align(&track, &x, SR, &c).unwrap();
        let b = align(&track, &x, SR, &c).unwrap();
        assert_eq!(a.f0, b.f0);
        assert_eq!(a.voiced, b.voiced);
        assert_eq!(a.loudness_db, b.loudness_db);
    }
}
