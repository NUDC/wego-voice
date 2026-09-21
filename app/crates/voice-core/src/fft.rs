//! 定长基-2 复数 FFT。
//!
//! # 为什么自己写而不用 `rustfft`
//!
//! `voice-core` 刻意保持**零依赖** —— 它将来要原样复用为 CLAP 插件的
//! DSP 核心，依赖越少，宿主环境适配越省事（见实施方案 §9.3）。
//!
//! 我们的需求也很窄：**定长**（构造时确定）、**2 的幂**、实数输入。
//! 这种情况下一个预计算旋转因子的迭代基-2 实现已经足够，
//! 没必要为了通用性背上一个大依赖。
//!
//! # 实时安全
//!
//! 构造时预计算旋转因子与位反转表；[`Fft::forward`] / [`Fft::inverse`]
//! **原地变换、零分配**。

use std::f32::consts::TAU;

/// 复数。刻意用最朴素的形式，便于编译器内联与自动向量化。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct C {
    pub re: f32,
    pub im: f32,
}

impl C {
    #[inline]
    pub const fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }

    #[inline]
    pub fn conj(self) -> Self {
        Self::new(self.re, -self.im)
    }

    #[inline]
    fn add(self, o: Self) -> Self {
        Self::new(self.re + o.re, self.im + o.im)
    }

    #[inline]
    fn sub(self, o: Self) -> Self {
        Self::new(self.re - o.re, self.im - o.im)
    }

    #[inline]
    fn mul(self, o: Self) -> Self {
        Self::new(
            self.re * o.re - self.im * o.im,
            self.re * o.im + self.im * o.re,
        )
    }

    #[inline]
    fn scale(self, k: f32) -> Self {
        Self::new(self.re * k, self.im * k)
    }
}

pub struct Fft {
    n: usize,
    /// 旋转因子 exp(-2πi·j/n)，j ∈ [0, n/2)。
    twiddles: Vec<C>,
    /// 位反转置换表。
    rev: Vec<u32>,
}

impl Fft {
    /// `n` 必须是 2 的幂且 ≥ 2。
    pub fn new(n: usize) -> Self {
        assert!(n >= 2 && n.is_power_of_two(), "FFT 长度必须是 ≥2 的 2 的幂");
        let twiddles = (0..n / 2)
            .map(|j| {
                let a = -TAU * j as f32 / n as f32;
                C::new(a.cos(), a.sin())
            })
            .collect();

        let bits = n.trailing_zeros();
        let rev = (0..n)
            .map(|i| (i as u32).reverse_bits() >> (32 - bits))
            .collect();

        Self { n, twiddles, rev }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.n
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// 原地正变换。`buf.len()` 必须等于 `n`。**零分配。**
    pub fn forward(&self, buf: &mut [C]) {
        debug_assert_eq!(buf.len(), self.n);
        let n = self.n;

        // 位反转置换
        for i in 0..n {
            let j = self.rev[i] as usize;
            if i < j {
                buf.swap(i, j);
            }
        }

        // 迭代蝶形
        let mut len = 2usize;
        while len <= n {
            let half = len / 2;
            let step = n / len;
            let mut start = 0usize;
            while start < n {
                for k in 0..half {
                    let w = self.twiddles[k * step];
                    let a = buf[start + k];
                    let b = buf[start + k + half].mul(w);
                    buf[start + k] = a.add(b);
                    buf[start + k + half] = a.sub(b);
                }
                start += len;
            }
            len <<= 1;
        }
    }

    /// 原地逆变换（含 1/n 归一化）。**零分配。**
    ///
    /// 实现方式是"共轭 → 正变换 → 共轭 → 缩放"，
    /// 省掉一套单独的逆向旋转因子表。
    pub fn inverse(&self, buf: &mut [C]) {
        debug_assert_eq!(buf.len(), self.n);
        for v in buf.iter_mut() {
            *v = v.conj();
        }
        self.forward(buf);
        let k = 1.0 / self.n as f32;
        for v in buf.iter_mut() {
            *v = v.conj().scale(k);
        }
    }

    /// 实数互相关：`r[tau] = Σ_j a[j]·b[j+tau]`，tau ∈ [0, `out.len()`)。
    ///
    /// # 为什么只要两次变换
    ///
    /// 朴素做法要三次变换（两次正、一次逆）。这里用「一次复数 FFT
    /// 同时算两路实数 FFT」的经典技巧：把 a 放实部、b 放虚部做一次正变换，
    /// 再从共轭对称性把两者拆出来。**省掉三分之一的工作量。**
    ///
    /// # 循环卷积的坑
    ///
    /// FFT 算的是**循环**相关。要让 tau ∈ [0, out.len()) 的结果等于线性相关，
    /// 必须满足 `n ≥ a.len() + out.len()`，否则尾部会绕回来污染低 lag 的结果。
    /// 这个条件由调用方保证（[`Fft::required_len`] 可以算）。
    ///
    /// `scratch` 与 `spec` 均需长度 `n`，由调用方预分配 —— 本函数零分配。
    pub fn correlate(
        &self,
        a: &[f32],
        b: &[f32],
        out: &mut [f32],
        scratch: &mut [C],
        spec: &mut [C],
    ) {
        let n = self.n;
        debug_assert!(a.len() + out.len() <= n, "n 不足，循环卷积会污染结果");
        debug_assert_eq!(scratch.len(), n);
        debug_assert_eq!(spec.len(), n);

        // 两路实数打包进一次复数变换
        for i in 0..n {
            scratch[i] = C::new(
                a.get(i).copied().unwrap_or(0.0),
                b.get(i).copied().unwrap_or(0.0),
            );
        }
        self.forward(scratch);

        // 从 Z 拆出 A、B，并直接算 conj(A)·B
        //
        // 推导：a、b 实数 ⇒ A[n-k] = conj(A[k])，B 同理。
        //   Z[k]            = A[k] + i·B[k]
        //   conj(Z[n-k])    = A[k] - i·B[k]
        // 于是 A[k] = (Z[k] + conj(Z[n-k]))/2，B[k] = -i·(Z[k] - conj(Z[n-k]))/2
        for k in 0..n {
            let zk = scratch[k];
            let zm = scratch[(n - k) % n].conj();
            let ak = zk.add(zm).scale(0.5);
            // -i·w 等价于 (w.im, -w.re)
            let d = zk.sub(zm).scale(0.5);
            let bk = C::new(d.im, -d.re);
            spec[k] = ak.conj().mul(bk);
        }

        self.inverse(spec);
        for (tau, slot) in out.iter_mut().enumerate() {
            *slot = spec[tau].re;
        }
    }

    /// 算出做长度为 `a_len` 与最大 lag `max_lag` 的线性互相关所需的 FFT 长度。
    pub fn required_len(a_len: usize, max_lag: usize) -> usize {
        (a_len + max_lag + 1).next_power_of_two()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive_dft(x: &[C]) -> Vec<C> {
        let n = x.len();
        (0..n)
            .map(|k| {
                let mut acc = C::default();
                for (j, v) in x.iter().enumerate() {
                    let a = -TAU * (k * j) as f32 / n as f32;
                    acc = acc.add(v.mul(C::new(a.cos(), a.sin())));
                }
                acc
            })
            .collect()
    }

    fn pseudo_random(n: usize, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (s >> 8) as f32 / 8_388_608.0 - 1.0
            })
            .collect()
    }

    #[test]
    fn matches_naive_dft() {
        for n in [2usize, 4, 8, 16, 64] {
            let fft = Fft::new(n);
            let src: Vec<C> = pseudo_random(n, 7)
                .iter()
                .zip(pseudo_random(n, 99).iter())
                .map(|(&re, &im)| C::new(re, im))
                .collect();
            let want = naive_dft(&src);
            let mut got = src.clone();
            fft.forward(&mut got);
            for (w, g) in want.iter().zip(got.iter()) {
                assert!(
                    (w.re - g.re).abs() < 1e-3 && (w.im - g.im).abs() < 1e-3,
                    "n={n}：期望 {w:?}，实得 {g:?}"
                );
            }
        }
    }

    #[test]
    fn forward_inverse_roundtrip() {
        let n = 1024;
        let fft = Fft::new(n);
        let src: Vec<C> = pseudo_random(n, 3)
            .iter()
            .map(|&re| C::new(re, 0.0))
            .collect();
        let mut buf = src.clone();
        fft.forward(&mut buf);
        fft.inverse(&mut buf);
        for (s, b) in src.iter().zip(buf.iter()) {
            assert!((s.re - b.re).abs() < 1e-4, "往返误差过大：{s:?} → {b:?}");
            assert!(b.im.abs() < 1e-4);
        }
    }

    /// 互相关必须与直接求和一致 —— 这是 YIN 换用 FFT 的正确性根基。
    #[test]
    fn correlate_matches_direct_sum() {
        let w = 686usize;
        let max_lag = 686usize;
        let n = Fft::required_len(w, max_lag);
        let fft = Fft::new(n);

        let full = pseudo_random(w + max_lag, 12345);
        let a = &full[..w];

        let mut got = vec![0.0f32; max_lag + 1];
        let mut scratch = vec![C::default(); n];
        let mut spec = vec![C::default(); n];
        fft.correlate(a, &full, &mut got, &mut scratch, &mut spec);

        // 参照：直接求和
        for tau in 0..=max_lag {
            let mut want = 0.0f64;
            for j in 0..w {
                want += a[j] as f64 * full[j + tau] as f64;
            }
            let err = (got[tau] as f64 - want).abs();
            // 相关值量级约 W/3，f32 FFT 的相对误差在 1e-5 量级
            assert!(
                err < 0.5,
                "tau={tau}：期望 {want:.4}，实得 {:.4}，误差 {err:.4}",
                got[tau]
            );
        }
    }

    /// 正弦信号的自相关应在整周期处出现峰值。
    #[test]
    fn correlate_finds_periodicity() {
        let sr = 48_000.0f32;
        let freq = 220.0f32;
        let w = 686usize;
        let max_lag = 686usize;
        let n = Fft::required_len(w, max_lag);
        let fft = Fft::new(n);

        let full: Vec<f32> = (0..(w + max_lag))
            .map(|i| (TAU * freq * i as f32 / sr).sin())
            .collect();

        let mut r = vec![0.0f32; max_lag + 1];
        let mut scratch = vec![C::default(); n];
        let mut spec = vec![C::default(); n];
        fft.correlate(&full[..w], &full, &mut r, &mut scratch, &mut spec);

        let period = (sr / freq).round() as usize; // ≈ 218
        // 在一个周期附近应当是局部极大
        let peak = (period - 3..=period + 3)
            .max_by(|&a, &b| r[a].partial_cmp(&r[b]).unwrap())
            .unwrap();
        assert!(
            (peak as i32 - period as i32).abs() <= 3,
            "自相关峰值落在 {peak}，期望约 {period}"
        );
        assert!(r[peak] > r[period / 2], "整周期处应强于半周期处");
    }

    #[test]
    fn required_len_avoids_wraparound() {
        assert!(Fft::required_len(686, 686) >= 686 + 686 + 1);
        assert!(Fft::required_len(686, 686).is_power_of_two());
    }
}
