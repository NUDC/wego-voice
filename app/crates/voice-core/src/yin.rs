//! YIN 基频检测。
//!
//! 参考：de Cheveigné & Kawahara (2002),
//! "YIN, a fundamental frequency estimator for speech and music".
//!
//! # 实时约束
//!
//! [`Yin::analyze`] **不做任何堆分配** —— 所有工作缓冲在 [`Yin::new`] 中预分配。
//! 这是实施方案 §9.2 纪律区一的要求。
//!
//! # 延迟
//!
//! 检测基频需要至少 2~3 个基音周期。男低音 F0≈80Hz → 周期 12.5ms，
//! 因此分析窗至少 25~37ms。这是物理下限，不是实现问题。
//!
//! # 计算量
//!
//! 差分函数里的互相关项走 FFT（[`crate::fft`]），整体 O(W log W)。
//! 早期版本是 O(W·τ_max) 的直接求和，约 441k 次乘加 —— 在 WASAPI 独占模式
//! 把每回调预算压到 3ms 之后，这一项把回调耗时峰值顶到了预算的 88%。

use crate::fft::{Fft, C};

/// 一次分析的结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PitchEstimate {
    /// 基频（Hz）。`is_voiced == false` 时该值无意义。
    pub f0_hz: f32,
    /// YIN 的非周期性度量（CMND 在最优 tau 处的取值）。
    /// 越小越可信：< 0.1 基本是稳定乐音，> 0.3 多半是清音或噪声。
    pub aperiodicity: f32,
    /// 是否判定为浊音（有确定基频）。清音/静音段不应做音高修正。
    pub is_voiced: bool,
}

impl PitchEstimate {
    pub const UNVOICED: Self = Self {
        f0_hz: 0.0,
        aperiodicity: 1.0,
        is_voiced: false,
    };
}

/// YIN 检测器配置。
#[derive(Debug, Clone, Copy)]
pub struct YinConfig {
    pub sample_rate: f32,
    /// 可检测的最低基频。决定 `tau_max`，进而决定所需的分析窗长度。
    pub f0_min: f32,
    /// 可检测的最高基频。决定 `tau_min`。
    pub f0_max: f32,
    /// CMND 绝对阈值。YIN 论文建议 0.1~0.15；
    /// 唱歌场景噪声大，放宽到 0.15 可减少漏检。
    pub threshold: f32,
    /// 低于此 RMS 视为静音，直接返回 unvoiced，跳过全部计算。
    pub silence_rms: f32,
}

impl Default for YinConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            f0_min: 70.0,   // 男低音留余量
            f0_max: 1100.0, // 女高音留余量
            threshold: 0.15,
            silence_rms: 1e-4,
        }
    }
}

pub struct Yin {
    cfg: YinConfig,
    tau_min: usize,
    tau_max: usize,
    /// 分析窗长度（用于差分函数的比较长度 W）。
    window: usize,
    /// 差分函数 d[tau]，预分配长度 tau_max + 1。
    diff: Vec<f32>,
    /// 累积均值归一化差分 d'[tau]，预分配。
    cmnd: Vec<f32>,

    // ---- FFT 加速用的预分配缓冲 ----
    fft: Fft,
    /// 互相关结果 ac[tau]。
    corr: Vec<f32>,
    fft_scratch: Vec<C>,
    fft_spec: Vec<C>,
}

impl Yin {
    pub fn new(cfg: YinConfig) -> Self {
        assert!(cfg.f0_min > 0.0 && cfg.f0_max > cfg.f0_min);
        let tau_min = (cfg.sample_rate / cfg.f0_max).floor().max(2.0) as usize;
        let tau_max = (cfg.sample_rate / cfg.f0_min).ceil() as usize;
        // 差分函数需要 W 个样本做比较，再加 tau_max 个样本做位移。
        // W = tau_max 是常见取法：保证最低频也有一个完整周期参与比较。
        let window = tau_max;

        let n = Fft::required_len(window, tau_max);
        Self {
            cfg,
            tau_min,
            tau_max,
            window,
            diff: vec![0.0; tau_max + 1],
            cmnd: vec![0.0; tau_max + 1],
            fft: Fft::new(n),
            corr: vec![0.0; tau_max + 1],
            fft_scratch: vec![C::default(); n],
            fft_spec: vec![C::default(); n],
        }
    }

    /// 内部使用的 FFT 长度。诊断与基准测试用。
    #[inline]
    pub fn fft_len(&self) -> usize {
        self.fft.len()
    }

    /// `analyze` 所需的最小输入长度。调用方必须喂够这么多样本。
    #[inline]
    pub fn required_len(&self) -> usize {
        self.window + self.tau_max
    }

    #[inline]
    pub fn tau_max(&self) -> usize {
        self.tau_max
    }

    /// 分析一段样本，返回基频估计。
    ///
    /// `x` 的长度必须 >= [`required_len`](Self::required_len)；
    /// 只使用开头的 `required_len()` 个样本，其余忽略。
    ///
    /// **实时安全**：无分配、无锁、无 panic 路径（长度不足时返回 UNVOICED 而非 panic）。
    pub fn analyze(&mut self, x: &[f32]) -> PitchEstimate {
        let need = self.required_len();
        if x.len() < need {
            return PitchEstimate::UNVOICED;
        }
        let w = self.window;

        // --- 静音快速通道：省掉 O(W·tau) 的主循环 ---
        let mut energy = 0.0f32;
        for &s in &x[..w] {
            energy += s * s;
        }
        let rms = (energy / w as f32).sqrt();
        if rms < self.cfg.silence_rms {
            return PitchEstimate::UNVOICED;
        }

        // --- 步骤 1/2：差分函数 ---
        //
        // d[tau] = Σ_{j<W} (x[j] - x[j+tau])²
        //        = Σx[j]² + Σx[j+tau]² - 2·Σx[j]·x[j+tau]
        //        = p0 + p_tau - 2·ac[tau]
        //
        // p_tau 增量推进，O(1) 每个 tau。
        //
        // ac[tau] 原先是 O(W) 内循环，整体 O(W·τ_max) ≈ 441k 次乘加。
        // 实测下来这是回调耗时的大头：独占模式把每回调预算压到 3ms 之后，
        // 峰值一度顶到 88%（见 docs/Phase0-实测记录.md §2.4）。
        // 现在改用 FFT 互相关，降到 O(W log W)。
        self.fft.correlate(
            &x[..w],
            &x[..need],
            &mut self.corr,
            &mut self.fft_scratch,
            &mut self.fft_spec,
        );

        let p0 = energy;
        let mut p_tau = energy;
        self.diff[0] = 0.0;

        for tau in 1..=self.tau_max {
            // p_tau: 窗口 [tau, tau+W) 的能量，由 [tau-1, tau-1+W) 滑动而来
            let leaving = x[tau - 1];
            let entering = x[tau - 1 + w];
            p_tau += entering * entering - leaving * leaving;

            // 数值误差可能让结果略小于 0，钳到 0 避免后续 sqrt/除法出问题
            self.diff[tau] = (p0 + p_tau - 2.0 * self.corr[tau]).max(0.0);
        }

        // --- 步骤 3：累积均值归一化 ---
        self.cmnd[0] = 1.0;
        let mut running = 0.0f32;
        for tau in 1..=self.tau_max {
            running += self.diff[tau];
            self.cmnd[tau] = if running > 0.0 {
                self.diff[tau] * tau as f32 / running
            } else {
                1.0
            };
        }

        // --- 步骤 4：绝对阈值 ---
        //
        // 取第一个跌破阈值的 tau，然后沿着局部极小继续下降。
        // "第一个"而非"全局最小"是 YIN 抑制倍频错误的关键：
        // 低八度的 tau 总是更大，先到先得就不会误判成低八度。
        let mut best_tau = 0usize;
        let mut tau = self.tau_min;
        while tau <= self.tau_max {
            if self.cmnd[tau] < self.cfg.threshold {
                // 沿局部极小继续走到谷底
                while tau + 1 <= self.tau_max && self.cmnd[tau + 1] < self.cmnd[tau] {
                    tau += 1;
                }
                best_tau = tau;
                break;
            }
            tau += 1;
        }

        // 没有任何 tau 跌破阈值 → 退回区间内全局最小，并按其取值判定清浊
        if best_tau == 0 {
            let mut min_val = f32::INFINITY;
            for t in self.tau_min..=self.tau_max {
                if self.cmnd[t] < min_val {
                    min_val = self.cmnd[t];
                    best_tau = t;
                }
            }
            if best_tau == 0 {
                return PitchEstimate::UNVOICED;
            }
        }

        let aperiodicity = self.cmnd[best_tau];
        // 放宽一档作为清浊判据：阈值内必浊，远高于阈值则判清音
        let is_voiced = aperiodicity < self.cfg.threshold * 2.0;

        // --- 步骤 5：抛物线插值，取得亚采样精度 ---
        let tau_refined = self.parabolic_refine(best_tau);
        let f0 = self.cfg.sample_rate / tau_refined;

        PitchEstimate {
            f0_hz: f0,
            aperiodicity,
            is_voiced,
        }
    }

    /// 在 `tau` 附近对 CMND 做抛物线拟合，返回亚采样精度的极小点。
    ///
    /// 不做这一步的话，48kHz 下 tau=100（480Hz）的相邻整数 tau 相差约 5Hz，
    /// 折合 17 cents —— 直接超出我们 15 cents 的音准误差指标。
    fn parabolic_refine(&self, tau: usize) -> f32 {
        if tau == 0 || tau + 1 > self.tau_max {
            return tau as f32;
        }
        let y0 = self.cmnd[tau - 1];
        let y1 = self.cmnd[tau];
        let y2 = self.cmnd[tau + 1];
        let denom = y0 - 2.0 * y1 + y2;
        if denom.abs() < 1e-12 {
            return tau as f32;
        }
        let offset = 0.5 * (y0 - y2) / denom;
        // 插值结果偏离中心超过一个采样说明拟合不可信，放弃插值
        if offset.abs() > 1.0 {
            tau as f32
        } else {
            tau as f32 + offset
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn sine(freq: f32, sr: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (TAU * freq * i as f32 / sr).sin()).collect()
    }

    /// 带谐波的信号更接近真实人声，也更容易诱发倍频错误。
    fn harmonic(freq: f32, sr: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / sr;
                (TAU * freq * t).sin()
                    + 0.5 * (TAU * 2.0 * freq * t).sin()
                    + 0.25 * (TAU * 3.0 * freq * t).sin()
            })
            .collect()
    }

    fn cents(a: f32, b: f32) -> f32 {
        1200.0 * (a / b).log2()
    }

    #[test]
    fn detects_pure_tones_across_vocal_range() {
        let cfg = YinConfig::default();
        let mut yin = Yin::new(cfg);
        let n = yin.required_len() + 64;

        for &f in &[82.4, 110.0, 220.0, 440.0, 523.25, 880.0] {
            let x = sine(f, cfg.sample_rate, n);
            let est = yin.analyze(&x);
            assert!(est.is_voiced, "{f} Hz 应判定为浊音");
            let err = cents(est.f0_hz, f).abs();
            assert!(err < 15.0, "{f} Hz: 误差 {err:.1} cents，超出 15 cents 指标");
        }
    }

    #[test]
    fn detects_harmonic_tones_without_octave_error() {
        let cfg = YinConfig::default();
        let mut yin = Yin::new(cfg);
        let n = yin.required_len() + 64;

        for &f in &[98.0, 146.83, 261.63, 440.0] {
            let x = harmonic(f, cfg.sample_rate, n);
            let est = yin.analyze(&x);
            assert!(est.is_voiced, "{f} Hz 应判定为浊音");
            let err = cents(est.f0_hz, f).abs();
            // 倍频错误会表现为 ±1200 cents 的偏差，这个断言能抓住它
            assert!(err < 20.0, "{f} Hz: 误差 {err:.1} cents（可能是倍频错误）");
        }
    }

    #[test]
    fn silence_is_unvoiced() {
        let mut yin = Yin::new(YinConfig::default());
        let x = vec![0.0; yin.required_len()];
        assert!(!yin.analyze(&x).is_voiced);
    }

    #[test]
    fn white_noise_is_unvoiced() {
        let mut yin = Yin::new(YinConfig::default());
        // 确定性的伪随机序列，避免测试不稳定
        let mut seed = 0x1234_5678u32;
        let x: Vec<f32> = (0..yin.required_len())
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / 8_388_608.0 - 1.0
            })
            .collect();
        assert!(!yin.analyze(&x).is_voiced, "白噪声不应判为浊音");
    }

    #[test]
    fn short_input_returns_unvoiced_instead_of_panicking() {
        let mut yin = Yin::new(YinConfig::default());
        let x = vec![0.5; 10];
        assert!(!yin.analyze(&x).is_voiced);
    }

    /// 被 FFT 取代的那段直接求和，保留为测试基准。
    ///
    /// 这是换实现时最要紧的一条防线：新旧两条路径必须给出同一个答案。
    fn diff_direct(x: &[f32], w: usize, tau_max: usize) -> Vec<f32> {
        let mut d = vec![0.0f32; tau_max + 1];
        for (tau, slot) in d.iter_mut().enumerate().skip(1) {
            let mut acc = 0.0f64;
            for j in 0..w {
                let delta = (x[j] - x[j + tau]) as f64;
                acc += delta * delta;
            }
            *slot = acc as f32;
        }
        d
    }

    /// FFT 路径算出的差分函数必须与直接求和一致。
    #[test]
    fn fft_diff_matches_direct_sum() {
        let cfg = YinConfig::default();
        let mut yin = Yin::new(cfg);
        let need = yin.required_len();
        let w = yin.window;
        let tau_max = yin.tau_max;

        // 用带谐波的信号：比纯正弦更接近真实人声，也更容易暴露数值问题
        let x = harmonic(147.0, cfg.sample_rate, need);
        let _ = yin.analyze(&x);

        let want = diff_direct(&x, w, tau_max);

        // 差分值量级约为 W（窗内能量和），f32 FFT 相对误差 ~1e-5，
        // 所以按相对误差判定，而不是绝对值
        let scale = want.iter().cloned().fold(0.0f32, f32::max).max(1.0);
        for tau in 1..=tau_max {
            let err = (yin.diff[tau] - want[tau]).abs() / scale;
            assert!(
                err < 1e-3,
                "tau={tau}：直接求和 {:.3}，FFT {:.3}，相对误差 {err:.2e}",
                want[tau],
                yin.diff[tau]
            );
        }
    }

    /// 换 FFT 之后，检测结果本身不能有可闻变化。
    ///
    /// 上一条测的是中间量，这条测的是**最终音高**——
    /// 即便差分函数有微小数值差异，落到 f0 上也必须远小于 1 音分。
    #[test]
    fn fft_path_preserves_detected_pitch() {
        let cfg = YinConfig::default();
        let mut yin = Yin::new(cfg);
        let need = yin.required_len();

        for &f in &[82.4f32, 110.0, 146.83, 220.0, 440.0, 880.0] {
            let x = harmonic(f, cfg.sample_rate, need);
            let est = yin.analyze(&x);
            assert!(est.is_voiced, "{f} Hz 应判定为浊音");
            let err = cents(est.f0_hz, f).abs();
            assert!(err < 20.0, "{f} Hz：偏差 {err:.1} cents");
        }
    }

    /// FFT 长度必须足够长，否则循环卷积会绕回来污染低 lag 的结果 ——
    /// 那会表现为"某些音高莫名其妙检测错"，极难排查。
    #[test]
    fn fft_length_avoids_wraparound() {
        let yin = Yin::new(YinConfig::default());
        assert!(
            yin.fft_len() >= yin.window + yin.tau_max + 1,
            "FFT 长度 {} 不足以容纳 W={} + tau_max={}",
            yin.fft_len(),
            yin.window,
            yin.tau_max
        );
        assert!(yin.fft_len().is_power_of_two());
    }
}
