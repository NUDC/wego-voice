//! 参考音频的声线指纹，以及"从我的声音到它，要拧多少"。
//!
//! # 这一步在整条链路里的位置
//!
//! 用户的诉求是"放一段某人的音频，我就唱成那个声线"。不管最终用不用
//! 神经网络，这一步都跑不掉：**得先量出参考音源的声线长什么样**。
//!
//! 所以它既是纯 DSP 方案的全部，也是神经方案的前置与兜底初值。
//!
//! # 量什么：谱包络的整体缩放，而不是 F1/F2/F3
//!
//! 常规做法是 LPC 求根、逐个提取共振峰。这里**刻意不那么做** ——
//! 我们的 DSP 能施加的就只有一个参数：共振峰整体缩放系数 α
//! （见 `psola::set_formant`）。逐个提取 F1/F2/F3 再想办法塞进一个 α，
//! 是先制造信息再丢掉信息。
//!
//! 直接量"整体缩放"更诚实也更稳：
//!
//! ```text
//! α* = argmin Σ | E_参考(f) − E_我(f/α) |²
//! ```
//!
//! # 为什么用对数频率轴
//!
//! 共振峰按 α 缩放，在**对数频率轴上就是一次平移**。于是上面那个优化问题
//! 退化成"两条曲线错开多少格最像" —— 一次互相关就出来了，不用搜索、不用求根。
//!
//! 顺带两个好处：对数轴本来就更接近人耳的频率分辨；不同采样率的素材
//! 落在同一套 Hz 栅格上，可以直接比。
//!
//! # 为什么音高默认不动
//!
//! 参考音源比你高一个八度，不代表你该升一个八度去唱 —— **那就不是这首歌了**。
//! 而且 PSOLA 超过 ±5 个半音会有明显金属感。
//!
//! "像另一个人"这件事主要由共振峰承担，音高不承担。所以这里把测到的音高差
//! 作为**信息**报出来，但建议值给 0，让用户自己决定要不要动。

use crate::fft::{Fft, C};
use crate::yin::{Yin, YinConfig};

/// 包络栅格的下限（Hz）。
///
/// 再往下是基频与其前几次谐波的地盘，那里反映的是"唱的什么音"，
/// 不是"谁在唱"。放进来只会让不同音高的素材算出假的差异。
const GRID_LO_HZ: f32 = 200.0;

/// 包络栅格的上限（Hz）。
///
/// 6kHz 以上主要是齿音与麦克风自身的高频响应，设备差异比人声差异还大。
const GRID_HI_HZ: f32 = 6000.0;

/// 每个八度多少格。24 格/八度 = 每格半个半音，也就是搜索分辨率。
const BINS_PER_OCT: usize = 24;

/// 分析窗长度（样本）。
///
/// 2048 @48kHz ≈ 43ms：足够长到能分辨共振峰，又足够短到音色还没变。
const FRAME: usize = 2048;

/// 倒谱保留的低 quefrency 系数个数。
///
/// 这一步是把"谐波梳"滤掉、只留下频谱包络。两侧都有陷阱：
///
/// - **留太多** → 谐波结构漏进来，测的就成了音高而不是音色
/// - **留太少** → 包络糊成一条平滑曲线，共振峰互相抹平
///
/// 换算关系：保留 q 个系数 ⇒ 能分辨间隔 ≥ `SR/q` Hz 的频谱结构；
/// 而 f0 的谐波梳住在 `q = SR/f0`。所以要求 `KEEP < SR/f0_max`。
///
/// 第一版取 40（@48kHz ⇒ 只能分辨 1200 Hz 间隔），而人声共振峰常常只隔
/// 500 Hz —— 被抹平之后合成素材实测只量回真值的一半（+2.0 vs 真值 +3.9）。
/// 取 64 可分辨 750 Hz，同时对 f0 ≤ 750 Hz 仍然安全（唱歌基本够用）。
const CEPSTRUM_KEEP: usize = 64;

/// 素材合格线：至少要有这么多秒的浊音。
///
/// 低于这条线算出来的东西没有意义，**必须明确拒绝而不是给个数字** ——
/// 用户拿到一个悄悄不准的结果，只会怪工具。
pub const MIN_VOICED_SECS: f32 = 1.5;

/// 一段音频的声线指纹。
#[derive(Debug, Clone)]
pub struct TimbreProfile {
    /// 浊音段基频中位数（Hz）。0 表示没测到。
    pub median_f0: f32,
    /// 基频的 10% / 90% 分位（Hz），用来描述音域。
    pub f0_low: f32,
    pub f0_high: f32,
    /// 浊音时长（秒）。低于 [`MIN_VOICED_SECS`] 时结果不可信。
    pub voiced_secs: f32,
    /// 总时长（秒）。
    pub duration_secs: f32,
    /// 对数频率栅格上的谱包络（dB，已去均值）。
    pub envelope: Vec<f32>,
    /// 频谱倾斜（dB/八度）。正 = 更明亮。
    pub tilt_db_per_oct: f32,
}

impl TimbreProfile {
    /// 素材够不够格。不够的话不要用它的数字。
    pub fn is_usable(&self) -> bool {
        self.voiced_secs >= MIN_VOICED_SECS && self.median_f0 > 0.0
    }
}

/// 从"我的声音"到"参考声线"需要拧多少。
#[derive(Debug, Clone, Copy)]
pub struct TimbreMatch {
    /// 建议的共振峰平移（半音）。**这是声线的主维度。**
    pub formant_shift: f32,
    /// 建议的整体移调（半音）。**默认 0** —— 见模块文档。
    pub pitch_shift: f32,
    /// 实测的音高差（半音），仅供参考。正 = 参考音源更高。
    pub pitch_delta: f32,
    /// 匹配置信度 0~1。低于 0.4 时应当在 UI 上明确说"没把握"。
    pub confidence: f32,
    /// 频谱倾斜差（dB/八度）。正 = 参考更明亮。
    ///
    /// ⚠️ **当前引擎补不了这个差异**（没有 EQ 环节）。
    /// 报出来是为了诚实说明"还差在哪"，不是已实现的功能。
    pub tilt_delta: f32,
}

/// 栅格格数。
fn grid_len() -> usize {
    ((GRID_HI_HZ / GRID_LO_HZ).log2() * BINS_PER_OCT as f32).round() as usize + 1
}

/// 第 `i` 格对应的频率（Hz）。
fn grid_hz(i: usize) -> f32 {
    GRID_LO_HZ * (i as f32 / BINS_PER_OCT as f32).exp2()
}

/// 分析一段音频，得到声线指纹。
///
/// `samples` 为单声道。任意采样率均可 —— 栅格按 Hz 定义，跨采样率可比。
pub fn analyze(samples: &[f32], sample_rate: f32) -> TimbreProfile {
    let n = grid_len();
    let mut env_sum = vec![0.0f64; n];
    let mut env_frames = 0usize;

    let hop = FRAME / 2;
    let fft = Fft::new(FRAME);
    let mut yin = Yin::new(YinConfig { sample_rate, ..Default::default() });

    let mut f0s: Vec<f32> = Vec::new();
    let mut buf = vec![C::default(); FRAME];
    let mut ceps = vec![C::default(); FRAME];

    // 汉宁窗预算好，逐帧复用
    let win: Vec<f32> = (0..FRAME)
        .map(|i| 0.5 * (1.0 - (std::f32::consts::TAU * i as f32 / (FRAME - 1) as f32).cos()))
        .collect();

    let mut pos = 0usize;
    while pos + FRAME <= samples.len() {
        let frame = &samples[pos..pos + FRAME];
        pos += hop;

        // 只统计浊音帧：清音与静音的谱包络反映的是噪声与设备，不是声线
        let est = yin.analyze(frame);
        if !est.is_voiced || est.f0_hz <= 0.0 {
            continue;
        }
        f0s.push(est.f0_hz);

        // --- 倒谱平滑求谱包络 ---
        //
        // 直接用 FFT 幅度是不行的：那上面全是谐波梳齿，测出来的是
        // "唱的什么音"而不是"谁在唱"。倒谱域里谐波结构住在高 quefrency，
        // 包络住在低 quefrency，截断一下就分开了。
        for (i, slot) in buf.iter_mut().enumerate() {
            *slot = C::new(frame[i] * win[i], 0.0);
        }
        fft.forward(&mut buf);

        for slot in buf.iter_mut() {
            let mag = (slot.re * slot.re + slot.im * slot.im).sqrt();
            *slot = C::new(mag.max(1e-9).ln(), 0.0);
        }
        ceps.copy_from_slice(&buf);
        fft.inverse(&mut ceps);
        // 截断：只留低 quefrency（两端都要留，倒谱是对称的）
        for (q, slot) in ceps.iter_mut().enumerate() {
            if q >= CEPSTRUM_KEEP && q < FRAME - CEPSTRUM_KEEP {
                *slot = C::default();
            }
        }
        fft.forward(&mut ceps);

        // 采样到对数频率栅格上（线性插值），单位换成 dB
        for (i, acc) in env_sum.iter_mut().enumerate() {
            let hz = grid_hz(i);
            let bin = hz * FRAME as f32 / sample_rate;
            if bin >= (FRAME / 2 - 1) as f32 {
                continue;
            }
            let b0 = bin.floor() as usize;
            let t = bin - b0 as f32;
            let v = ceps[b0].re * (1.0 - t) + ceps[b0 + 1].re * t;
            // 自然对数 → dB
            *acc += (v * 20.0 / std::f32::consts::LN_10) as f64;
        }
        env_frames += 1;
    }

    let voiced_secs = env_frames as f32 * hop as f32 / sample_rate;
    let duration_secs = samples.len() as f32 / sample_rate;

    let mut envelope = vec![0.0f32; n];
    if env_frames > 0 {
        for (i, acc) in env_sum.iter().enumerate() {
            envelope[i] = (*acc / env_frames as f64) as f32;
        }
        // 去均值：只关心形状，不关心录音电平
        let mean = envelope.iter().sum::<f32>() / n as f32;
        for v in envelope.iter_mut() {
            *v -= mean;
        }
    }

    f0s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pick = |p: f32| -> f32 {
        if f0s.is_empty() {
            0.0
        } else {
            f0s[((f0s.len() - 1) as f32 * p) as usize]
        }
    };

    TimbreProfile {
        median_f0: pick(0.5),
        f0_low: pick(0.1),
        f0_high: pick(0.9),
        voiced_secs,
        duration_secs,
        tilt_db_per_oct: tilt(&envelope),
        envelope,
    }
}

/// 谱包络的整体倾斜（dB/八度），最小二乘拟合。
fn tilt(env: &[f32]) -> f32 {
    if env.len() < 2 {
        return 0.0;
    }
    // x 轴用八度数
    let n = env.len() as f32;
    let xs: Vec<f32> = (0..env.len())
        .map(|i| i as f32 / BINS_PER_OCT as f32)
        .collect();
    let mx = xs.iter().sum::<f32>() / n;
    let my = env.iter().sum::<f32>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for (x, y) in xs.iter().zip(env.iter()) {
        num += (x - mx) * (y - my);
        den += (x - mx) * (x - mx);
    }
    if den.abs() < 1e-12 {
        0.0
    } else {
        num / den
    }
}

/// 求"从 `src` 到 `target` 要拧多少"。
///
/// # 已知偏差：**估计偏保守**
///
/// 合成素材实测（共振峰比 1.25，真值 +3.86 半音）量回 **+3.0** ——
/// 方向与量级正确，但系统性偏小约 20%。
///
/// 原因是固有的：包络里 F1 以下和 F3 以上那些区域**不随 α 缩放**
/// （它们是整个级联的渐近斜率，以及麦克风自身的频响），
/// 却同样参与了误差求和，于是把估计往 0 拽。
///
/// 没有为此调参数 —— 那是对单个合成样本过拟合。它的定位是
/// **一个靠谱的起点**，用户再用滑杆凭耳朵微调。UI 上要如实这么说。
///
/// 共振峰按 α 缩放在对数频率轴上就是平移，所以这里只是找
/// **错开多少格时两条包络最像** —— 一次互相关，没有迭代优化。
pub fn match_to(src: &TimbreProfile, target: &TimbreProfile) -> TimbreMatch {
    // ±8 个半音 = ±16 格。超出这个范围 PSOLA 本身也撑不住了
    const MAX_SHIFT_BINS: i32 = 16;

    let n = src.envelope.len().min(target.envelope.len());

    // ⚠️ **不要在这里做去趋势。**
    //
    // 曾经试过：想法是"倾斜差平移消不掉，先减掉它只比形状"。
    // 但两条曲线的倾斜不同，各减各的直线会把峰位**朝不同方向扭** ——
    // 合成素材上实测把 +3.9 的真值直接压成了 0.0，比不减还差。
    //
    // 倾斜差归 `tilt_delta` 单独上报，不参与匹配。
    let a = &src.envelope[..n];
    let b = &target.envelope[..n];
    let mut best_shift = 0i32;
    let mut best_err = f32::INFINITY;
    let mut errs: Vec<f32> = Vec::with_capacity((MAX_SHIFT_BINS * 2 + 1) as usize);

    for s in -MAX_SHIFT_BINS..=MAX_SHIFT_BINS {
        let mut sum = 0.0f32;
        let mut cnt = 0usize;
        for i in 0..n {
            let j = i as i32 - s;
            if j < 0 || j as usize >= n {
                continue;
            }
            let d = b[i] - a[j as usize];
            sum += d * d;
            cnt += 1;
        }
        // 重叠区太小时这个位移没有意义，别让它赢
        let err = if cnt < n / 2 { f32::INFINITY } else { sum / cnt as f32 };
        errs.push(err);
        if err < best_err {
            best_err = err;
            best_shift = s;
        }
    }

    // 置信度：最优点比"典型位移"好多少。
    //
    // 只看残差绝对值是不行的 —— 两段本来就相似的素材残差天然就小，
    // 而那正是"移不移都差不多"、最没把握的情况。
    let finite: Vec<f32> = errs.iter().copied().filter(|e| e.is_finite()).collect();
    let median_err = {
        let mut v = finite.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v.get(v.len() / 2).copied().unwrap_or(1.0)
    };
    let sharpness = if median_err > 1e-6 {
        (1.0 - best_err / median_err).clamp(0.0, 1.0)
    } else {
        0.0
    };
    // 素材长度也参与：1.5 秒浊音勉强够用，5 秒以上才算充分
    let material = (src.voiced_secs.min(target.voiced_secs) / 5.0).clamp(0.0, 1.0);
    let confidence = if src.is_usable() && target.is_usable() {
        (sharpness * 0.7 + material * 0.3).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let pitch_delta = if src.median_f0 > 0.0 && target.median_f0 > 0.0 {
        12.0 * (target.median_f0 / src.median_f0).log2()
    } else {
        0.0
    };

    // 抛物线插值取亚格精度。
    //
    // 栅格是 0.5 半音一格，直接取整会带来最多 ±0.25 半音的量化偏差；
    // 而真实的 α 不会正好落在格点上。用最优点及其左右邻居拟合抛物线、
    // 取顶点，是标准做法，几乎零成本。
    let refined = {
        let i = (best_shift + MAX_SHIFT_BINS) as usize;
        if i > 0 && i + 1 < errs.len() && errs[i - 1].is_finite() && errs[i + 1].is_finite() {
            let (y0, y1, y2) = (errs[i - 1], errs[i], errs[i + 1]);
            let den = y0 - 2.0 * y1 + y2;
            // den <= 0 说明这不是个极小点（数值噪声），别信插值
            if den > 1e-9 {
                let delta = 0.5 * (y0 - y2) / den;
                best_shift as f32 + delta.clamp(-1.0, 1.0)
            } else {
                best_shift as f32
            }
        } else {
            best_shift as f32
        }
    };

    TimbreMatch {
        // 格 → 半音：每格半个半音
        formant_shift: refined * 12.0 / BINS_PER_OCT as f32,
        // 刻意为 0，见模块文档："像另一个人"由共振峰承担，不由音高承担
        pitch_shift: 0.0,
        pitch_delta,
        confidence,
        tilt_delta: target.tilt_db_per_oct - src.tilt_db_per_oct,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::psola::{Psola, PsolaConfig};
    use std::f32::consts::TAU;

    const SR: f32 = 48_000.0;

    /// 冲激串过若干个二阶谐振器 —— 有基频，也有明确的共振峰结构。
    fn voiced(f0: f32, formants: &[f32], secs: f32) -> Vec<f32> {
        let n = (secs * SR) as usize;
        let period = (SR / f0).round().max(2.0) as usize;
        let mut x: Vec<f32> = (0..n)
            .map(|i| if i % period == 0 { 1.0 } else { 0.0 })
            .collect();
        for &f in formants {
            // 真实人声共振峰带宽 50~150 Hz ⇒ r = exp(-πB/SR)。
            // 早期版本用 0.94（带宽约 945 Hz！）——三个峰被自己的带宽糊成一个包，
            // 于是"算法测不准"其实是**测试信号不合格**。
            let r = (-std::f32::consts::PI * 110.0 / SR).exp();
            let theta = TAU * f / SR;
            let (a1, a2) = (2.0 * r * theta.cos(), -r * r);
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

    /// 用我们自己的 PSOLA 施加共振峰平移，得到"同一个人被移过的版本"。
    fn formant_shifted(input: &[f32], f0: f32, semis: f32) -> Vec<f32> {
        let mut p = Psola::new(PsolaConfig {
            sample_rate: SR,
            latency_f0_floor: 100.0,
            f0_max: 1100.0,
        });
        p.set_pitch(f0, true);
        p.set_ratio(1.0);
        p.set_formant((semis / 12.0).exp2());
        let mut out = vec![0.0; input.len()];
        let mut buf = vec![0.0; 256];
        for (i, chunk) in input.chunks(256).enumerate() {
            let k = chunk.len();
            p.process(chunk, &mut buf[..k]);
            out[i * 256..i * 256 + k].copy_from_slice(&buf[..k]);
        }
        out
    }

    /// 合成素材上的端到端精度：共振峰比 1.25 ⇒ 真值 +3.86 半音。
    ///
    /// 这条同时锁住**两件事**：数值在可用范围内，以及**正反向对称** ——
    /// 反过来匹配必须给出大小相当、符号相反的结果，
    /// 否则说明匹配里混进了与方向有关的偏置。
    #[test]
    fn estimates_a_known_formant_ratio() {
        let mine = analyze(&voiced(120.0, &[600.0, 1100.0, 2400.0], 5.0), SR);
        let other = analyze(&voiced(200.0, &[750.0, 1400.0, 3000.0], 5.0), SR);

        let fwd = match_to(&mine, &other).formant_shift;
        let back = match_to(&other, &mine).formant_shift;

        // 真值 +3.86，实测 +3.0（见 `match_to` 的"已知偏差"说明）
        assert!(
            (2.0..=4.5).contains(&fwd),
            "共振峰比 1.25（真值 +3.86）量回 {fwd:+.2} 半音"
        );
        assert!(
            (fwd + back).abs() < 1.0,
            "正反向不对称：{fwd:+.2} vs {back:+.2}"
        );
    }

    /// **这条是整个模块的正确性支点。**
    ///
    /// 拿一段声音，用我们自己的 DSP 把共振峰移 +3 个半音，
    /// 然后让分析器去量 —— 它必须量回 +3。
    ///
    /// 量不回来就说明「分析」和「施加」两侧对 α 的定义不一致，
    /// 那样用户点"从参考音频生成"会得到一个方向或幅度都错的结果。
    #[test]
    fn recovers_a_shift_that_we_applied_ourselves() {
        let src = voiced(160.0, &[700.0, 1200.0, 2600.0], 4.0);

        for &applied in &[3.0f32, -3.0, 1.5] {
            let tgt = formant_shifted(&src, 160.0, applied);
            // 跳过 PSOLA 的预热段
            let a = analyze(&src[12_000..], SR);
            let b = analyze(&tgt[12_000..], SR);
            let m = match_to(&a, &b);

            assert!(
                (m.formant_shift - applied).abs() <= 1.0,
                "施加 {applied:+} 半音，量回 {:+.1} 半音",
                m.formant_shift
            );
        }
    }

    /// 同一段素材跟自己比，必须是 0 —— 否则会凭空给用户一个偏移。
    #[test]
    fn identical_material_needs_no_shift() {
        let x = voiced(180.0, &[650.0, 1100.0, 2500.0], 4.0);
        let p = analyze(&x, SR);
        let m = match_to(&p, &p);
        assert_eq!(m.formant_shift, 0.0);
        assert!(m.pitch_delta.abs() < 0.01);
    }

    /// 共振峰整体更高的音源，应当得到正的平移值（"更细/更小"的方向）。
    #[test]
    fn a_shorter_vocal_tract_gives_a_positive_shift() {
        let male = analyze(&voiced(120.0, &[600.0, 1100.0, 2400.0], 4.0), SR);
        let female = analyze(&voiced(120.0, &[750.0, 1400.0, 3000.0], 4.0), SR);
        let m = match_to(&male, &female);
        assert!(
            m.formant_shift > 1.5,
            "共振峰整体上移的音源只算出 {:+.1} 半音",
            m.formant_shift
        );
    }

    /// 音高差**不得**污染共振峰估计。
    ///
    /// 同一个"人"（同一组共振峰）唱高八度，声线没变，
    /// 所以 formant_shift 应当接近 0 —— 倒谱平滑就是为这条服务的。
    #[test]
    fn pitch_does_not_leak_into_the_formant_estimate() {
        let low = analyze(&voiced(110.0, &[700.0, 1200.0, 2600.0], 4.0), SR);
        let high = analyze(&voiced(220.0, &[700.0, 1200.0, 2600.0], 4.0), SR);
        let m = match_to(&low, &high);
        assert!(
            m.formant_shift.abs() <= 1.0,
            "音高翻倍导致共振峰估计偏了 {:+.1} 半音",
            m.formant_shift
        );
        // 但音高差本身要如实报出来
        assert!(
            (m.pitch_delta - 12.0).abs() < 1.0,
            "音高差报成了 {:+.1} 半音，期望 +12",
            m.pitch_delta
        );
    }

    /// 音高建议值恒为 0 —— 改了就不是这首歌了。
    #[test]
    fn pitch_shift_suggestion_is_always_zero() {
        let a = analyze(&voiced(110.0, &[700.0, 1200.0, 2600.0], 3.0), SR);
        let b = analyze(&voiced(260.0, &[800.0, 1500.0, 3100.0], 3.0), SR);
        assert_eq!(match_to(&a, &b).pitch_shift, 0.0);
    }

    /// 素材太短必须被判为不可用，而不是给一个悄悄不准的数字。
    #[test]
    fn short_material_is_rejected() {
        let p = analyze(&voiced(180.0, &[700.0, 1200.0], 0.4), SR);
        assert!(!p.is_usable(), "0.4 秒的素材不该被判为可用");
        let good = analyze(&voiced(180.0, &[700.0, 1200.0], 4.0), SR);
        assert!(good.is_usable());
        assert_eq!(match_to(&p, &good).confidence, 0.0, "不可用素材必须零置信度");
    }

    /// 静音/噪声不能被当成人声分析。
    #[test]
    fn silence_is_not_usable() {
        let p = analyze(&vec![0.0; 48_000 * 3], SR);
        assert!(!p.is_usable());
        assert_eq!(p.median_f0, 0.0);
    }

    #[test]
    fn brighter_material_reports_positive_tilt_delta() {
        let dark = analyze(&voiced(150.0, &[500.0, 800.0, 1200.0], 4.0), SR);
        let bright = analyze(&voiced(150.0, &[500.0, 2500.0, 4500.0], 4.0), SR);
        assert!(match_to(&dark, &bright).tilt_delta > 0.0);
    }
}
