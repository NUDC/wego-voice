//! 调式量化与 retune 策略。
//!
//! 这个模块决定了修音听起来"像人"还是"像机器"。实施方案 §3.4 的四条策略
//! 全部落在这里：
//!
//! 1. **量化到调内音**而非十二平均律全部音
//! 2. **保留颤音**（4~7Hz 的 f0 调制）
//! 3. **retune speed** 可调（0ms = 电音，20~80ms = 自然）
//! 4. **修正上限**：跑调过大时不硬拉

/// 十二平均律音名（以 C 为 0）。
pub const SEMITONE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// 调式：以半音为单位的音级集合（相对主音）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleKind {
    Chromatic,
    Major,
    NaturalMinor,
    HarmonicMinor,
    MajorPentatonic,
    MinorPentatonic,
}

impl ScaleKind {
    /// 相对主音的半音偏移集合。
    pub fn degrees(self) -> &'static [u8] {
        match self {
            ScaleKind::Chromatic => &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
            ScaleKind::Major => &[0, 2, 4, 5, 7, 9, 11],
            ScaleKind::NaturalMinor => &[0, 2, 3, 5, 7, 8, 10],
            ScaleKind::HarmonicMinor => &[0, 2, 3, 5, 7, 8, 11],
            ScaleKind::MajorPentatonic => &[0, 2, 4, 7, 9],
            ScaleKind::MinorPentatonic => &[0, 3, 5, 7, 10],
        }
    }
}

/// 调（主音 + 调式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    /// 主音，0 = C，1 = C#，……，11 = B。
    pub tonic: u8,
    pub scale: ScaleKind,
}

impl Default for Key {
    fn default() -> Self {
        Self {
            tonic: 0,
            scale: ScaleKind::Chromatic,
        }
    }
}

impl Key {
    /// 判断某个 MIDI 音高是否在调内。
    pub fn contains_midi(&self, midi: i32) -> bool {
        let pc = midi.rem_euclid(12);
        let rel = (pc - self.tonic as i32).rem_euclid(12);
        self.scale.degrees().contains(&(rel as u8))
    }

    /// 找出离 `midi_f`（可为小数）最近的调内音，返回整数 MIDI 音高。
    ///
    /// 平局时偏向更低的音：唱高了被拉下来比唱低了被顶上去更自然。
    pub fn nearest_in_scale(&self, midi_f: f32) -> i32 {
        let center = midi_f.round() as i32;
        let mut best = center;
        let mut best_dist = f32::INFINITY;
        // ±6 个半音足以覆盖任何调式的最大音级间隔（全音阶最大间隔为 3）
        for cand in (center - 6)..=(center + 6) {
            if !self.contains_midi(cand) {
                continue;
            }
            let d = (cand as f32 - midi_f).abs();
            if d < best_dist - 1e-6 {
                best_dist = d;
                best = cand;
            }
        }
        best
    }
}

/// 频率 → MIDI 音高（A4 = 440Hz = MIDI 69）。返回小数。
#[inline]
pub fn hz_to_midi(hz: f32) -> f32 {
    69.0 + 12.0 * (hz / 440.0).log2()
}

/// MIDI 音高 → 频率。接受小数。
#[inline]
pub fn midi_to_hz(midi: f32) -> f32 {
    440.0 * ((midi - 69.0) / 12.0).exp2()
}

/// 两个频率之间的音分差。
#[inline]
pub fn cents_between(a: f32, b: f32) -> f32 {
    1200.0 * (a / b).log2()
}

#[derive(Debug, Clone, Copy)]
pub struct RetuneConfig {
    pub sample_rate: f32,
    /// 修正速度（毫秒）。0 = 瞬间吸附（电音），20~80 = 自然。
    pub retune_ms: f32,
    /// "意图音高"跟踪的时间常数（毫秒）。
    ///
    /// 这是保留颤音的机制：用慢速平滑得到歌手"想唱的音"，
    /// 只对它做量化；f0 相对平滑值的快速起伏（也就是颤音）原样保留。
    /// 150ms 对应约 6.7Hz 以下的调制不被跟踪 —— 正好落在 4~7Hz 的颤音带外沿。
    pub intent_ms: f32,
    /// 修正幅度上限（音分）。超出则只做部分修正，不硬拉。
    ///
    /// 硬拉 300 cents 以上会产生严重的 PSOLA 失真，听感比不修还糟。
    ///
    /// # ⚠️ 当前（v1 调式量化方案下）这个上限触发不了
    ///
    /// 量化目标是"最近的调内音"，所以需要的修正量天然被
    /// **半个最大音级间隔**卡住：
    ///
    /// | 调式 | 最大间隔 | 最大修正量 |
    /// |---|---|---|
    /// | 半音阶 | 100 cents | 50 cents |
    /// | 大调/小调 | 200 cents | 100 cents |
    /// | 五声 | 300 cents | 150 cents |
    ///
    /// 也就是说无论唱得多离谱，"离最近调内音的距离"永远不超过 150 cents。
    ///
    /// 这个上限真正起作用要等到**引入参考旋律**（Phase 2+ 的 MIDI 目标音）
    /// 之后 —— 那时歌手可能整整唱错一个音，偏离目标 500 cents 以上，
    /// 硬拉过去必然失真。在那之前它是一条备而不用的安全网。
    pub max_correction_cents: f32,
    /// 超出上限后仍施加的修正比例（0 = 完全不修，1 = 硬拉）。
    pub over_limit_ratio: f32,
}

impl Default for RetuneConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            retune_ms: 40.0,
            intent_ms: 150.0,
            max_correction_cents: 300.0,
            over_limit_ratio: 0.35,
        }
    }
}

/// 一帧修正的结果，同时供音频处理和 UI 显示使用。
#[derive(Debug, Clone, Copy, Default)]
pub struct RetuneFrame {
    /// 送给 PSOLA 的移调比率（输出 f0 / 输入 f0）。1.0 = 不变调。
    pub ratio: f32,
    /// 平滑后的"意图音高"（Hz）。
    pub intent_hz: f32,
    /// 量化目标音（Hz）。
    pub target_hz: f32,
    /// 目标音的 MIDI 音高，用于 UI 显示音名。
    pub target_midi: i32,
    /// 修正前相对目标音的偏差（音分）。UI 的音准柱画的就是这个。
    pub cents_off: f32,
    /// 本帧是否因超出上限而只做了部分修正。
    pub clamped: bool,
}

/// retune 状态机。每个分析帧调用一次 [`Retuner::process`]。
pub struct Retuner {
    cfg: RetuneConfig,
    key: Key,
    /// 平滑后的意图音高（MIDI 域，便于做等比例平滑）。
    intent_midi: f32,
    /// 平滑后的修正比率（对数域，避免不同方向的修正速度不对称）。
    smoothed_log_ratio: f32,
    initialized: bool,
}

impl Retuner {
    pub fn new(cfg: RetuneConfig, key: Key) -> Self {
        Self {
            cfg,
            key,
            intent_midi: 0.0,
            smoothed_log_ratio: 0.0,
            initialized: false,
        }
    }

    pub fn set_key(&mut self, key: Key) {
        self.key = key;
    }

    pub fn key(&self) -> Key {
        self.key
    }

    pub fn set_retune_ms(&mut self, ms: f32) {
        self.cfg.retune_ms = ms.max(0.0);
    }

    /// 浊音段中断时调用，避免下一次起音被上一句的状态污染。
    pub fn reset(&mut self) {
        self.initialized = false;
        self.smoothed_log_ratio = 0.0;
    }

    /// 处理一个分析帧。
    ///
    /// `f0_hz` 为本帧检测到的基频，`hop_samples` 为距上一帧的样本数
    /// （用于把时间常数换算成单帧的平滑系数）。
    ///
    /// **实时安全**：纯算术，无分配无分支惩罚。
    pub fn process(&mut self, f0_hz: f32, hop_samples: usize) -> RetuneFrame {
        if f0_hz <= 0.0 {
            return RetuneFrame {
                ratio: 1.0,
                ..Default::default()
            };
        }
        let midi = hz_to_midi(f0_hz);

        // --- 意图音高跟踪（颤音保留的核心）---
        if !self.initialized {
            self.intent_midi = midi;
            self.initialized = true;
        } else {
            let a = one_pole_coeff(self.cfg.intent_ms, self.cfg.sample_rate, hop_samples);
            self.intent_midi += a * (midi - self.intent_midi);
        }

        // --- 量化：只对意图音高做，不对瞬时 f0 做 ---
        let target_midi = self.key.nearest_in_scale(self.intent_midi);
        let target_hz = midi_to_hz(target_midi as f32);

        // 需要的修正量（音分），以意图音高为基准
        let intent_hz = midi_to_hz(self.intent_midi);
        let needed_cents = cents_between(target_hz, intent_hz);

        // --- 修正上限 ---
        let (applied_cents, clamped) =
            if needed_cents.abs() > self.cfg.max_correction_cents {
                (needed_cents * self.cfg.over_limit_ratio, true)
            } else {
                (needed_cents, false)
            };

        // --- retune speed 平滑（在对数域做，升降调速度才对称）---
        let target_log_ratio = applied_cents / 1200.0;
        if self.cfg.retune_ms <= 0.0 {
            self.smoothed_log_ratio = target_log_ratio; // 电音档：瞬间吸附
        } else {
            let a = one_pole_coeff(self.cfg.retune_ms, self.cfg.sample_rate, hop_samples);
            self.smoothed_log_ratio += a * (target_log_ratio - self.smoothed_log_ratio);
        }

        // 注意：ratio 只由"意图音高 → 目标音"的修正量决定，
        // 与瞬时 f0 无关。所以 f0 相对意图音高的快速起伏（颤音）
        // 被原样搬运到输出上 —— 这正是我们要的。
        let ratio = self.smoothed_log_ratio.exp2();

        RetuneFrame {
            ratio,
            intent_hz,
            target_hz,
            target_midi,
            cents_off: cents_between(f0_hz, target_hz),
            clamped,
        }
    }
}

/// 一阶低通的单帧系数：给定时间常数（ms）与本帧步进的样本数。
///
/// 采用 1 - exp(-dt/tau) 而非固定系数，这样 hop 大小变化时平滑速度保持一致。
#[inline]
fn one_pole_coeff(tau_ms: f32, sample_rate: f32, hop_samples: usize) -> f32 {
    if tau_ms <= 0.0 {
        return 1.0;
    }
    let dt = hop_samples as f32 / sample_rate;
    let tau = tau_ms * 1e-3;
    (1.0 - (-dt / tau).exp()).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_hz_roundtrip() {
        for &m in &[40.0f32, 60.0, 69.0, 81.0] {
            let hz = midi_to_hz(m);
            assert!((hz_to_midi(hz) - m).abs() < 1e-3);
        }
        assert!((midi_to_hz(69.0) - 440.0).abs() < 1e-3);
    }

    #[test]
    fn c_major_excludes_black_keys() {
        let key = Key { tonic: 0, scale: ScaleKind::Major };
        assert!(key.contains_midi(60)); // C
        assert!(!key.contains_midi(61)); // C#
        assert!(key.contains_midi(62)); // D
        assert!(!key.contains_midi(66)); // F#
        assert!(key.contains_midi(71)); // B
    }

    #[test]
    fn quantizes_to_scale_not_chromatic() {
        let key = Key { tonic: 0, scale: ScaleKind::Major };
        // 唱到 C#（61）偏低一点：半音修正应落到 C(60) 而不是停在 C#
        assert_eq!(key.nearest_in_scale(60.9), 60);
        // 唱到 F#（66）偏高一点：应落到 G(67)
        assert_eq!(key.nearest_in_scale(66.6), 67);
    }

    #[test]
    fn chromatic_scale_keeps_every_semitone() {
        let key = Key { tonic: 0, scale: ScaleKind::Chromatic };
        for m in 55..75 {
            assert!(key.contains_midi(m));
        }
        assert_eq!(key.nearest_in_scale(61.2), 61);
    }

    /// 稳态跑调应被拉回目标音。
    #[test]
    fn corrects_steady_flat_note() {
        let cfg = RetuneConfig { retune_ms: 20.0, ..Default::default() };
        let key = Key { tonic: 0, scale: ScaleKind::Major };
        let mut r = Retuner::new(cfg, key);

        // 比 A4 低 40 cents
        let sung = midi_to_hz(69.0 - 0.40);
        let hop = 256;
        let mut frame = RetuneFrame::default();
        // 跑 400ms，远超两个时间常数
        for _ in 0..(0.4 * 48_000.0 / hop as f32) as usize {
            frame = r.process(sung, hop);
        }
        let corrected = sung * frame.ratio;
        let err = cents_between(corrected, 440.0).abs();
        assert!(err < 10.0, "稳态修正后仍偏 {err:.1} cents");
    }

    /// 颤音必须活下来：修正后的 f0 起伏幅度应与输入接近。
    #[test]
    fn preserves_vibrato() {
        let cfg = RetuneConfig { retune_ms: 20.0, ..Default::default() };
        let key = Key { tonic: 0, scale: ScaleKind::Major };
        let mut r = Retuner::new(cfg, key);

        let hop = 256usize;
        let sr = 48_000.0f32;
        let vib_hz = 5.5; // 典型颤音速率
        let vib_cents = 50.0; // ±50 cents 的颤音深度

        let mut out_min = f32::INFINITY;
        let mut out_max = f32::NEG_INFINITY;
        let frames = (1.0 * sr / hop as f32) as usize;

        for i in 0..frames {
            let t = i as f32 * hop as f32 / sr;
            let dev = vib_cents * (std::f32::consts::TAU * vib_hz * t).sin();
            let f0 = 440.0 * (dev / 1200.0).exp2();
            let frame = r.process(f0, hop);
            // 前半秒用于收敛，只统计后半段
            if t > 0.5 {
                let out = f0 * frame.ratio;
                out_min = out_min.min(out);
                out_max = out_max.max(out);
            }
        }

        let out_depth = cents_between(out_max, out_min) / 2.0;
        // 颤音深度应基本保住。若实现错误地量化瞬时 f0，这里会塌缩到接近 0
        assert!(
            out_depth > vib_cents * 0.8,
            "颤音被修掉了：输入 ±{vib_cents} cents，输出仅 ±{out_depth:.1} cents"
        );
    }

    /// 跑调超过上限时不应硬拉。
    ///
    /// 这里把上限压到 30 cents 才测得到 —— 原因见 `max_correction_cents`
    /// 的文档：调式量化下需要的修正量天然不超过 150 cents，
    /// 默认的 300 cents 上限在引入参考旋律之前触发不了。
    #[test]
    fn does_not_hard_pull_beyond_limit() {
        let cfg = RetuneConfig {
            retune_ms: 0.0,
            intent_ms: 0.0,
            max_correction_cents: 30.0,
            over_limit_ratio: 0.35,
            ..Default::default()
        };
        let key = Key { tonic: 0, scale: ScaleKind::Chromatic };
        let mut r = Retuner::new(cfg, key);

        // 比 A4 高 45 cents：半音阶下最近音仍是 A4，需修正 -45 cents，超过 30 上限
        let sung = midi_to_hz(69.0) * (45.0f32 / 1200.0).exp2();
        let mut frame = RetuneFrame::default();
        for _ in 0..200 {
            frame = r.process(sung, 256);
        }
        assert!(frame.clamped, "超限时应标记 clamped");

        let applied = cents_between(sung * frame.ratio, sung).abs();
        assert!(applied < 30.0, "超限时仍施加了 {applied:.1} cents");
        // 应当按 over_limit_ratio 做部分修正，而不是完全不修
        assert!(
            applied > 5.0,
            "部分修正应仍有可观幅度，实际只有 {applied:.1} cents"
        );
    }

    /// 记录一条设计边界：调式量化下，需要的修正量有天然上界。
    ///
    /// 这条测试的价值在于：将来若引入参考旋律而忘了调 `max_correction_cents`，
    /// 它会提醒这里的假设已经变了。
    #[test]
    fn scale_quantization_bounds_correction_amount() {
        for (scale, max_expected) in [
            (ScaleKind::Chromatic, 50.0f32),
            (ScaleKind::Major, 100.0),
            (ScaleKind::MinorPentatonic, 150.0),
        ] {
            let key = Key { tonic: 0, scale };
            let mut worst: f32 = 0.0;
            // 扫过一个八度内的所有音高
            for i in 0..=1200 {
                let midi = 60.0 + i as f32 / 100.0;
                let target = key.nearest_in_scale(midi);
                let dev = ((target as f32 - midi) * 100.0).abs();
                worst = worst.max(dev);
            }
            assert!(
                worst <= max_expected + 1.0,
                "{scale:?}：最大修正量 {worst:.0} cents，超出预期上界 {max_expected:.0}"
            );
        }
    }

    /// retune_ms = 0 应当立刻吸附（电音档）。
    #[test]
    fn zero_retune_snaps_immediately() {
        let cfg = RetuneConfig { retune_ms: 0.0, intent_ms: 0.0, ..Default::default() };
        let key = Key { tonic: 0, scale: ScaleKind::Major };
        let mut r = Retuner::new(cfg, key);
        let sung = midi_to_hz(69.0 - 0.3);
        let frame = r.process(sung, 256);
        let err = cents_between(sung * frame.ratio, 440.0).abs();
        assert!(err < 5.0, "电音档首帧就该吸附，实际偏 {err:.1} cents");
    }
}
