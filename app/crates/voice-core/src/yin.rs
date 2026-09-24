//! YIN 基频检测。
//!
//! 参考：de Cheveigné & Kawahara (2002),
//! "YIN, a fundamental frequency estimator for speech and music".
//!
//! # 实时约束
//!
//! [`Yin::analyze`] **不做任何堆分配** —— 所有工作缓冲在 [`Yin::new`] 中预分配。
//! 这是实时纪律的硬要求。
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
        // 峰值一度顶到 88%。
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

        best_tau = self.undo_subharmonic(best_tau);

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

    /// 次谐波回收：把被噪声压到 2τ / 3τ 上的估计拉回真实周期。
    ///
    /// # 这是量出来的，不是猜的
    ///
    /// `wego-bench f0` 在 6 dB 信噪比（有空调/风扇的房间）下测到 24% 的帧
    /// 差了整八度或十二度，而误差**全部朝下**：−12 半音 188 帧、−19 半音 73 帧，
    /// 朝上的一帧都没有。
    ///
    /// 成因是步骤 4 的"第一个跌破阈值"：噪声把真周期处的谷填浅了，跨不过
    /// 0.15，扫描就继续往大 tau 走；而 CMND 的累积均值归一化让 d′ 随 tau
    /// 增大而系统性变小，于是 2τ、3τ 反而先跌破 —— 正好是朝下的方向。
    ///
    /// 离线的邻域中值救不了：实测错段中位数 13 帧、最长 83 帧，
    /// 而中值窗口只有 ±12 帧 —— 中值本身就落在错的那一侧。
    /// 所以必须在这里修，而不是在后处理里。
    ///
    /// 判据是**单向**的（只往高频方向捞），因为观测到的错误是单向的。
    /// 双向检查会凭空引入朝上的八度错误，那是拿一种病换另一种病。
    fn undo_subharmonic(&self, tau: usize) -> usize {
        /// 候选谷必须至少这么"像个周期"。纯粹的次谐波位置 d′ 会很高，
        /// 这一道就把它们挡住了。
        const CANDIDATE_MAX: f32 = 0.45;
        /// 候选可以比当前选择差多少倍仍被采纳。
        ///
        /// 必须 > 1.0：真周期处的谷被噪声填浅了，正是它**比**次谐波差的原因；
        /// 要求"必须更好"就等于什么都不做。实测 `TOLERANCE=1.0` 时
        /// noisy6 的 RPA 停在 66.8%，1.2 起跳到 100%。
        ///
        /// ⚠️ 扫描结果：1.2 / 1.6 / 2.2 / 3.0 在现有素材上**成绩完全一样**，
        /// `CANDIDATE_MAX` 从 0.35 到 0.90 也一样。也就是说这两个常数
        /// 坐在一片很宽的平台上，**现有素材钉不住它们的上界** ——
        /// 取 1.6 是取平台中段，不是因为量出了最优值。
        /// 真嗓子上会不会在某处翻车，这套合成素材回答不了。
        const TOLERANCE: f32 = 1.6;

        let here = self.cmnd[tau];
        // 先试 ÷3 再试 ÷2：取最高的那个合法候选，避免只回收一半
        for k in [3usize, 2] {
            let cand = tau / k;
            if cand < self.tau_min {
                continue;
            }
            // 谐波关系不会精确到样本，在邻域里找真正的谷底
            let radius = (cand / 50).max(2);
            let lo = cand.saturating_sub(radius).max(self.tau_min);
            let hi = (cand + radius).min(self.tau_max);
            let mut best = lo;
            for t in lo..=hi {
                if self.cmnd[t] < self.cmnd[best] {
                    best = t;
                }
            }
            // 必须是**局部极小**，不能只是某段下坡上的一点
            let is_min = best > 0
                && best < self.tau_max
                && self.cmnd[best] <= self.cmnd[best - 1]
                && self.cmnd[best] <= self.cmnd[best + 1];

            if is_min && self.cmnd[best] < CANDIDATE_MAX && self.cmnd[best] <= here * TOLERANCE {
                return best;
            }
        }
        tau
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

    /// 带噪声的浊音不许被判成低八度/低十二度。
    ///
    /// ⚠️ 回归测试。`wego-bench f0` 在 6 dB 信噪比下量到 24% 的帧偏低，
    /// 而且**全部朝下**（减半 188 帧、÷3 73 帧，朝上零帧）——
    /// 成因是 CMND 的累积均值归一化让 d′ 随 tau 系统性变小，
    /// 于是噪声下 2τ、3τ 反而先跌破绝对阈值。
    ///
    /// 离线的邻域中值救不了：错段中位数 13 帧，而中值窗口只有 ±12 帧。
    #[test]
    fn noise_must_not_drag_the_estimate_down_an_octave() {
        const SR: f32 = 48_000.0;
        let mut yin = Yin::new(YinConfig { sample_rate: SR, ..Default::default() });
        let n = yin.required_len();

        for f0 in [110.0f32, 165.0, 220.0, 330.0] {
            // 谐波堆 + 噪声，近似 6 dB 信噪比
            let mut x: Vec<f32> = (0..n)
                .map(|i| {
                    let t = i as f32 / SR;
                    (1..=12)
                        .map(|k| (TAU * f0 * k as f32 * t).sin() / k as f32)
                        .sum::<f32>()
                })
                .collect();
            let rms = (x.iter().map(|v| v * v).sum::<f32>() / n as f32).sqrt();
            let mut st = 0x9E3779B97F4A7C15u64;
            for v in x.iter_mut() {
                st ^= st << 13;
                st ^= st >> 7;
                st ^= st << 17;
                *v += (((st >> 40) as f32 / 8_388_608.0) - 1.0) * rms * 0.5;
            }

            let est = yin.analyze(&x);
            assert!(est.is_voiced, "{f0} Hz + 噪声被判成清音");
            let cents = 1200.0 * (est.f0_hz / f0).log2();
            assert!(
                cents > -600.0,
                "{f0} Hz 被拖低了 {cents:.0} 音分（估计 {:.1} Hz）—— 次谐波回收失效",
                est.f0_hz
            );
            assert!(cents.abs() < 60.0, "{f0} Hz 估成了 {:.1} Hz", est.f0_hz);
        }
    }

    /// 次谐波回收**不许**把真实低音顶上去。
    ///
    /// 这是上一条的反向风险：判据放松过头，85 Hz 的半周期会被当成基频。
    #[test]
    fn a_genuinely_low_voice_is_not_pushed_up_an_octave() {
        const SR: f32 = 48_000.0;
        let mut yin = Yin::new(YinConfig { sample_rate: SR, ..Default::default() });
        let n = yin.required_len();
        for f0 in [80.0f32, 85.0, 98.0] {
            let x: Vec<f32> = (0..n)
                .map(|i| {
                    let t = i as f32 / SR;
                    (1..=20)
                        .map(|k| (TAU * f0 * k as f32 * t).sin() / k as f32)
                        .sum::<f32>()
                })
                .collect();
            let est = yin.analyze(&x);
            let cents = 1200.0 * (est.f0_hz / f0).log2();
            assert!(
                cents < 600.0,
                "{f0} Hz 被顶高了 {cents:.0} 音分（估计 {:.1} Hz）",
                est.f0_hz
            );
        }
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
