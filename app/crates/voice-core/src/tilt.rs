//! 频谱倾斜滤波器 —— 整体调亮或调暗。
//!
//! # 为什么需要它
//!
//! 声线有两个维度：**共振峰位置**（声道长度）和**频谱倾斜**（整体明暗）。
//! `psola::set_formant` 解决了前者，而后者在这个模块之前是**测得出来、
//! 补不了** —— `timbre::match_to` 会报出 `tilt_delta`，但引擎里没有环节能施加它。
//!
//! 合成素材上实测这个差异能到 5 dB/八度，比共振峰估计那 20% 的偏差大得多。
//! 它是纯 DSP 路线离"真的像"最主要的剩余差距。
//!
//! # 结构：两级（单极点低通 + 互补高通）
//!
//! ```text
//! lp[n] = lp[n-1] + a·(x[n] − lp[n-1])     一阶低通，截止在支点
//! hp[n] = x[n] − lp[n]                      互补高通（两者相加恒等于 x）
//! y[n]  = g_lo·lp[n] + g_hi·hp[n]
//! ```
//!
//! 低频得到 `g_lo`、高频得到 `g_hi`，支点附近平滑过渡 —— 这正是
//! 母带工具里那个「tilt」旋钮的经典做法。
//!
//! **用两级、支点错开。** 单级在 4.9 个八度上做不出直斜率：过渡是 S 形的，
//! 幅度一大频带两端就饱和。实测单级要 -3.5 dB/八度只给到 -2.83（81%）。
//! 把支点放在频带的 1/3 与 2/3 处、各担一半倾斜，过渡摊开之后就直多了。
//!
//! 选它的三个理由：
//!
//! - **无缓冲延迟**：输出第 n 个样本只依赖输入第 n 个样本。群延迟随频率变化、
//!   集中在支点附近且远小于 1ms —— 对 0.08ms 的预算来说，这是唯一可行的形态
//! - **无条件稳定**：`a ∈ (0,1)`，单极点在单位圆内，参数怎么拧都不会发散
//! - **便宜**：每样本三次乘加，没有状态数组、没有分支
//!
//! # 标定不是猜的
//!
//! "多少 dB 的档位差 = 多少 dB/八度"取决于支点位置与分析频带，
//! 解析式算出来的和 `timbre::analyze` 实际量到的不是一回事。
//! 所以 [`CALIBRATION`] 是**实测标定**出来的，并且有测试守着：
//! 要 +3 dB/八度就必须量回 +3 dB/八度。

/// 两级的支点频率（Hz）：分析频带 200~6000 Hz 在对数轴上的 1/3 与 2/3 处。
///
/// 摊开而不是都放中心 —— 两个过渡区接力，合成的斜率才接近直线。
const PIVOT_HZ: [f32; 2] = [622.0, 1934.0];

/// 分析频带跨越的八度数：log2(6000/200) ≈ 4.91。
/// 必须与 `timbre` 的栅格一致，否则标定就错了位。
const BAND_OCTAVES: f32 = 4.907;

/// 档位差 → 实测斜率的标定系数。
///
/// 一阶过渡是 S 形而不是直线，所以频带两端拿不满全部增益；
/// 而 `timbre::tilt` 又是对整段做直线最小二乘。两个效应叠加，
/// 实测斜率只有"总档位差 ÷ 八度数"的一部分。
///
/// **这个数是实测标定的**（见 `calibration_matches_measurement` 测试），
/// 不是推导出来的。改支点或改分析频带都必须重新标定。
///
/// 标定过程见提交记录：单级版先取 2.30（实测 +2.0→+3.01）修正到 1.528，
/// 改两级之后重新标定为 1.30。**改结构必须重标。**
const CALIBRATION: f32 = 1.30;

/// 允许的最大倾斜（dB/八度）。
///
/// ±4 dB/八度在 4.9 个八度上already是 ±20 dB 的总摆幅，
/// 再大就不是"音色"而是"故障"了。
pub const MAX_DB_PER_OCT: f32 = 4.0;

/// 一阶频谱倾斜滤波器。
///
/// **实时安全**：无分配、无锁、无 panic、无分支。
#[derive(Debug, Clone)]
pub struct Tilt {
    /// 两级的低通系数。
    a: [f32; 2],
    /// 低频 / 高频增益（线性），每级各担一半倾斜。
    g_lo: f32,
    g_hi: f32,
    /// 两级的低通状态。
    lp: [f32; 2],
    /// 当前设定（dB/八度）。
    db_per_oct: f32,
    /// 为 0 时走逐位透传的快路径。
    bypass: bool,
}

impl Tilt {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let coef = |hz: f32| {
            (1.0 - (-std::f32::consts::TAU * hz / sr).exp()).clamp(1e-6, 0.999)
        };
        Self {
            a: [coef(PIVOT_HZ[0]), coef(PIVOT_HZ[1])],
            g_lo: 1.0,
            g_hi: 1.0,
            lp: [0.0; 2],
            db_per_oct: 0.0,
            bypass: true,
        }
    }

    /// 设置倾斜量（dB/八度）。正 = 更亮，负 = 更暗。
    pub fn set_db_per_oct(&mut self, s: f32) {
        let s = s.clamp(-MAX_DB_PER_OCT, MAX_DB_PER_OCT);
        if s == self.db_per_oct {
            return;
        }
        self.db_per_oct = s;
        self.bypass = s.abs() < 1e-4;

        // 总档位差 = 斜率 × 八度数 × 标定；两级平摊，每级内部低高再各分一半
        let total_db = s * BAND_OCTAVES * CALIBRATION / 2.0;
        self.g_lo = 10f32.powf(-total_db / 40.0);
        self.g_hi = 10f32.powf(total_db / 40.0);
    }

    #[inline]
    pub fn db_per_oct(&self) -> f32 {
        self.db_per_oct
    }

    /// 清空状态。换设备或重新开始时调用。
    pub fn reset(&mut self) {
        self.lp = [0.0; 2];
    }

    /// 就地处理一块样本。**实时安全。**
    #[inline]
    pub fn process(&mut self, buf: &mut [f32]) {
        // 0 时逐位透传。不只是省 CPU —— 它保证"没开倾斜"与
        // "没有这个模块"在数值上完全一致，A/B 盲测才站得住
        if self.bypass {
            return;
        }
        for s in buf.iter_mut() {
            let mut v = *s;
            for k in 0..2 {
                self.lp[k] += self.a[k] * (v - self.lp[k]);
                let hp = v - self.lp[k];
                v = self.g_lo * self.lp[k] + self.g_hi * hp;
            }
            *s = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timbre;
    use std::f32::consts::TAU;

    const SR: f32 = 48_000.0;

    /// 合成浊音：冲激串过真实带宽的谐振器。
    fn voiced(f0: f32, formants: &[f32], secs: f32) -> Vec<f32> {
        let n = (secs * SR) as usize;
        let period = (SR / f0).round().max(2.0) as usize;
        let mut x: Vec<f32> = (0..n)
            .map(|i| if i % period == 0 { 1.0 } else { 0.0 })
            .collect();
        let r = (-std::f32::consts::PI * 110.0 / SR).exp();
        for &f in formants {
            let th = TAU * f / SR;
            let (a1, a2) = (2.0 * r * th.cos(), -r * r);
            let (mut y1, mut y2) = (0.0f32, 0.0f32);
            for s in x.iter_mut() {
                let y = *s + a1 * y1 + a2 * y2;
                *s = y;
                y2 = y1;
                y1 = y;
            }
        }
        let peak = x.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        x.iter().map(|s| s / peak.max(1e-9) * 0.5).collect()
    }

    fn apply(x: &[f32], s: f32) -> Vec<f32> {
        let mut t = Tilt::new(SR);
        t.set_db_per_oct(s);
        let mut y = x.to_vec();
        t.process(&mut y);
        y
    }

    /// **标定测试：要多少就得给多少。**
    ///
    /// 这条把 `CALIBRATION` 钉死。改支点、改分析频带、改 `timbre` 的栅格，
    /// 都会在这里露馅 —— 而一旦标定漂了，"从参考音频生成"给出的
    /// 倾斜建议施加下去就不会真的补平差异。
    #[test]
    fn calibration_matches_measurement() {
        let x = voiced(150.0, &[700.0, 1300.0, 2700.0], 5.0);
        let base = timbre::analyze(&x, SR).tilt_db_per_oct;

        for &want in &[2.0f32, -2.0, 3.5, -3.5] {
            let got = timbre::analyze(&apply(&x, want), SR).tilt_db_per_oct - base;
            assert!(
                (got - want).abs() < 0.6,
                "要 {want:+.1} dB/八度，实测 {got:+.2}"
            );
        }
    }

    /// **闭环测试：建议施加下去之后，差异必须真的缩小。**
    ///
    /// 这条把 `timbre::match_to`（量）与 `Tilt`（施加）钉在一起。
    /// 两边任何一侧的定义或标定漂了，这里就会失败 ——
    /// 而那种失败在产品上的表现是"点了应用，听起来没变"。
    #[test]
    fn applying_the_suggestion_closes_the_gap() {
        let mine = voiced(130.0, &[620.0, 1150.0, 2500.0], 5.0);
        let other = voiced(190.0, &[760.0, 1420.0, 3050.0], 5.0);

        let a = timbre::analyze(&mine, SR);
        let b = timbre::analyze(&other, SR);
        let before = timbre::match_to(&a, &b).tilt_delta;

        // 只施加倾斜建议（共振峰归 PSOLA 管，不在这条测试范围内）
        let fixed = timbre::analyze(&apply(&mine, before), SR);
        let after = timbre::match_to(&fixed, &b).tilt_delta;

        assert!(
            after.abs() < before.abs() * 0.4,
            "倾斜差从 {before:+.2} 只降到 {after:+.2} dB/八度"
        );
    }

    /// 0 必须是逐位透传 —— 否则"关掉倾斜"和"没有这个模块"听起来不一样，
    /// A/B 盲测就失去意义了。
    #[test]
    fn zero_is_bit_exact_passthrough() {
        let x = voiced(200.0, &[650.0, 1200.0], 1.0);
        assert_eq!(apply(&x, 0.0), x);
    }

    /// 正负必须对称：先 +3 再 -3 应当基本还原。
    ///
    /// 一阶滤波不是严格可逆的（相位不还原），所以只比幅度谱倾斜。
    #[test]
    fn opposite_tilts_cancel() {
        let x = voiced(150.0, &[700.0, 1300.0, 2700.0], 4.0);
        let round = apply(&apply(&x, 3.0), -3.0);
        let a = timbre::analyze(&x, SR).tilt_db_per_oct;
        let b = timbre::analyze(&round, SR).tilt_db_per_oct;
        assert!((a - b).abs() < 0.5, "来回一趟倾斜差了 {:.2} dB/八度", a - b);
    }

    /// 参数怎么拧都不能发散、不能出 NaN。
    #[test]
    fn stays_bounded_at_the_extremes() {
        for &s in &[MAX_DB_PER_OCT, -MAX_DB_PER_OCT] {
            let mut t = Tilt::new(SR);
            t.set_db_per_oct(s * 10.0); // 超范围，应被钳住
            assert!(t.db_per_oct().abs() <= MAX_DB_PER_OCT);

            let mut y = voiced(120.0, &[600.0, 1100.0, 2400.0], 3.0);
            t.process(&mut y);
            assert!(y.iter().all(|v| v.is_finite()), "出现 NaN/Inf");
            let peak = y.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(peak < 8.0, "峰值 {peak:.2}，增益失控");
        }
    }

    /// 分块处理与整块处理必须一致 —— 滤波器状态要跨块延续。
    ///
    /// 不延续的话每个音频块开头都会有一个瞬态，听感是持续的"沙沙"声。
    #[test]
    fn block_size_does_not_change_the_result() {
        let x = voiced(180.0, &[700.0, 1200.0], 1.0);
        let whole = apply(&x, 2.5);

        let mut t = Tilt::new(SR);
        t.set_db_per_oct(2.5);
        let mut chunked = Vec::with_capacity(x.len());
        for c in x.chunks(144) {
            let mut b = c.to_vec();
            t.process(&mut b);
            chunked.extend_from_slice(&b);
        }
        assert_eq!(whole, chunked, "跨块状态没延续");
    }
}
