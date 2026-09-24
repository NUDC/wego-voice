//! f0 评测台：拿**已知真值**的合成素材去量音高检测的准确度。
//!
//! # 为什么先做这个，而不是直接换模型
//!
//! "换成 RMVPE 会更准" —— 这是个信念，不是测量。而它的报价是
//! **361 MB 模型**（外加 ONNX Runtime），装进一个 6.5 MB 的免安装 exe 里。
//!
//! 这么大的代价，必须先有一个数说明当前的 f0 差在哪、差多少。
//! 没有基线的话，换完也说不清是不是真的变好了 —— 只能靠"听着像"。
//!
//! 所以这里先造基线：合成一批**每一刻的 f0 都精确已知**的素材，
//! 跑一遍检测器，按 MIR 领域的标准指标打分。
//!
//! # 指标
//!
//! 沿用 `mir_eval.melody` 那一套，因为它们能把不同的失败**分开**：
//!
//! - **RPA**（原始音高准确率）—— 真值有声的帧里，估计值落在 ±50 音分内的比例
//! - **RCA**（原始音级准确率）—— 同上，但**折叠八度**
//! - **RPA 与 RCA 之差就是八度错误**。这是 YIN 最典型的失败，
//!   混在一个总准确率里根本看不出来，而它对声线转换是致命的
//! - **浊音召回 / 虚警** —— 该出声时没出、不该出声时出了，是两种病
//! - **音分误差中位数 / P90** —— 准的那部分到底有多准
//!
//! # 真值怎么对齐
//!
//! YIN 在一个窗口上做自相关，得到的是**整窗的平均周期**，不是某一瞬间的值。
//! 所以真值取该窗口内的**几何平均**（音分域的算术平均），而不是窗口右端那一刻 ——
//! 否则滑音上会凭空多出半个窗口的滞后误差，那是对齐方式造成的，不是检测器的错。
//!
//! 窗口里跨了"有声↔无声"边界的帧**不计入音高指标**（仍计入浊音判定）：
//! 那种帧里没有任何单一正确答案，算进去只是在制造噪声。

/// 一段带已知 f0 的合成素材。
pub struct Probe {
    pub name: &'static str,
    pub what: &'static str,
    pub samples: Vec<f32>,
    pub sample_rate: f32,
    /// 逐样本真值（Hz）。0 = 静音或清音。
    pub truth: Vec<f32>,
    /// 只作参考，不计入平均分。
    ///
    /// 有些素材是用来**标出系统放弃的位置**的（比如 0 dB 信噪比）。
    /// 那里的正确行为是判为清音而不是硬报一个音高，
    /// 于是它永远是 0 分 —— 算进平均分只会把一条设计选择记成失败。
    pub informational: bool,
}

/// 一次评测的成绩。
#[derive(Debug, Clone, Copy, Default)]
pub struct Score {
    /// 参与音高指标的帧数。
    pub evaluated: usize,
    /// 真值有声的帧数。
    pub voiced_truth: usize,
    /// 真值无声的帧数。
    pub unvoiced_truth: usize,
    /// 落在 ±50 音分内的比例。
    pub rpa: f32,
    /// 折叠八度之后落在 ±50 音分内的比例。
    pub rca: f32,
    /// 音级对但差了整八度的比例 —— `rca - rpa` 的具体来源。
    pub octave_error: f32,
    /// 真值有声且被判为浊音的比例。
    pub voicing_recall: f32,
    /// 真值无声却被判为浊音的比例。
    pub voicing_false_alarm: f32,
    /// 判对音高那部分的 |误差| 中位数（音分）。
    pub median_cents: f32,
    pub p90_cents: f32,
}

/// 检测器交出来的一帧。`window_end` 是分析窗右端的样本位置。
#[derive(Debug, Clone, Copy)]
pub struct Frame {
    pub window_end: usize,
    pub f0: f32,
    pub voiced: bool,
}

fn cents(a: f32, b: f32) -> f32 {
    1200.0 * (a / b).log2()
}

/// 折叠到一个八度之内的音分差（-600..=600）。
fn chroma_cents(a: f32, b: f32) -> f32 {
    let mut c = cents(a, b) % 1200.0;
    if c > 600.0 {
        c -= 1200.0;
    } else if c < -600.0 {
        c += 1200.0;
    }
    c
}

/// 按真值给一串帧打分。
///
/// `window` 是分析窗长度（样本）—— 真值要在同样的窗口上取平均才可比。
pub fn score(truth: &[f32], frames: &[Frame], window: usize) -> Score {
    let mut s = Score::default();
    let mut errs: Vec<f32> = Vec::new();
    let (mut rpa_hit, mut rca_hit, mut oct) = (0usize, 0usize, 0usize);
    let (mut recall_hit, mut fa_hit) = (0usize, 0usize);

    for f in frames {
        let end = f.window_end.min(truth.len());
        let start = end.saturating_sub(window);
        let win = &truth[start..end];
        if win.is_empty() {
            continue;
        }

        let voiced_n = win.iter().filter(|v| **v > 0.0).count();
        let truth_voiced = voiced_n * 2 > win.len(); // 窗口里过半有声就算有声帧

        if truth_voiced {
            s.voiced_truth += 1;
            if f.voiced {
                recall_hit += 1;
            }
        } else {
            s.unvoiced_truth += 1;
            if f.voiced {
                fa_hit += 1;
            }
        }

        // 窗口里跨了**突变**的帧不计入音高指标：
        //
        // - 有声↔无声边界：窗口里一半是静音，没有单一正确答案
        // - 音与音之间的跳进：窗口同时装着 165 和 330，检测器报哪个都"错"，
        //   而几何平均（233）是两边都不认的第三个值 —— 那是评测方式的问题，
        //   不是检测器的问题。**不剔掉的话，跳进越密的素材分数越低，
        //   而这个分差纯属虚构。**
        //
        // 判据是**逐样本**斜率：滑音与颤音再陡也只有 0.04 音分/样本，
        // 而音与音之间的跳进是不连续的。50 音分/样本 只会打到后者。
        let clean_voicing = voiced_n == win.len() || voiced_n == 0;
        let no_jump = !win.windows(2).any(|w| {
            w[0] > 0.0 && w[1] > 0.0 && cents(w[1], w[0]).abs() > 50.0
        });
        if !truth_voiced || !clean_voicing || !no_jump || !f.voiced || f.f0 <= 0.0 {
            continue;
        }

        // 音分域取平均 = Hz 域取几何平均。YIN 给的是整窗的平均周期。
        let mean_cents: f32 = win.iter().map(|v| v.log2()).sum::<f32>() / win.len() as f32;
        let reference = mean_cents.exp2();

        s.evaluated += 1;
        let e = cents(f.f0, reference);
        let ce = chroma_cents(f.f0, reference);
        if e.abs() <= 50.0 {
            rpa_hit += 1;
            errs.push(e.abs());
        }
        if ce.abs() <= 50.0 {
            rca_hit += 1;
            if e.abs() > 50.0 {
                oct += 1;
            }
        }
    }

    let ev = s.evaluated.max(1) as f32;
    s.rpa = rpa_hit as f32 / ev;
    s.rca = rca_hit as f32 / ev;
    s.octave_error = oct as f32 / ev;
    s.voicing_recall = recall_hit as f32 / s.voiced_truth.max(1) as f32;
    s.voicing_false_alarm = fa_hit as f32 / s.unvoiced_truth.max(1) as f32;

    errs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if !errs.is_empty() {
        s.median_cents = errs[errs.len() / 2];
        s.p90_cents = errs[(errs.len() * 9 / 10).min(errs.len() - 1)];
    }
    s
}

// ───────────────────────── 素材合成 ─────────────────────────

/// 三个共振峰的中心频率与带宽（Hz）。取的是中性元音 /ɑ/ 的典型值。
///
/// 用正弦叠加 + 幅度整形来近似，而不是真的过一遍滤波器：
/// 滤波器会引入相位延迟，而真值是按**瞬时相位**定义的 ——
/// 那点延迟会被当成检测误差记到账上，而它其实是素材自己的问题。
const FORMANTS: [(f32, f32); 3] = [(700.0, 110.0), (1220.0, 130.0), (2600.0, 180.0)];

fn formant_gain(hz: f32) -> f32 {
    let mut g = 0.05f32;
    for (f, bw) in FORMANTS {
        let d = (hz - f) / bw;
        g += 1.0 / (1.0 + d * d);
    }
    g
}

/// 合成一段人声样的素材。
///
/// 声源是 1/k 衰减的谐波堆（近似声门脉冲的频谱斜率），
/// 再按共振峰曲线整形。**不是纯正弦** —— 纯正弦对 YIN 太容易了，
/// 量出来的成绩会比真实情况好看得多，那种基线没有用。
///
/// `contour(t)` 返回该时刻的 f0，0 表示静音。
pub fn synth(
    contour: impl Fn(f32) -> f32,
    secs: f32,
    sample_rate: f32,
    level: f32,
    snr_db: Option<f32>,
) -> (Vec<f32>, Vec<f32>) {
    let n = (secs * sample_rate) as usize;
    let mut out = Vec::with_capacity(n);
    let mut truth = Vec::with_capacity(n);
    let mut phase = 0.0f64;
    // 包络跟着有声/无声走，避免起止的咔哒声被当成音高事件
    let mut env = 0.0f32;
    let attack = 1.0 - (-1.0 / (0.008 * sample_rate)).exp();

    for i in 0..n {
        let t = i as f32 / sample_rate;
        let f0 = contour(t);
        truth.push(f0);

        let target = if f0 > 0.0 { 1.0 } else { 0.0 };
        env += (target - env) * attack;

        if f0 > 0.0 {
            phase += (f0 as f64) * std::f64::consts::TAU / sample_rate as f64;
            if phase > std::f64::consts::TAU {
                phase -= std::f64::consts::TAU;
            }
        }

        let mut v = 0.0f32;
        if f0 > 0.0 {
            // 谐波数卡在 0.45×Nyquist 以内，别让混叠伪造出额外的周期性
            let kmax = ((0.45 * sample_rate / f0) as usize).clamp(1, 80);
            for k in 1..=kmax {
                let hz = f0 * k as f32;
                v += (phase as f32 * k as f32).sin() * formant_gain(hz) / k as f32;
            }
        }
        out.push(v * env * level * 0.25);
    }

    if let Some(snr) = snr_db {
        let sig: f32 = (out.iter().map(|v| v * v).sum::<f32>() / n.max(1) as f32).sqrt();
        let noise_rms = sig * 10f32.powf(-snr / 20.0);
        // 确定性伪随机：评测要能复现，不能每次跑出来的噪声都不一样
        let mut st = 0x2545F491_4F6CDD1Du64;
        for v in out.iter_mut() {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            let u = ((st >> 40) as f32 / 8_388_608.0) - 1.0;
            *v += u * noise_rms * 1.73;
        }
    }

    (out, truth)
}

/// 标准素材组。覆盖的是**不同的失败模式**，不是不同的旋律。
pub fn probes(sample_rate: f32) -> Vec<Probe> {
    let mut v = Vec::new();

    let mut add = |name, what, secs, level, snr, f: Box<dyn Fn(f32) -> f32>| {
        let (samples, truth) = synth(f, secs, sample_rate, level, snr);
        v.push(Probe { name, what, samples, sample_rate, truth, informational: false });
    };

    add("steady", "220 Hz 长音 —— 最容易的一档，这里都不准就别谈别的", 2.0, 1.0, None,
        Box::new(|_| 220.0));

    add("vibrato", "220 Hz ±50 音分 / 6 Hz 颤音 —— 修音必须保住它", 2.0, 1.0, None,
        Box::new(|t| 220.0 * (0.5 * (std::f32::consts::TAU * 6.0 * t).sin() / 12.0).exp2()));

    add("glide", "150→400 Hz 滑音 —— 考的是窗口内 f0 在变时还准不准", 2.0, 1.0, None,
        Box::new(|t| 150.0 * (400.0f32 / 150.0).powf(t / 2.0)));

    add("low", "85 Hz 男低音 —— 贴着 f0_min=70 的下沿", 1.5, 1.0, None,
        Box::new(|_| 85.0));

    add("high", "880 Hz 女高音 —— 周期只有 55 个样本", 1.5, 1.0, None,
        Box::new(|_| 880.0));

    // 八度跳进最容易诱发 YIN 的倍频/分频错误
    add("octaves", "165↔330↔660 Hz 八度跳进 —— 专钓倍频/分频错误", 3.0, 1.0, None,
        Box::new(|t| match (t * 2.0) as u32 % 3 {
            0 => 165.0,
            1 => 330.0,
            _ => 660.0,
        }));

    add("phrase", "带换气停顿的乐句 —— 考浊音判定，不只是音高", 3.0, 1.0, None,
        Box::new(|t| {
            let seg = (t / 0.6) as u32;
            if (t % 0.6) > 0.45 { return 0.0; } // 每句之间留 150 ms 静音
            [196.0, 220.0, 247.0, 262.0, 294.0][(seg % 5) as usize]
        }));

    add("noisy20", "乐句 + 20 dB 信噪比 —— 安静房间里的电容麦", 3.0, 1.0, Some(20.0),
        Box::new(|t| {
            let seg = (t / 0.6) as u32;
            if (t % 0.6) > 0.45 { return 0.0; }
            [196.0, 220.0, 247.0, 262.0, 294.0][(seg % 5) as usize]
        }));

    add("noisy6", "乐句 + 6 dB 信噪比 —— 有空调/风扇的房间", 3.0, 1.0, Some(6.0),
        Box::new(|t| {
            let seg = (t / 0.6) as u32;
            if (t % 0.6) > 0.45 { return 0.0; }
            [196.0, 220.0, 247.0, 262.0, 294.0][(seg % 5) as usize]
        }));

    // 次谐波回收最可能**帮倒忙**的两档：低音 + 噪声。
    // 低音的半周期本来就比较像一个周期，放宽标准很容易把 85 Hz 判成 170 Hz。
    add("low_noisy", "85 Hz 男低音 + 10 dB 信噪比 —— 最容易被误判成高八度", 2.0, 1.0, Some(10.0),
        Box::new(|_| 85.0));

    // 这一段是**边界标记**，不是要考的题：正确行为是判清音、什么都不报。
    add("noisy0", "乐句 + 0 dB 信噪比 —— 超出可用范围，正确行为是判清音", 3.0, 1.0, Some(0.0),
        Box::new(|t| {
            let seg = (t / 0.6) as u32;
            if (t % 0.6) > 0.45 { return 0.0; }
            [196.0, 220.0, 247.0, 262.0, 294.0][(seg % 5) as usize]
        }));

    add("quiet", "乐句 @ -34 dBFS —— 离麦远、不敢放开唱", 3.0, 0.02, None,
        Box::new(|t| {
            let seg = (t / 0.6) as u32;
            if (t % 0.6) > 0.45 { return 0.0; }
            [196.0, 220.0, 247.0, 262.0, 294.0][(seg % 5) as usize]
        }));

    for p in v.iter_mut() {
        if p.name == "noisy0" {
            p.informational = true;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    const WIN: usize = 1372;

    /// 把**真值本身**当成估计喂回去，必须满分。
    ///
    /// 不先验这一条的话，评测台自己错了也看不出来 ——
    /// 一个恒返回 0 分的台子和一个检测器很烂，长得一模一样。
    fn frames_from(truth: &[f32], hop: usize, map: impl Fn(f32) -> (f32, bool)) -> Vec<Frame> {
        (0..truth.len() / hop)
            .map(|i| {
                let end = i * hop + hop;
                let start = end.saturating_sub(WIN);
                let win = &truth[start..end.min(truth.len())];
                let voiced_n = win.iter().filter(|v| **v > 0.0).count();
                let mean = if voiced_n == win.len() && !win.is_empty() {
                    (win.iter().map(|v| v.log2()).sum::<f32>() / win.len() as f32).exp2()
                } else {
                    0.0
                };
                let (f0, voiced) = map(mean);
                Frame { window_end: end, f0, voiced }
            })
            .collect()
    }

    #[test]
    fn a_perfect_estimator_scores_perfectly() {
        let (_, truth) = synth(|_| 220.0, 1.0, SR, 1.0, None);
        let fr = frames_from(&truth, 128, |m| (m, m > 0.0));
        let s = score(&truth, &fr, WIN);
        assert!(s.evaluated > 100, "没有帧参与评测：{s:?}");
        assert_eq!(s.rpa, 1.0, "{s:?}");
        assert_eq!(s.rca, 1.0, "{s:?}");
        assert_eq!(s.octave_error, 0.0);
        assert_eq!(s.voicing_recall, 1.0);
    }

    /// 整条轨减半八度：RPA 必须归零，而 RCA 必须满分。
    ///
    /// 这是 RPA/RCA 这对指标存在的**全部理由** —— 它们分不开的话，
    /// 八度错误就会被平均进一个总准确率里看不见。
    #[test]
    fn octave_halving_shows_up_as_rca_minus_rpa() {
        let (_, truth) = synth(|_| 220.0, 1.0, SR, 1.0, None);
        let fr = frames_from(&truth, 128, |m| (m / 2.0, m > 0.0));
        let s = score(&truth, &fr, WIN);
        assert_eq!(s.rpa, 0.0, "减半了 RPA 却不是 0：{s:?}");
        assert_eq!(s.rca, 1.0, "音级没变，RCA 应当满分：{s:?}");
        assert!((s.octave_error - 1.0).abs() < 1e-6, "{s:?}");
    }

    /// 偏 30 音分仍算命中（阈值 50），偏 80 音分不算。
    #[test]
    fn the_fifty_cent_threshold_is_where_it_says() {
        let (_, truth) = synth(|_| 220.0, 1.0, SR, 1.0, None);
        for (off, want) in [(30.0f32, 1.0f32), (80.0, 0.0)] {
            let fr = frames_from(&truth, 128, |m| (m * (off / 1200.0).exp2(), m > 0.0));
            let s = score(&truth, &fr, WIN);
            assert_eq!(s.rpa, want, "偏 {off} 音分时 RPA={}", s.rpa);
        }
    }

    /// 全判无声：召回 0、虚警 0，而且音高指标没有样本可算。
    #[test]
    fn silence_everywhere_is_recall_zero_not_accuracy_zero() {
        let (_, truth) = synth(|_| 220.0, 1.0, SR, 1.0, None);
        let fr = frames_from(&truth, 128, |_| (0.0, false));
        let s = score(&truth, &fr, WIN);
        assert_eq!(s.voicing_recall, 0.0);
        assert_eq!(s.voicing_false_alarm, 0.0);
        assert_eq!(s.evaluated, 0, "没判出任何浊音，不该有帧参与音高评测");
    }

    /// 合成出来的素材得真的像人声：谐波要在，而且能量要落在共振峰附近。
    #[test]
    fn synth_is_not_a_bare_sine() {
        let (x, _) = synth(|_| 220.0, 0.5, SR, 1.0, None);
        let rms = (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
        assert!(rms > 0.01, "合成信号太弱：{rms}");

        // 半周期内的过零次数远多于 2，说明高次谐波确实在
        let zc = x.windows(2).filter(|w| w[0].signum() != w[1].signum()).count();
        let periods = 0.5 * 220.0;
        assert!(
            zc as f32 > periods * 4.0,
            "过零只有 {zc} 次，这还是条正弦（{periods} 个周期）"
        );
    }

    #[test]
    fn every_probe_has_truth_aligned_with_samples() {
        for p in probes(SR) {
            assert_eq!(p.samples.len(), p.truth.len(), "{} 真值与样本不等长", p.name);
            assert!(p.truth.iter().any(|v| *v > 0.0), "{} 全是静音", p.name);
        }
    }
}
