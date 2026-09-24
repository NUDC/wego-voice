//! 合成级：把激励与噪声在频域滤波，再合回波形。
//!
//! # 在解码器里的位置
//!
//! ```text
//! 梳齿波（source.rs）──→ STFT ──┐
//!                                ├─→ ×网络预测的复数滤波器 ──→ ISTFT ──→ 波形
//! 白噪声 ──────────────→ STFT ──┘
//! ```
//!
//! 网络吐的不是波形，是**两组复数滤波器**（谐波的和噪声的），
//! 每帧一组、每组 `win_length/2+1` 个频点。这一层把它们施加上去。
//!
//! 和 `source.rs` 一样，这里**不含任何权重** —— 给定滤波器，
//! 输出是完全确定的。所以它能在没有模型的情况下验到底。
//!
//! # 两个必须一模一样的细节
//!
//! **窗必须是周期 Hann，不是对称 Hann。** `torch.hann_window` 默认
//! `periodic=True`，即 `w[n] = 0.5 - 0.5·cos(2πn/N)`（分母是 N 不是 N-1）。
//! 用对称版的话 COLA 条件不成立，重建会带上一层随帧起伏的幅度调制 ——
//! 听感上是周期性的"呼吸"，而波形图上几乎看不出来。
//!
//! **ISTFT 要除以窗平方的叠加和**，不是窗的叠加和。重叠相加时每帧
//! 被乘了两次窗（分析一次、合成一次），漏掉这一步首尾会明显发闷。
//!
//! # 噪声用固定种子
//!
//! 参考实现用 `torch.randn_like`，**每次跑出来的波形都不一样**。
//! 我们用固定种子：同一段素材跑两遍必须逐位相同 ——
//! 否则"这次转出来好像好一点"永远分不清是改动生效了还是运气。

use voice_core::fft::{Fft, C};

/// 周期 Hann 窗（与 `torch.hann_window(n)` 一致）。
pub fn hann(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos())
        .collect()
}

/// 复数频谱：`frames` × `bins`，行优先。
pub struct Spectrum {
    pub data: Vec<C>,
    pub frames: usize,
    pub bins: usize,
}

impl Spectrum {
    pub fn frame(&self, t: usize) -> &[C] {
        &self.data[t * self.bins..(t + 1) * self.bins]
    }
    pub fn frame_mut(&mut self, t: usize) -> &mut [C] {
        &mut self.data[t * self.bins..(t + 1) * self.bins]
    }
}

/// 复数乘法。`voice_core::fft::C` 只是个裸结构体，没有实现算符 ——
/// 那个 crate 的 FFT 内部自己展开，不需要。这里补上。
fn cmul(a: C, b: C) -> C {
    C {
        re: a.re * b.re - a.im * b.im,
        im: a.re * b.im + a.im * b.re,
    }
}

fn cadd(a: C, b: C) -> C {
    C { re: a.re + b.re, im: a.im + b.im }
}

/// 反射索引（不重复边界值），与 numpy / torch 的 `reflect` 一致。
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

/// 短时傅里叶变换，`center = true`、反射填充。
///
/// 帧数 = `1 + len / hop`，第 `t` 帧**以 `t * hop` 为中心**。
pub fn stft(x: &[f32], n_fft: usize, hop: usize, window: &[f32]) -> Spectrum {
    let bins = n_fft / 2 + 1;
    let frames = 1 + x.len() / hop;
    let half = n_fft / 2;
    let fft = Fft::new(n_fft);

    let mut data = vec![C::default(); frames * bins];
    let mut buf = vec![C::default(); n_fft];

    for t in 0..frames {
        for (k, slot) in buf.iter_mut().enumerate() {
            // 填充后的下标 t*hop + k 对应原始下标
            let orig = (t * hop + k) as isize - half as isize;
            let v = if x.is_empty() { 0.0 } else { x[reflect(orig, x.len())] };
            *slot = C { re: v * window[k], im: 0.0 };
        }
        fft.forward(&mut buf);
        data[t * bins..(t + 1) * bins].copy_from_slice(&buf[..bins]);
    }

    Spectrum { data, frames, bins }
}

/// 逆短时傅里叶变换，`center = true`。
///
/// 输出长度 `(frames - 1) * hop`。
pub fn istft(spec: &Spectrum, n_fft: usize, hop: usize, window: &[f32]) -> Vec<f32> {
    let half = n_fft / 2;
    let padded_len = (spec.frames - 1) * hop + n_fft;
    let fft = Fft::new(n_fft);

    let mut acc = vec![0.0f32; padded_len];
    // 窗**平方**的叠加和。重叠相加时每帧被乘了两次窗（分析一次、合成一次）。
    let mut wsum = vec![0.0f32; padded_len];
    let mut buf = vec![C::default(); n_fft];

    for t in 0..spec.frames {
        let f = spec.frame(t);
        // 由半谱补出全谱（实信号的共轭对称）
        for k in 0..n_fft {
            buf[k] = if k < spec.bins {
                f[k]
            } else {
                f[n_fft - k].conj()
            };
        }
        fft.inverse(&mut buf);

        let base = t * hop;
        for k in 0..n_fft {
            acc[base + k] += buf[k].re * window[k];
            wsum[base + k] += window[k] * window[k];
        }
    }

    // 掐掉 center padding
    let out_len = (spec.frames - 1) * hop;
    (0..out_len)
        .map(|i| {
            let w = wsum[i + half];
            if w > 1e-8 {
                acc[i + half] / w
            } else {
                0.0
            }
        })
        .collect()
}

/// 确定性白噪声（Box–Muller + xorshift）。
///
/// 参考实现用 `randn_like`，每次结果都不同。这里固定种子：
/// 同一段素材跑两遍必须逐位相同，否则"这次好像好一点"永远分不清
/// 是改动生效了还是运气。
pub fn noise(n: usize, seed: u64) -> Vec<f32> {
    let mut st = seed | 1;
    let mut next = || {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        // 开区间 (0,1)，避免 ln(0)
        ((st >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    };
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let (u1, u2) = (next(), next());
        let r = (-2.0 * u1.ln()).sqrt();
        let th = std::f64::consts::TAU * u2;
        out.push((r * th.cos()) as f32);
        if out.len() < n {
            out.push((r * th.sin()) as f32);
        }
    }
    out
}

/// 一帧的复数滤波器。
///
/// 网络吐的是 `magnitude` 与 `phase`，合成 `exp(mag + iπ·phase)`。
/// 指数形式意味着网络输出的是**对数幅度** —— 它天然非负，
/// 而且动态范围大，不必再夹一层激活函数。
pub fn filter_from(mag: &[f32], phase: &[f32], scale: f32) -> Vec<C> {
    mag.iter()
        .zip(phase)
        .map(|(m, p)| {
            let a = m.exp() * scale;
            let th = std::f32::consts::PI * p;
            C { re: a * th.cos(), im: a * th.sin() }
        })
        .collect()
}

/// 把谐波滤波器与噪声滤波器施加到激励上，合成波形。
///
/// `src_filter` / `noise_filter`：每帧 `n_fft/2+1` 个复数，
/// **帧数必须比 block 帧多 1**（STFT 的 `center=true` 会多出一帧）——
/// 参考实现是把最后一帧复制一份接上去。
pub fn synthesize(
    combtooth: &[f32],
    n_fft: usize,
    hop: usize,
    src_filter: &Spectrum,
    noise_filter: &Spectrum,
    seed: u64,
) -> Vec<f32> {
    let window = hann(n_fft);
    let a = stft(combtooth, n_fft, hop, &window);
    let nz = noise(combtooth.len(), seed);
    let b = stft(&nz, n_fft, hop, &window);

    let frames = a.frames.min(src_filter.frames).min(noise_filter.frames);
    let mut out = Spectrum {
        data: vec![C::default(); frames * a.bins],
        frames,
        bins: a.bins,
    };
    for t in 0..frames {
        let (fa, fb) = (a.frame(t), b.frame(t));
        let (ga, gb) = (src_filter.frame(t), noise_filter.frame(t));
        let dst = out.frame_mut(t);
        for k in 0..a.bins {
            dst[k] = cadd(cmul(fa[k], ga[k]), cmul(fb[k], gb[k]));
        }
    }
    istft(&out, n_fft, hop, &window)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NFFT: usize = 2048;
    const HOP: usize = 512;

    fn tone(hz: f32, sr: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (std::f32::consts::TAU * hz * i as f32 / sr).sin() * 0.5)
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// 周期 Hann：`w[0] = 0`，最大值在中点，**且 `w[n]` 不等于 `w[0]` 的镜像**。
    ///
    /// 对称版的 `w[N-1]` 也是 0，周期版不是 —— 这一位之差就是
    /// COLA 成不成立的分界。
    #[test]
    fn window_is_periodic_hann() {
        let w = hann(8);
        assert!(w[0].abs() < 1e-6, "w[0] 应当是 0");
        assert!((w[4] - 1.0).abs() < 1e-6, "中点应当是 1，实际 {}", w[4]);
        assert!(w[7] > 0.1, "w[N-1] = {} —— 这是对称 Hann，不是周期 Hann", w[7]);
    }

    /// ⚠️ 完美重建：STFT → ISTFT 必须还原原信号。
    ///
    /// 这一条不过，后面所有听感问题都无从谈起 —— 分不清是网络吐错了
    /// 还是重建本身就有损。
    #[test]
    fn stft_istft_round_trips() {
        let x = tone(440.0, 44_100.0, HOP * 30);
        let w = hann(NFFT);
        let spec = stft(&x, NFFT, HOP, &w);
        let y = istft(&spec, NFFT, HOP, &w);

        assert_eq!(y.len(), x.len(), "长度不对");
        // 掐掉首尾各 2 个 n_fft，那里窗叠加还没进入稳态
        let lo = NFFT * 2;
        let hi = x.len() - NFFT * 2;
        let err: f32 = (lo..hi).map(|i| (y[i] - x[i]).abs()).fold(0.0, f32::max);
        assert!(err < 1e-3, "最大重建误差 {err:.6}，应当接近 0");
    }

    /// 帧数与长度的关系必须和参考实现一致。
    #[test]
    fn frame_count_matches_torch() {
        for frames in [10usize, 33, 100] {
            let x = vec![0.1f32; frames * HOP];
            let spec = stft(&x, NFFT, HOP, &hann(NFFT));
            assert_eq!(spec.frames, frames + 1, "{frames} 个 block 应当出 {} 帧", frames + 1);
            assert_eq!(spec.bins, NFFT / 2 + 1);
        }
    }

    /// 滤波器真的在滤：把高频频点清零，高频就该消失。
    #[test]
    fn a_filter_actually_filters() {
        let sr = 44_100.0f32;
        let n = HOP * 40;
        // 低频 + 高频各一个
        let x: Vec<f32> = tone(300.0, sr, n)
            .iter()
            .zip(tone(8000.0, sr, n).iter())
            .map(|(a, b)| a + b)
            .collect();

        let w = hann(NFFT);
        let mut spec = stft(&x, NFFT, HOP, &w);
        // 清掉 2 kHz 以上
        let cut = (2000.0 / sr * NFFT as f32) as usize;
        for t in 0..spec.frames {
            for v in spec.frame_mut(t)[cut..].iter_mut() {
                *v = C::default();
            }
        }
        let y = istft(&spec, NFFT, HOP, &w);
        let core = &y[NFFT * 2..y.len() - NFFT * 2];

        // 低频还在（单音 RMS = 0.5/√2 ≈ 0.354）
        assert!(rms(core) > 0.3, "低频也被滤掉了：RMS {}", rms(core));
        // 8 kHz 没了
        let e = goertzel(core, sr, 8000.0);
        let e_lo = goertzel(core, sr, 300.0);
        assert!(e / e_lo < 0.02, "8 kHz 还剩 {:.3} 倍", e / e_lo);
    }

    fn goertzel(x: &[f32], sr: f32, hz: f32) -> f32 {
        let w = 2.0 * std::f64::consts::PI * hz as f64 / sr as f64;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, v) in x.iter().enumerate() {
            let p = w * i as f64;
            re += *v as f64 * p.cos();
            im += *v as f64 * p.sin();
        }
        ((re * re + im * im).sqrt() / x.len() as f64) as f32
    }

    /// 复数滤波器由对数幅度与相位合成。
    #[test]
    fn filter_uses_log_magnitude() {
        let f = filter_from(&[0.0, 1.0], &[0.0, 0.0], 1.0);
        assert!((f[0].re - 1.0).abs() < 1e-6, "mag=0 应当给出增益 1（e^0）");
        assert!((f[1].re - std::f32::consts::E).abs() < 1e-5, "mag=1 应当给出 e");
        // phase=1 → 相位 π → 实部取反
        let g = filter_from(&[0.0], &[1.0], 1.0);
        assert!((g[0].re + 1.0).abs() < 1e-5, "phase=1 应当把相位转 π");
    }

    /// 噪声：均值 ~0、标准差 ~1，而且**可复现**。
    #[test]
    fn noise_is_gaussian_and_reproducible() {
        let a = noise(20_000, 42);
        let b = noise(20_000, 42);
        assert_eq!(a, b, "同种子跑两遍结果不同 —— 产物将不可复现");

        let mean = a.iter().sum::<f32>() / a.len() as f32;
        let sd = rms(&a);
        assert!(mean.abs() < 0.05, "均值 {mean}");
        assert!((sd - 1.0).abs() < 0.05, "标准差 {sd}");
        assert!(a.iter().all(|v| v.is_finite()));

        let c = noise(20_000, 7);
        assert_ne!(a, c, "不同种子应当给出不同噪声");
    }

    /// 整条合成路径：全通谐波滤波器 + 零噪声滤波器 = 原样输出激励。
    #[test]
    fn synthesize_with_unit_filters_returns_the_exciter() {
        let f0 = vec![220.0f32; 30];
        let src = crate::source::combtooth(&f0, 44_100.0, HOP);
        let bins = NFFT / 2 + 1;
        let frames = src.combtooth.len() / HOP + 1;

        let one = Spectrum {
            data: vec![C { re: 1.0, im: 0.0 }; frames * bins],
            frames,
            bins,
        };
        let zero = Spectrum { data: vec![C::default(); frames * bins], frames, bins };

        let y = synthesize(&src.combtooth, NFFT, HOP, &one, &zero, 1);
        assert_eq!(y.len(), src.combtooth.len());

        let lo = NFFT * 2;
        let hi = y.len() - NFFT * 2;
        let err: f32 = (lo..hi)
            .map(|i| (y[i] - src.combtooth[i]).abs())
            .fold(0.0, f32::max);
        assert!(err < 1e-3, "全通滤波器下最大误差 {err:.6}");
    }

    /// 同样的输入跑两遍必须逐位相同。
    #[test]
    fn synthesis_is_deterministic() {
        let f0 = vec![196.0f32; 20];
        let src = crate::source::combtooth(&f0, 44_100.0, HOP);
        let bins = NFFT / 2 + 1;
        let frames = src.combtooth.len() / HOP + 1;
        let g = Spectrum {
            data: vec![C { re: 0.5, im: 0.1 }; frames * bins],
            frames,
            bins,
        };
        let a = synthesize(&src.combtooth, NFFT, HOP, &g, &g, 9);
        let b = synthesize(&src.combtooth, NFFT, HOP, &g, &g, 9);
        assert_eq!(a, b);
    }
}
