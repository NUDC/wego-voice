//! 离线 f0 轨提取与后处理。
//!
//! # 为什么单独有一套离线的
//!
//! 实时链路被 30 ms 预算捆着手脚，三件事做不了：
//!
//! - **不能回看**。第 n 帧只能用前面的信息判断，而"这一帧到底是 220 Hz
//!   还是 110 Hz"往往要看它后面几帧才说得清。
//! - **f0 只能因果**。中值滤波、八度纠错这类需要邻域的操作全都用不上。
//! - **PSOLA 窗口被 `f0_floor` 截断**（低音区拿音质换延迟）。
//!
//! 离线重跑这三条限制一条都没有。同一段干声，离线版的音高轨明显更干净。
//!
//! # 它同时是声线转换的前置
//!
//! DDSP-SVC 这类模型的输入就是「干声 + f0 轨 + 内容特征」。
//! 所以这个模块不是只为离线校准服务的 —— f0 轨这一项是两条路共用的。
//! 先把它做扎实，后面接模型时少一个变量。
//!
//! # 三步后处理，顺序不能换
//!
//! 1. **八度纠错** —— YIN 的经典失败模式是整帧翻倍/减半。必须先修它，
//!    因为一个 2× 的离群值会把后面的中值和统计全带偏。
//! 2. **中值滤波** —— 去掉孤立毛刺。放在纠错之后，否则中值本身就是错的。
//! 3. **补洞** —— 一个音中间掉一两帧通常是检测失败，不是真的断了。
//!    放最后，因为它依赖前两步已经把值修对。

use crate::yin::{Yin, YinConfig};

/// 分析步进（样本）。
///
/// 128 @48kHz ≈ 2.7 ms —— 比实时链路的 256 更密。
/// 离线不缺 CPU，而密一倍能把颤音的形状描得更准。
pub const HOP: usize = 128;

/// 中值滤波窗口（帧数，取奇数）。
///
/// 5 帧 ≈ 13 ms。再宽就会开始啃颤音了 —— 人声颤音典型 5~7 Hz，
/// 半周期约 70~100 ms，13 ms 的窗口动不了它。
const MEDIAN_WIDTH: usize = 5;

/// 允许被填补的最大空洞（帧数）。
///
/// 8 帧 ≈ 21 ms。比这更长的静音大概率是真的换气或断句，
/// 硬填会把两个音之间接出一条不存在的滑音。
const MAX_GAP: usize = 8;

/// 一帧的音高信息。
#[derive(Debug, Clone, Copy, Default)]
pub struct PitchFrame {
    /// 基频（Hz）。`voiced == false` 时为 0。
    pub f0: f32,
    pub voiced: bool,
    /// YIN 的非周期度，越小越确信。
    pub aperiodicity: f32,
    pub rms: f32,
}

/// 整段音频的音高轨。
#[derive(Debug, Clone)]
pub struct PitchTrack {
    pub frames: Vec<PitchFrame>,
    pub hop: usize,
    pub sample_rate: f32,
    /// 后处理修正了多少帧 —— 报出来才知道这一步值不值。
    pub octave_fixes: usize,
    pub gap_fills: usize,
}

impl PitchTrack {
    pub fn voiced_count(&self) -> usize {
        self.frames.iter().filter(|f| f.voiced).count()
    }

    /// 浊音帧的 f0 中位数（Hz）。0 表示没有浊音。
    pub fn median_f0(&self) -> f32 {
        let mut v: Vec<f32> = self.frames.iter().filter(|f| f.voiced).map(|f| f.f0).collect();
        if v.is_empty() {
            return 0.0;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v[v.len() / 2]
    }

    /// 第 `i` 帧对应的样本位置。
    pub fn frame_pos(&self, i: usize) -> usize {
        i * self.hop
    }
}

/// 提取音高轨。`progress` 收到 0~1，用于 UI；返回 false 表示用户取消。
///
/// 取消不是可选功能：一首五分钟的歌有上万帧，用户点了停就得停。
pub fn track_pitch(
    samples: &[f32],
    sample_rate: f32,
    progress: impl FnMut(f32) -> bool,
) -> Option<PitchTrack> {
    let mut track = track_pitch_raw(samples, sample_rate, progress)?;
    track.octave_fixes = fix_octaves(&mut track.frames);
    median_filter(&mut track.frames);
    track.gap_fills = fill_gaps(&mut track.frames);
    Some(track)
}

/// 只做逐帧检测，**不做任何后处理**。
///
/// 单独留一个口子是为了能量出"后处理到底值多少" ——
/// 八度纠错、中值滤波、补洞这三步的收益，只有拿同一段素材
/// 跑两遍（有/无后处理）对比才说得清。见 `wego-bench f0`。
pub fn track_pitch_raw(
    samples: &[f32],
    sample_rate: f32,
    mut progress: impl FnMut(f32) -> bool,
) -> Option<PitchTrack> {
    let mut yin = Yin::new(YinConfig { sample_rate, ..Default::default() });
    let win = yin.required_len();
    let n_frames = samples.len() / HOP;
    let mut frames = Vec::with_capacity(n_frames);

    // 分析窗以当前位置为**右端**，和实时链路一致 ——
    // 这样同一段音频两条路算出来的 f0 是可比的，调试时不会因为
    // 对齐方式不同而看出假的差异。
    let mut buf = vec![0.0f32; win];
    for i in 0..n_frames {
        let end = (i * HOP + HOP).min(samples.len());
        let start = end.saturating_sub(win);
        let have = end - start;
        buf[..win - have].fill(0.0);
        buf[win - have..].copy_from_slice(&samples[start..end]);

        let est = yin.analyze(&buf);
        let rms = (buf.iter().map(|s| s * s).sum::<f32>() / win as f32).sqrt();
        frames.push(PitchFrame {
            f0: if est.is_voiced { est.f0_hz } else { 0.0 },
            voiced: est.is_voiced,
            aperiodicity: est.aperiodicity,
            rms,
        });

        // 每 64 帧汇报一次。每帧都报会让 IPC 比 DSP 还忙。
        if i % 64 == 0 && !progress(i as f32 / n_frames.max(1) as f32) {
            return None;
        }
    }

    progress(1.0);
    Some(PitchTrack {
        frames,
        hop: HOP,
        sample_rate,
        octave_fixes: 0,
        gap_fills: 0,
    })
}

/// 分析窗长度（样本）。评测时真值要在同样的窗口上取平均才可比。
pub fn analysis_window(sample_rate: f32) -> usize {
    Yin::new(YinConfig { sample_rate, ..Default::default() }).required_len()
}

/// 八度纠错。
///
/// YIN 最典型的失败是整帧翻倍或减半（自相关在 2τ 处也有峰）。
/// 判据：这一帧与**邻域中位数**差了接近整数个八度，就把它拉回来。
///
/// 用邻域而不是全局中位数：一首歌里音高会走很远，全局中位数对
/// 高音段和低音段都不合适，会把正确的高音误判成八度错误。
fn fix_octaves(frames: &mut [PitchFrame]) -> usize {
    const NEIGH: usize = 12; // ±12 帧 ≈ ±32 ms
    let orig: Vec<f32> = frames.iter().map(|f| if f.voiced { f.f0 } else { 0.0 }).collect();
    let mut fixed = 0;

    for i in 0..frames.len() {
        if !frames[i].voiced || orig[i] <= 0.0 {
            continue;
        }
        let lo = i.saturating_sub(NEIGH);
        let hi = (i + NEIGH + 1).min(orig.len());
        let mut neigh: Vec<f32> = orig[lo..hi]
            .iter()
            .enumerate()
            .filter(|(k, v)| lo + k != i && **v > 0.0)
            .map(|(_, v)| *v)
            .collect();
        if neigh.len() < 4 {
            continue;
        }
        neigh.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med = neigh[neigh.len() / 2];

        // 与中位数相差几个八度（可为负）
        let oct = (orig[i] / med).log2();
        let rounded = oct.round();
        // 差得接近整数个八度、且确实不是同一个八度 → 判定为八度错误
        if rounded != 0.0 && rounded.abs() <= 2.0 && (oct - rounded).abs() < 0.12 {
            frames[i].f0 = orig[i] / (rounded).exp2();
            fixed += 1;
        }
    }
    fixed
}

/// 中值滤波，只作用于浊音帧。
///
/// 只改 f0，不改清浊判定 —— 那一步交给 `fill_gaps`，两件事混在一起
/// 会让"到底是谁改的"变得不可追。
fn median_filter(frames: &mut [PitchFrame]) {
    let half = MEDIAN_WIDTH / 2;
    let orig: Vec<f32> = frames.iter().map(|f| if f.voiced { f.f0 } else { 0.0 }).collect();
    let mut win: Vec<f32> = Vec::with_capacity(MEDIAN_WIDTH);

    for i in 0..frames.len() {
        if !frames[i].voiced {
            continue;
        }
        win.clear();
        let lo = i.saturating_sub(half);
        let hi = (i + half + 1).min(orig.len());
        win.extend(orig[lo..hi].iter().copied().filter(|v| *v > 0.0));
        if win.len() < 3 {
            continue;
        }
        win.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        frames[i].f0 = win[win.len() / 2];
    }
}

/// 补洞：浊音段中间的短暂失检，按两端线性插值填上。
///
/// 只填**被浊音夹住**的洞。开头和结尾的清音是真的清音，填了等于凭空
/// 造出一个不存在的音。
fn fill_gaps(frames: &mut [PitchFrame]) -> usize {
    let mut filled = 0;
    let mut i = 0;
    while i < frames.len() {
        if frames[i].voiced {
            i += 1;
            continue;
        }
        let start = i;
        while i < frames.len() && !frames[i].voiced {
            i += 1;
        }
        let len = i - start;
        // 必须两端都有浊音，否则不是"洞"而是边界
        if start == 0 || i >= frames.len() || len > MAX_GAP {
            continue;
        }
        let a = frames[start - 1].f0;
        let b = frames[i].f0;
        if a <= 0.0 || b <= 0.0 {
            continue;
        }
        // 在对数域插值：音高的感知是对数的，线性插值在大跨度时会跑偏
        let (la, lb) = (a.ln(), b.ln());
        for (k, f) in frames[start..i].iter_mut().enumerate() {
            let t = (k + 1) as f32 / (len + 1) as f32;
            f.f0 = (la + (lb - la) * t).exp();
            f.voiced = true;
        }
        filled += len;
    }
    filled
}


// ─────────────────────── 离线重新校准 ───────────────────────

/// 离线校准参数。
#[derive(Debug, Clone, Copy)]
pub struct RecorrectConfig {
    pub key: crate::Key,
    /// 修正速度（毫秒）。0 = 立即吸附。
    pub retune_ms: f32,
    /// "意图音高"跟踪的时间常数（毫秒）。**量化只对它做。**
    ///
    /// 这是保留颤音的机制，也是不在音阶边界上抖的机制 ——
    /// 两件事由同一个参数负责，而它必须比 `retune_ms` 慢得多。
    ///
    /// 第一版把它和 `retune_ms` 合成了一个，结果：一个低 45 音分的 C4
    /// 正好落在 B3/C4 的判定边界（59.55 半音，边界 59.5），
    /// ±35 音分的颤音反复跨过去，量化目标在两个音之间抖，
    /// 实测只修回 7 音分（其余三个音都修到 ±1.5 以内）。
    pub intent_ms: f32,
    /// 角色的整体移调（半音）。
    pub pitch_shift: f32,
    /// 角色的共振峰平移（半音）。
    pub formant_shift: f32,
}

impl Default for RecorrectConfig {
    fn default() -> Self {
        Self {
            key: crate::Key::default(),
            retune_ms: 40.0,
            intent_ms: 150.0,
            pitch_shift: 0.0,
            formant_shift: 0.0,
        }
    }
}

/// 用已经整理好的音高轨重新校准整段音频。
///
/// # 和实时版的三处不同
///
/// 1. **音高轨是后处理过的** —— 八度纠错、中值滤波、补洞都做完了。
///    实时版拿到的是逐帧的生数据。
/// 2. **平滑是零相位的** —— 前向平滑一遍再反向平滑一遍，两次的滞后
///    正好抵消。实时版只能单向，修正必然慢半拍，起音处尤其明显。
/// 3. **PSOLA 不再被 `f0_floor` 截断** —— 那个截断是拿低音音质换延迟的，
///    离线没有延迟预算，直接按真实周期走。
pub fn recorrect(
    samples: &[f32],
    sample_rate: f32,
    track: &PitchTrack,
    cfg: RecorrectConfig,
    mut progress: impl FnMut(f32) -> bool,
) -> Option<Vec<f32>> {
    use crate::psola::{Psola, PsolaConfig};

    // 取实际用到的最低音做窗口下限，而不是写死的 f0_floor。
    // 留 0.9 的余量防止边界帧刚好卡在等号上。
    let lowest = track
        .frames
        .iter()
        .filter(|f| f.voiced && f.f0 > 0.0)
        .fold(f32::MAX, |m, f| m.min(f.f0));
    let floor = if lowest.is_finite() { (lowest * 0.9).max(50.0) } else { 70.0 };

    let mut psola = Psola::new(PsolaConfig {
        sample_rate,
        latency_f0_floor: floor,
        f0_max: 1100.0,
    });

    // ── 两级时间常数，和实时版 `Retuner` 一致 ──
    //
    // 慢的那级（intent）决定"想唱哪个音"，量化只对它做；
    // 快的那级（retune）只平滑修正量。合成一级会同时坏掉两件事：
    // 边界音上抖，以及颤音被抵消。
    let intent = smooth_zero_phase(track, cfg.intent_ms);

    // 每帧需要的修正量（以 log2 比率表示），只看意图音高，**不看瞬时 f0**。
    //
    // 这一条是颤音能活下来的原因：ratio 与瞬时 f0 无关，
    // 所以 f0 相对意图的快速起伏被原样搬到输出上。
    // 写成 `target / f0` 就会把每个瞬时值都拽到固定目标上 —— 颤音没了。
    let mut correction: Vec<f32> = Vec::with_capacity(track.frames.len());
    let voiced: Vec<bool> = track.frames.iter().map(|f| f.voiced && f.f0 > 0.0).collect();
    for (i, f) in track.frames.iter().enumerate() {
        if !voiced[i] {
            correction.push(0.0);
            continue;
        }
        let _ = f;
        let intent_midi = crate::hz_to_midi(intent[i]);
        let target_midi = cfg.key.nearest_in_scale(intent_midi) as f32;
        correction.push((target_midi - intent_midi) / 12.0);
    }
    // 同上：清音处的 0 会把相邻音符的修正量拖向"不修"，先填再平滑
    hold_fill(&mut correction, &voiced);
    smooth_vec_zero_phase(&mut correction, track, cfg.retune_ms);

    let formant = (cfg.formant_shift / 12.0).exp2();
    let shift = (cfg.pitch_shift / 12.0).exp2();

    let mut out = vec![0.0f32; samples.len()];
    let mut buf = vec![0.0f32; track.hop];
    let n = track.frames.len();

    for i in 0..n {
        let start = i * track.hop;
        let end = (start + track.hop).min(samples.len());
        if start >= end {
            break;
        }
        let f = track.frames[i];

        if f.voiced && f.f0 > 0.0 {
            let ratio = correction[i].exp2() * shift;
            psola.set_pitch(f.f0, true);
            psola.set_ratio(ratio);
            psola.set_formant(formant);
        } else {
            psola.set_pitch(0.0, false);
            psola.set_ratio(1.0);
            psola.set_formant(1.0);
        }

        let len = end - start;
        psola.process(&samples[start..end], &mut buf[..len]);
        out[start..end].copy_from_slice(&buf[..len]);

        if i % 64 == 0 && !progress(i as f32 / n.max(1) as f32) {
            return None;
        }
    }

    // PSOLA 有固定算法延迟，输出整体右移了这么多 —— 移回来，
    // 否则离线产物和原始干声对不齐，没法分轨叠在一起听。
    let delay = psola.latency_samples() as usize;
    if delay > 0 && delay < out.len() {
        out.copy_within(delay.., 0);
        let keep = out.len() - delay;
        out[keep..].fill(0.0);
    }

    progress(1.0);
    Some(out)
}

/// 零相位一阶平滑：前向 + 反向各一遍。
///
/// 实时链路只能单向平滑，代价是修正永远滞后半拍 —— 起音处听得最清楚。
/// 离线可以反向再跑一遍，两次的滞后正好抵消，**修正与演唱同相**。
fn smooth_zero_phase(track: &PitchTrack, tau_ms: f32) -> Vec<f32> {
    // 在对数域平滑：音高的感知是对数的，升降调的速度才对称
    let mut v: Vec<f32> = track.frames.iter().map(|f| f.f0.max(1.0).ln()).collect();
    let voiced: Vec<bool> = track.frames.iter().map(|f| f.voiced && f.f0 > 0.0).collect();

    // ⚠️ **必须先填洞再平滑。**
    //
    // 清音帧的 f0 是 0，`max(1.0).ln()` 得到 ln(1 Hz) = 0 —— 而真实值在
    // ln(255) ≈ 5.5 附近。把这些 0 一起平滑，每一段静音都会把意图音高
    // 狠狠往下拽。
    //
    // 实测代价：一段 C4-E4-G4-E4 的旋律（音符之间有包络间隙），
    // 紧邻开头静音的第一个音**完全没被修正**，G4 也只修了一半。
    hold_fill(&mut v, &voiced);

    smooth_vec_zero_phase(&mut v, track, tau_ms);
    v.iter().map(|x| x.exp()).collect()
}

/// 用两侧的有效值填补无效区间：区间内线性过渡，首尾段用最近的有效值兜住。
///
/// 目的不是"猜出静音处的音高"（那没有意义），而是**不让无效值污染平滑**。
/// 填出来的值只进平滑器，不会被当成真实音高用 —— 清音帧照样走透传。
fn hold_fill(v: &mut [f32], valid: &[bool]) {
    let first = valid.iter().position(|b| *b);
    let Some(first) = first else { return }; // 全是清音，没什么可填
    let last = valid.iter().rposition(|b| *b).unwrap_or(first);

    // 首尾段：用最近的有效值兜住
    let head = v[first];
    for x in v[..first].iter_mut() {
        *x = head;
    }
    let tail = v[last];
    for x in v[last + 1..].iter_mut() {
        *x = tail;
    }

    // 中间的洞：线性过渡
    let mut i = first;
    while i <= last {
        if valid[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i <= last && !valid[i] {
            i += 1;
        }
        let (a, b) = (v[start - 1], v[i]);
        let len = i - start;
        for (k, x) in v[start..i].iter_mut().enumerate() {
            *x = a + (b - a) * (k + 1) as f32 / (len + 1) as f32;
        }
    }
}

/// 就地做零相位一阶平滑。
fn smooth_vec_zero_phase(v: &mut [f32], track: &PitchTrack, tau_ms: f32) {
    if tau_ms <= 0.0 || v.is_empty() {
        return;
    }
    let dt = track.hop as f32 / track.sample_rate;
    // 两遍平滑相当于时间常数加倍，所以每遍用一半
    let a = (1.0 - (-dt / (tau_ms / 2000.0).max(1e-6)).exp()).clamp(0.0, 1.0);

    let mut acc = v[0];
    for x in v.iter_mut() {
        acc += (*x - acc) * a;
        *x = acc;
    }
    acc = v[v.len() - 1];
    for x in v.iter_mut().rev() {
        acc += (*x - acc) * a;
        *x = acc;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const SR: f32 = 48_000.0;

    fn sine(freq: f32, secs: f32) -> Vec<f32> {
        let n = (secs * SR) as usize;
        (0..n).map(|i| (TAU * freq * i as f32 / SR).sin() * 0.5).collect()
    }

    /// 带颤音的正弦：5 Hz、±40 音分，接近真实演唱。
    fn vibrato(center: f32, secs: f32) -> Vec<f32> {
        let n = (secs * SR) as usize;
        let mut phase = 0.0f32;
        (0..n)
            .map(|i| {
                let t = i as f32 / SR;
                let f = center * (40.0 / 1200.0 * (TAU * 5.0 * t).sin()).exp2();
                phase += TAU * f / SR;
                phase.sin() * 0.5
            })
            .collect()
    }

    fn run(x: &[f32]) -> PitchTrack {
        track_pitch(x, SR, |_| true).expect("未取消")
    }

    #[test]
    fn tracks_a_steady_tone() {
        let t = run(&sine(220.0, 1.0));
        assert!(t.voiced_count() > t.frames.len() / 2, "大部分帧应判为浊音");
        let err = 1200.0 * (t.median_f0() / 220.0).log2();
        assert!(err.abs() < 15.0, "中位基频偏了 {err:.1} cents");
    }

    /// **八度纠错是这个模块存在的首要理由。**
    ///
    /// YIN 最典型的失败就是整帧翻倍/减半。离线能看到邻域，
    /// 就能把这类离群值揪出来 —— 实时链路做不到。
    #[test]
    fn corrects_injected_octave_errors() {
        let mut t = run(&sine(196.0, 1.0));
        // 人为注入：每 20 帧把一帧翻倍
        let mut injected = 0;
        for (i, f) in t.frames.iter_mut().enumerate() {
            if f.voiced && i % 20 == 7 {
                f.f0 *= 2.0;
                injected += 1;
            }
        }
        assert!(injected > 5, "测试本身没注入足够的错误");

        let fixed = fix_octaves(&mut t.frames);
        assert!(
            fixed >= injected * 8 / 10,
            "注入 {injected} 个八度错误，只修回 {fixed} 个"
        );

        // 修完之后不该还有明显的离群值
        let med = t.median_f0();
        let outliers = t
            .frames
            .iter()
            .filter(|f| f.voiced && (f.f0 / med).log2().abs() > 0.5)
            .count();
        assert_eq!(outliers, 0, "还剩 {outliers} 个半八度以上的离群值");
    }

    /// 颤音**必须留住**。
    ///
    /// 后处理的每一步都有"顺手抹平"的风险，而颤音被抹掉的结果就是
    /// 唱成 MIDI —— 那正是这个产品最不该出现的效果。
    #[test]
    fn keeps_vibrato() {
        let t = run(&vibrato(220.0, 2.0));
        let f: Vec<f32> = t.frames.iter().filter(|x| x.voiced).map(|x| x.f0).collect();
        assert!(f.len() > 200);

        // 掐掉首尾的建立段，量中间的音分摆幅
        let mid = &f[f.len() / 4..f.len() * 3 / 4];
        let med = t.median_f0();
        let (lo, hi) = mid.iter().fold((f32::MAX, f32::MIN), |(a, b), v| (a.min(*v), b.max(*v)));
        let span = 1200.0 * (hi / lo).log2();
        assert!(
            span > 40.0,
            "颤音摆幅只剩 {span:.0} cents（注入的是 ±40 = 80 cents 全幅），后处理抹平了它"
        );
        assert!((1200.0 * (med / 220.0).log2()).abs() < 20.0);
    }

    /// 音中间的短暂失检要补上；真正的断句不能补。
    #[test]
    fn fills_short_dropouts_but_not_real_breaks() {
        let mut t = run(&sine(200.0, 1.0));
        let n = t.frames.len();

        // 中间挖一个 4 帧的洞（模拟失检）
        for f in t.frames[n / 2..n / 2 + 4].iter_mut() {
            f.voiced = false;
            f.f0 = 0.0;
        }
        // 再挖一个远超上限的洞（模拟真的换气）
        let big = n / 4;
        for f in t.frames[big..big + MAX_GAP * 3].iter_mut() {
            f.voiced = false;
            f.f0 = 0.0;
        }

        let filled = fill_gaps(&mut t.frames);
        assert_eq!(filled, 4, "短洞应当正好补 4 帧，实际补了 {filled}");
        assert!(t.frames[n / 2 + 1].voiced, "短洞没补上");
        assert!(!t.frames[big + 1].voiced, "长洞被错误地补了 —— 那会造出不存在的滑音");
    }

    /// 边界处的清音不是"洞"，不能填。
    #[test]
    fn does_not_fill_at_the_edges() {
        let mut t = run(&sine(200.0, 0.5));
        for f in t.frames.iter_mut().take(3) {
            f.voiced = false;
            f.f0 = 0.0;
        }
        let n = t.frames.len();
        for f in t.frames[n - 3..].iter_mut() {
            f.voiced = false;
            f.f0 = 0.0;
        }
        fill_gaps(&mut t.frames);
        assert!(!t.frames[0].voiced && !t.frames[n - 1].voiced, "边界被当成洞填了");
    }

    /// 取消要立刻生效 —— 一首五分钟的歌有上万帧，点了停就得停。
    #[test]
    fn honours_cancellation() {
        let mut calls = 0;
        let out = track_pitch(&sine(220.0, 5.0), SR, |_| {
            calls += 1;
            calls < 3 // 第三次汇报时取消
        });
        assert!(out.is_none(), "取消之后仍然返回了结果");
    }

    #[test]
    fn silence_yields_no_voiced_frames() {
        let t = run(&vec![0.0; 24_000]);
        assert_eq!(t.voiced_count(), 0);
        assert_eq!(t.median_f0(), 0.0);
    }

    /// 后处理的三步顺序不能换：纠错必须在中值之前。
    ///
    /// 反过来的话，离群值会污染中值本身 —— 这条用一个极端例子钉住。
    #[test]
    fn octave_fix_must_precede_median() {
        let mut a = run(&sine(196.0, 0.6));
        let mut b = a.clone();
        // 注入一串连续的八度错误
        for f in a.frames.iter_mut().skip(20).take(6) {
            if f.voiced {
                f.f0 *= 2.0;
            }
        }
        for f in b.frames.iter_mut().skip(20).take(6) {
            if f.voiced {
                f.f0 *= 2.0;
            }
        }

        // 正确顺序
        fix_octaves(&mut a.frames);
        median_filter(&mut a.frames);
        // 错误顺序
        median_filter(&mut b.frames);
        fix_octaves(&mut b.frames);

        let bad = |t: &PitchTrack| {
            let med = 196.0;
            t.frames
                .iter()
                .filter(|f| f.voiced && (f.f0 / med).log2().abs() > 0.5)
                .count()
        };
        assert!(
            bad(&a) <= bad(&b),
            "顺序换了反而更好？纠错 {} vs 中值先 {}",
            bad(&a),
            bad(&b)
        );
        assert_eq!(bad(&a), 0, "正确顺序下不该有残留离群值");
    }

    fn cfg_c_major() -> RecorrectConfig {
        RecorrectConfig {
            key: crate::Key { tonic: 0, scale: crate::ScaleKind::Major },
            retune_ms: 20.0,
            intent_ms: 150.0,
            ..Default::default()
        }
    }

    /// 离线校准要把跑调的音拉到音阶上。
    #[test]
    fn recorrect_pulls_a_flat_note_onto_the_scale() {
        // 比 A4 低 45 音分
        let sung = crate::midi_to_hz(69.0) * (-45.0f32 / 1200.0).exp2();
        let x = sine(sung, 2.0);
        let t = run(&x);
        let y = recorrect(&x, SR, &t, cfg_c_major(), |_| true).unwrap();

        assert_eq!(y.len(), x.len(), "长度必须一致，否则没法和干声对齐");
        assert!(y.iter().all(|v| v.is_finite()));

        // 量输出的中段
        let out = run(&y[SR as usize / 2..]);
        let err = 1200.0 * (out.median_f0() / crate::midi_to_hz(69.0)).log2();
        assert!(err.abs() < 25.0, "修完还差目标音 {err:.0} cents");
    }

    /// **零相位平滑是离线版最实在的好处**：修正不再滞后半拍。
    ///
    /// 实时版只能单向平滑，起音处必然慢一拍。这里用阶跃音高验证：
    /// 双向平滑的结果应当在跳变点**两侧对称**，而单向的会整体右移。
    #[test]
    fn zero_phase_smoothing_has_no_lag() {
        let n = 400;
        let hop = HOP;
        // 前一半 200Hz，后一半 300Hz 的阶跃
        let frames: Vec<PitchFrame> = (0..n)
            .map(|i| PitchFrame {
                f0: if i < n / 2 { 200.0 } else { 300.0 },
                voiced: true,
                aperiodicity: 0.05,
                rms: 0.3,
            })
            .collect();
        let t = PitchTrack { frames, hop, sample_rate: SR, octave_fixes: 0, gap_fills: 0 };

        let sm = smooth_zero_phase(&t, 60.0);
        let mid = n / 2;
        let geo = (200.0f32 * 300.0).sqrt();

        // 跳变点本身应当落在两个电平的几何中点附近 —— 这正是"无滞后"的表现。
        // 单向平滑在这一点还停在 200Hz 附近。
        let err = 1200.0 * (sm[mid] / geo).log2();
        assert!(
            err.abs() < 120.0,
            "跳变点处于 {:.0}Hz（几何中点 {:.0}Hz，差 {err:.0} cents）—— 看起来仍有滞后",
            sm[mid], geo
        );
        // 两端要收敛到各自的电平
        assert!((sm[20] - 200.0).abs() < 8.0, "起始端没收敛：{:.1}", sm[20]);
        assert!((sm[n - 20] - 300.0).abs() < 8.0, "结束端没收敛：{:.1}", sm[n - 20]);
    }

    /// 离线 PSOLA 不该再被 `f0_floor` 截断 —— 低音区窗口按真实周期走。
    #[test]
    fn low_notes_are_not_truncated_offline() {
        let x = sine(82.0, 1.5); // E2，远低于实时链路 130Hz 的下限
        let t = run(&x);
        assert!(t.voiced_count() > 0, "82Hz 应当能测到");
        let y = recorrect(&x, SR, &t, cfg_c_major(), |_| true).unwrap();
        assert!(y.iter().all(|v| v.is_finite()));
        let peak = y.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak > 0.05 && peak < 4.0, "输出峰值 {peak:.2} 不合理");
    }

    #[test]
    fn recorrect_honours_cancellation() {
        let x = sine(220.0, 3.0);
        let t = run(&x);
        let mut calls = 0;
        let y = recorrect(&x, SR, &t, cfg_c_major(), |_| {
            calls += 1;
            calls < 3
        });
        assert!(y.is_none(), "取消之后仍然返回了结果");
    }

    /// **回归用例：音符之间的静音不能污染意图音高。**
    ///
    /// 清音帧的 f0 是 0，`max(1.0).ln()` = ln(1 Hz) = 0，比真实值
    /// （ln 255 ≈ 5.5）低一大截。不先填洞就平滑，每段静音都会把意图
    /// 往下拽 —— 实测代价是**紧邻开头静音的第一个音完全没被修正**
    /// （-39 → -39），而中间的音正常。
    ///
    /// 这条用带静音间隔的旋律守住它。
    #[test]
    fn silence_between_notes_must_not_poison_the_intent() {
        // C4 低 45 音分，前后各留 0.15s 静音
        let sung = crate::midi_to_hz(60.0) * (-45.0f32 / 1200.0).exp2();
        let gap = vec![0.0f32; (SR * 0.15) as usize];
        let mut x = gap.clone();
        x.extend_from_slice(&sine(sung, 1.2));
        x.extend_from_slice(&gap);

        let t = run(&x);
        let y = recorrect(&x, SR, &t, cfg_c_major(), |_| true).unwrap();

        // 量中段，避开起收音
        let mid = &y[(SR * 0.5) as usize..(SR * 1.1) as usize];
        let out = run(mid);
        assert!(out.voiced_count() > 0, "输出中段应当是浊音");
        let err = 1200.0 * (out.median_f0() / crate::midi_to_hz(60.0)).log2();
        assert!(
            err.abs() < 25.0,
            "第一个音只修到差 {err:.0} cents —— 静音把意图音高拽跑了"
        );
    }

    /// `hold_fill` 本身：洞被两侧的值接上，首尾用最近的有效值兜住。
    #[test]
    fn hold_fill_bridges_holes_and_clamps_edges() {
        let mut v = vec![0.0, 0.0, 10.0, 0.0, 0.0, 0.0, 20.0, 0.0];
        let valid = [false, false, true, false, false, false, true, false];
        hold_fill(&mut v, &valid);

        assert_eq!(&v[..2], &[10.0, 10.0], "首段没用最近有效值兜住");
        assert_eq!(v[7], 20.0, "尾段没兜住");
        // 中间三格应当从 10 线性过渡到 20
        assert!((v[3] - 12.5).abs() < 1e-5, "v[3]={}", v[3]);
        assert!((v[4] - 15.0).abs() < 1e-5, "v[4]={}", v[4]);
        assert!((v[5] - 17.5).abs() < 1e-5, "v[5]={}", v[5]);
    }

    /// 全是清音时不该 panic，也不该改动任何值。
    #[test]
    fn hold_fill_survives_all_invalid() {
        let mut v = vec![1.0, 2.0, 3.0];
        hold_fill(&mut v, &[false, false, false]);
        assert_eq!(v, vec![1.0, 2.0, 3.0]);
    }
}
