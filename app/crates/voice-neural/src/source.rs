//! 激励信号生成：由 f0 造出「梳齿波」(combtooth)。
//!
//! # 它在解码器里的位置
//!
//! DDSP 解码器的思路是**网络不直接吐波形**，而是吐一个经典合成器的参数。
//! 这一半是那个合成器的「声源」：
//!
//! ```text
//! f0 ──→ 梳齿波（本模块）──→ STFT ──┐
//!                                    ├─→ 乘上网络预测的滤波器 ──→ ISTFT ──→ 波形
//! 白噪声 ─────────────────→ STFT ──┘
//! ```
//!
//! 声源完全由 f0 决定，**不含任何权重** —— 所以它是整个解码器里
//! 唯一能在没有模型的情况下完整验证的部分。音高准不准、颤音在不在、
//! 有没有咔哒声，全在这一步定下来。
//!
//! # 为什么是 sinc 而不是锯齿波
//!
//! 直接生成锯齿或方波会有**混叠** —— 谐波超过奈奎斯特之后折回来，
//! 听起来是金属味的脏。梳齿波用 `sinc` 造带限脉冲串：
//! 每个脉冲是一个 sinc，它的频谱天然截在奈奎斯特处。
//!
//! # 帧内相位是二阶的，不是线性的
//!
//! 一帧之内 f0 也在变（颤音 6 Hz，一帧 11.6 ms，帧内就能变百分之几）。
//! 相位按**匀加速**推进而不是匀速：`s0·(n+1) + ½·ds0·n(n+1)/block`。
//! 按匀速算，每帧末尾会攒下一点相位误差，逐帧累积成可听的音高漂移。
//!
//! 移植自 DDSP-SVC 的 `CombSubSuperFast.fast_source_gen`。

/// 一帧激励。
pub struct Source {
    /// 逐样本的梳齿波，长度 `frames * block_size`。
    pub combtooth: Vec<f32>,
    /// 每帧起点的相位（弧度）。网络要拿它当输入之一。
    pub phase: Vec<f32>,
}

/// 由逐帧 f0 生成梳齿波激励。
///
/// `f0` 是每帧一个值（Hz，0 表示清音），`block_size` 是解码器的帧步进。
pub fn combtooth(f0: &[f32], sample_rate: f32, block_size: usize) -> Source {
    let frames = f0.len();
    let mut combtooth = vec![0.0f32; frames * block_size];
    let mut phase = vec![0.0f32; frames];

    // 每帧的「圈数/样本」，以及它到下一帧的增量
    let s0: Vec<f32> = f0.iter().map(|hz| hz / sample_rate).collect();
    let ds0: Vec<f32> = (0..frames)
        .map(|t| if t + 1 < frames { s0[t + 1] - s0[t] } else { 0.0 })
        .collect();

    // 帧与帧之间的相位接力。
    //
    // 不能简单地把每帧相位一路累加下去：f32 在几十万个样本之后会丢精度，
    // 表现为**越唱到后面越跑调**。这里只累加每帧**末尾**那一点相位余数
    // （已经折回 ±0.5 圈），量级始终很小。
    let mut acc = 0.0f32;
    let bs = block_size as f32;

    for t in 0..frames {
        let carry = acc; // 上一帧结束时的相位，本帧从这里接着走

        for n in 0..block_size {
            let nf = n as f32;
            // 帧内二阶相位：匀加速推进
            let mut rad = s0[t] * (nf + 1.0) + 0.5 * ds0[t] * nf * (nf + 1.0) / bs;
            // 帧内的瞬时频率，sinc 的宽度要跟着它走
            let s_inst = s0[t] + ds0[t] * nf / bs;

            rad += carry;
            // 折回 ±0.5 圈：sinc 只在零点附近有能量，不折的话
            // 大数相减会把有效位数吃光
            rad -= rad.round();

            combtooth[t * block_size + n] = sinc(rad / (s_inst + 1e-5));
        }

        // 本帧起始相位（n = 0 处，已加 carry 并折回 ±0.5 圈）。
        //
        // ⚠️ 这是**要喂进网络**的量，所以必须和参考实现逐项一致：
        // 它取的是 `rad[:, :, :1]`，也就是加完 carry、折完之后的第 0 个样本 ——
        // 而 n=0 处的 `rad` 是 `s0·1`（公式里是 `n+1`），不是 0。
        // 少算这一项，条件输入就整体偏了一个样本的相位。
        let first = s0[t] + carry;
        phase[t] = std::f32::consts::TAU * (first - first.round());

        // 本帧末尾的相位余数，折到 [-0.5, 0.5)
        let end = s0[t] * bs + 0.5 * ds0[t] * (bs - 1.0);
        acc = (end + carry + 0.5).rem_euclid(1.0) - 0.5;
    }

    Source { combtooth, phase }
}

/// 归一化 sinc：`sin(πx)/(πx)`，`sinc(0) = 1`。
///
/// ⚠️ 必须是**归一化**的那一种（分子分母都带 π）。用 `sin(x)/x` 的话
/// 脉冲宽度差一个 π 倍，频谱整个错位 —— 而波形看着仍然像个脉冲串。
fn sinc(x: f32) -> f32 {
    if x.abs() < 1e-6 {
        1.0
    } else {
        let p = std::f32::consts::PI * x;
        p.sin() / p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 44_100.0;
    const BS: usize = 512;

    /// 单频点能量（Goertzel）。
    ///
    /// ⚠️ 这里**刻意不用 YIN**，尽管它就在手边而且已经量到 RPA 100%。
    ///
    /// 第一版我用了它，它在 220 Hz 的梳齿波上报 110 Hz。查下去发现
    /// 不是 YIN 坏了，也不是生成器坏了 —— 是**合成脉冲串对时域自相关
    /// 天然不友好**：每个脉冲落在不同的亚采样相位上，采到的波形逐个不同，
    /// 自相关只有 0.694（而周期取整时是 1.000）。
    ///
    /// 频谱上则干干净净：220/440/660/880 四次谐波等幅，110 Hz 只有 0.008 倍。
    /// 真人声不会这样 —— 它有共振峰和噪声成分，自相关表现好得多。
    ///
    /// **对的仪器取决于被测对象，不取决于它有多好用。**
    fn bin_energy(x: &[f32], sr: f32, hz: f32) -> f32 {
        let w = 2.0 * std::f64::consts::PI * hz as f64 / sr as f64;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, v) in x.iter().enumerate() {
            let p = w * i as f64;
            re += *v as f64 * p.cos();
            im += *v as f64 * p.sin();
        }
        ((re * re + im * im).sqrt() / x.len() as f64) as f32
    }

    /// 脉冲间距（样本），取中位数。
    fn pulse_spacing(x: &[f32]) -> f32 {
        let mut peaks = vec![];
        for i in 1..x.len() - 1 {
            if x[i] > 0.5 && x[i] >= x[i - 1] && x[i] >= x[i + 1] {
                peaks.push(i);
            }
        }
        if peaks.len() < 3 {
            return 0.0;
        }
        let mut d: Vec<usize> = peaks.windows(2).map(|w| w[1] - w[0]).collect();
        d.sort_unstable();
        d[d.len() / 2] as f32
    }

    /// 能量必须**只**落在 f0 的整数倍上。
    ///
    /// 这是「音高对不对」的直接检验：脉冲间距对、但次谐波也有能量的话，
    /// 听起来就会低八度 —— 而波形图上完全看不出来。
    #[test]
    fn energy_lands_only_on_harmonics() {
        for hz in [110.0f32, 220.0, 440.0] {
            let f0 = vec![hz; 60];
            let s = combtooth(&f0, SR, BS);
            assert_eq!(s.combtooth.len(), 60 * BS);
            let x = &s.combtooth[BS..BS * 50]; // 掐掉首帧

            let base = bin_energy(x, SR, hz);
            assert!(base > 1e-4, "{hz} Hz 基频能量只有 {base:.6}");

            // 谐波要在
            for k in [2.0f32, 3.0, 4.0] {
                if hz * k > SR / 2.0 {
                    continue;
                }
                let e = bin_energy(x, SR, hz * k) / base;
                assert!(e > 0.5, "{hz} Hz 的 {k} 次谐波只有 {e:.3} 倍 —— 不是带限脉冲串");
            }

            // 次谐波与非谐波不许有
            for (label, probe) in [("次谐波", hz / 2.0), ("三分之一", hz / 3.0), ("非谐波", hz * 1.37)] {
                let e = bin_energy(x, SR, probe) / base;
                assert!(
                    e < 0.05,
                    "{hz} Hz 的{label}（{probe:.1} Hz）有 {e:.3} 倍能量 —— 激励不纯"
                );
            }
        }
    }

    /// 脉冲间距必须等于 `sr / f0`。
    #[test]
    fn pulse_spacing_matches_f0() {
        for hz in [110.0f32, 196.0, 220.0, 440.0] {
            let f0 = vec![hz; 60];
            let s = combtooth(&f0, SR, BS);
            let got = pulse_spacing(&s.combtooth[BS..]);
            let want = SR / hz;
            assert!(
                (got - want).abs() <= 1.0,
                "{hz} Hz：脉冲间距 {got} 样本，应当是 {want:.2}"
            );
        }
    }

    /// 没有 NaN、没有爆幅。
    ///
    /// `sinc(rad / s_inst)` 在 f0→0 时分母趋零 —— 清音帧正好是这种情况。
    #[test]
    fn unvoiced_frames_do_not_blow_up() {
        let mut f0 = vec![220.0f32; 40];
        f0[10..20].fill(0.0); // 换气
        let s = combtooth(&f0, SR, BS);
        assert!(s.combtooth.iter().all(|v| v.is_finite()), "有非有限值");
        assert!(
            s.combtooth.iter().all(|v| v.abs() <= 1.001),
            "幅度超了 1：{}",
            s.combtooth.iter().cloned().fold(0.0f32, |a, b| a.max(b.abs()))
        );
    }

    /// ⚠️ 帧与帧之间不许有相位跳变。
    ///
    /// 相位接力写错的典型表现是每 `block_size` 个样本一个咔哒声 ——
    /// 听感上是持续的"电流声"，而波形图上完全看不出来。
    /// 这里查帧边界处的样本差是否与帧内相当。
    #[test]
    fn frames_are_phase_continuous() {
        let f0 = vec![196.0f32; 40];
        let s = combtooth(&f0, SR, BS);

        // 帧内相邻样本差的分布
        let mut inner: Vec<f32> = Vec::new();
        for t in 0..40 {
            for n in 1..BS {
                inner.push((s.combtooth[t * BS + n] - s.combtooth[t * BS + n - 1]).abs());
            }
        }
        inner.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p99 = inner[inner.len() * 99 / 100];

        for t in 1..40 {
            let jump = (s.combtooth[t * BS] - s.combtooth[t * BS - 1]).abs();
            assert!(
                jump <= p99 * 1.5,
                "第 {t} 帧边界跳了 {jump:.4}，而帧内 P99 只有 {p99:.4} —— 相位接力断了"
            );
        }
    }

    /// 颤音要能穿过去：逐帧 f0 变化时，激励的瞬时周期也要跟着变。
    #[test]
    fn vibrato_survives() {
        let f0: Vec<f32> = (0..100)
            .map(|t| {
                let secs = t as f32 * BS as f32 / SR;
                220.0 * (0.5 * (std::f32::consts::TAU * 6.0 * secs).sin() / 12.0).exp2()
            })
            .collect();
        let s = combtooth(&f0, SR, BS);
        assert!(s.combtooth.iter().all(|v| v.is_finite()));

        // 在颤音的两个极值处各量一次脉冲间距。窗口只取 3 帧，
        // 比颤音周期（167 ms ≈ 14 帧）短得多 —— 量到的是瞬时值。
        let pick = |best: fn(f32, f32) -> bool| -> usize {
            f0.iter()
                .enumerate()
                .skip(2)
                .take(96)
                .fold((2usize, f0[2]), |acc, (i, v)| if best(*v, acc.1) { (i, *v) } else { acc })
                .0
        };
        let hi_t = pick(|v, b| v > b);
        let lo_t = pick(|v, b| v < b);
        let seg = |t: usize| pulse_spacing(&s.combtooth[(t - 1) * BS..(t + 2) * BS]);

        let (sp_hi, sp_lo) = (seg(hi_t), seg(lo_t));
        assert!(sp_hi > 0.0 && sp_lo > 0.0, "没找到脉冲");
        // 间距与频率成反比
        let want = 1200.0 * (f0[hi_t] / f0[lo_t]).log2();
        let got = 1200.0 * (sp_lo / sp_hi).log2();
        assert!(
            (got - want).abs() < 30.0,
            "颤音幅度：要 {want:.0} 音分，激励里只剩 {got:.0} 音分（间距 {sp_lo} → {sp_hi} 样本）"
        );
    }

    /// 归一化 sinc：`sinc(1) = 0`，`sinc(0) = 1`。
    ///
    /// 写成 `sin(x)/x` 的话 `sinc(1)` 会是 0.841 而不是 0 ——
    /// 脉冲宽度差一个 π 倍，而波形看着仍然像脉冲串。
    #[test]
    fn sinc_is_the_normalized_one() {
        assert_eq!(sinc(0.0), 1.0);
        assert!(sinc(1.0).abs() < 1e-6, "sinc(1) = {}，不是归一化版本", sinc(1.0));
        assert!(sinc(2.0).abs() < 1e-6);
        assert!((sinc(0.5) - 2.0 / std::f32::consts::PI).abs() < 1e-5);
    }
}
