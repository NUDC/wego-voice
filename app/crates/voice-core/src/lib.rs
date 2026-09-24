//! # voice-core
//!
//! wego-voice 的纯 DSP 核心：音高检测（YIN）、调式量化与 retune 策略、
//! 音高修正（TD-PSOLA）。
//!
//! ## 设计约束
//!
//! 这个 crate **刻意不依赖任何 I/O、音频后端或 GUI**。原因：
//! 同一套 DSP 将来要能直接复用为 CLAP 插件的核心，宿主换了它不该知道。
//! 接口因此是纯函数式的 —— 喂 buffer 进去，吐 buffer 出来。
//!
//! ## 实时安全
//!
//! [`Corrector::process`] 及其调用的一切**不做堆分配、不加锁、不打日志、不 panic**。
//! 所有缓冲在 [`Corrector::new`] 中预分配。这是实时纪律的硬要求。
//!
//! ## 用法
//!
//! ```
//! use voice_core::{Corrector, CorrectorConfig};
//!
//! let mut c = Corrector::new(CorrectorConfig::default());
//! let input = vec![0.0f32; 256];
//! let mut output = vec![0.0f32; 256];
//! c.process(&input, &mut output);
//! let frame = c.last_frame();     // 供 UI 显示
//! ```

pub mod f0eval;
pub mod fft;
pub mod noise;
pub mod offline;
pub mod psola;
pub mod scale;
pub mod tilt;
pub mod timbre;
pub mod yin;

pub use noise::{to_dbfs, NoiseGate, NoiseGateConfig};
pub use offline::{recorrect, track_pitch, PitchFrame, PitchTrack, RecorrectConfig};
pub use psola::{Psola, PsolaConfig};
pub use tilt::Tilt;
pub use timbre::{analyze as analyze_timbre, match_to, TimbreMatch, TimbreProfile};
pub use scale::{
    cents_between, hz_to_midi, midi_to_hz, Key, RetuneConfig, RetuneFrame, Retuner, ScaleKind,
    SEMITONE_NAMES,
};
pub use yin::{PitchEstimate, Yin, YinConfig};

#[derive(Debug, Clone, Copy)]
pub struct CorrectorConfig {
    pub sample_rate: f32,
    /// 分析步进（样本数）。每积累这么多新样本跑一次 YIN。
    ///
    /// 256 @48kHz ≈ 5.3ms，足够跟上唱歌的音高变化；
    /// 调小会线性增加 CPU 占用（YIN 是主要开销）。
    pub hop: usize,
    pub yin: YinConfig,
    pub retune: RetuneConfig,
    pub psola: PsolaConfig,
    pub noise: NoiseGateConfig,
    pub key: Key,
}

impl Default for CorrectorConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            hop: 256,
            yin: YinConfig::default(),
            retune: RetuneConfig::default(),
            psola: PsolaConfig::default(),
            noise: NoiseGateConfig::default(),
            key: Key::default(),
        }
    }
}

impl CorrectorConfig {
    /// 按给定采样率生成一套一致的默认配置。
    ///
    /// 直接改 `sample_rate` 字段是不够的 —— 三个子模块各自也有采样率，
    /// 必须一起改，否则周期换算会全错。
    pub fn for_sample_rate(sample_rate: f32) -> Self {
        let d = Self::default();
        Self {
            sample_rate,
            yin: YinConfig { sample_rate, ..d.yin },
            retune: RetuneConfig { sample_rate, ..d.retune },
            psola: PsolaConfig { sample_rate, ..d.psola },
            noise: NoiseGateConfig { sample_rate, ..d.noise },
            ..d
        }
    }
}

/// 一个分析帧的全部可观测量。UI 的音高条、音准柱、诊断页都读这个。
#[derive(Debug, Clone, Copy, Default)]
pub struct AnalysisFrame {
    pub f0_hz: f32,
    pub is_voiced: bool,
    pub aperiodicity: f32,
    /// 输入信号 RMS（线性）。
    pub rms: f32,
    /// 本帧是否出现削顶（|s| >= 0.999）。录音增益过大时 UI 要提示。
    pub clipping: bool,
    /// 量化目标音的 MIDI 音高。
    pub target_midi: i32,
    /// 修正前相对目标音的偏差（音分）。音准柱画的就是它。
    pub cents_off: f32,
    /// 实际施加的移调比率。
    pub ratio: f32,
    /// 是否因跑调过大而只做了部分修正。
    pub clamped: bool,
    /// 当前房间噪声本底估计（线性 RMS）。诊断页显示它。
    pub noise_floor: f32,
    /// 本帧是否越过了噪声门。为 false 时 YIN 根本没跑。
    pub gate_open: bool,
}

pub struct Corrector {
    cfg: CorrectorConfig,
    yin: Yin,
    retuner: Retuner,
    psola: Psola,
    gate: NoiseGate,
    tilt: Tilt,

    /// YIN 的线性分析缓冲，长度恰为 `yin.required_len()`。
    /// 用 `copy_within` 左移，不重新分配。
    analysis: Vec<f32>,
    /// 自上次分析以来累积的新样本数。
    since_analysis: usize,

    frame: AnalysisFrame,
    bypass: bool,

    /// 角色的整体移调（半音）。叠在调式修正之上。
    pitch_shift: f32,
    /// 角色的共振峰平移（半音）。决定声线粗细，与音高无关。
    formant_shift: f32,
}

impl Corrector {
    pub fn new(cfg: CorrectorConfig) -> Self {
        let yin = Yin::new(cfg.yin);
        let analysis = vec![0.0; yin.required_len()];
        Self {
            yin,
            retuner: Retuner::new(cfg.retune, cfg.key),
            psola: Psola::new(cfg.psola),
            gate: NoiseGate::new(cfg.noise),
            tilt: Tilt::new(cfg.sample_rate),
            analysis,
            since_analysis: 0,
            frame: AnalysisFrame::default(),
            bypass: false,
            pitch_shift: 0.0,
            formant_shift: 0.0,
            cfg,
        }
    }

    /// 音频路径的固定算法延迟（样本数）。
    ///
    /// 只由 PSOLA 决定：YIN 分析的是已有历史，不给音频路径增加延迟
    /// （它带来的是控制量的滞后，不是声音的滞后）。
    #[inline]
    pub fn latency_samples(&self) -> u64 {
        self.psola.latency_samples()
    }

    #[inline]
    pub fn latency_ms(&self) -> f32 {
        self.psola.latency_ms()
    }

    /// 旁路：原样输出，但**保持与开启时完全相同的延迟**。
    ///
    /// 延迟不变是刻意的：A/B 对比时如果两条路径延迟不同，
    /// 人耳会把延迟差异当成音质差异，盲测结论就废了。
    pub fn set_bypass(&mut self, bypass: bool) {
        self.bypass = bypass;
    }

    #[inline]
    pub fn is_bypassed(&self) -> bool {
        self.bypass
    }

    pub fn set_key(&mut self, key: Key) {
        self.cfg.key = key;
        self.retuner.set_key(key);
    }

    #[inline]
    pub fn key(&self) -> Key {
        self.cfg.key
    }

    /// 设置修正速度（毫秒）。0 = 电音档，20~80 = 自然档。
    pub fn set_retune_ms(&mut self, ms: f32) {
        self.retuner.set_retune_ms(ms);
    }

    /// 角色的整体移调（半音，可为负）。叠加在调式修正之上。
    ///
    /// ⚠️ 这是 PSOLA 的**重变调**，不是修音那种几十音分的微调。
    /// 超过 ±5 半音音质会明显劣化（颗粒重叠失衡、金属感）——
    /// UI 上要如实告知，不要假装能无损变声。
    pub fn set_pitch_shift(&mut self, semitones: f32) {
        self.pitch_shift = semitones.clamp(-12.0, 12.0);
    }

    /// 角色的共振峰平移（半音，可为负）。
    ///
    /// 这才是"声线"的主维度：**不动音高**，只缩放频谱包络。
    /// 正值 = 共振峰上移 = 声道变短 = 听起来更细/更小（童声、少女）；
    /// 负值 = 下移 = 声道变长 = 更粗/更大（大叔）。
    pub fn set_formant_shift(&mut self, semitones: f32) {
        self.formant_shift = semitones.clamp(-12.0, 12.0);
    }

    /// 角色的频谱倾斜（dB/八度）。正 = 更亮，负 = 更暗。
    ///
    /// 这是声线的**第二个维度**：共振峰管"声道多长"，倾斜管"整体明暗"。
    /// `timbre::match_to` 报出的 `tilt_delta` 就是喂给这里的。
    pub fn set_tilt_db_per_oct(&mut self, s: f32) {
        self.tilt.set_db_per_oct(s);
    }

    #[inline]
    pub fn tilt_db_per_oct(&self) -> f32 {
        self.tilt.db_per_oct()
    }

    /// 噪声门余量（dB）。人声要高出实测本底这么多才放行。
    ///
    /// 调高：更不容易被风扇/电流声误触发，但会吃掉弱起音和收尾气声。
    /// 调低：反之。0 相当于关掉门（只剩绝对静音保护）。
    pub fn set_noise_gate_db(&mut self, db: f32) {
        self.gate.set_margin_db(db);
    }

    /// 当前噪声本底估计（线性 RMS）。
    #[inline]
    pub fn noise_floor(&self) -> f32 {
        self.gate.floor()
    }

    /// 换设备后重新学一遍本底。
    pub fn reset_noise_gate(&mut self) {
        self.gate.reset();
    }

    #[inline]
    pub fn pitch_shift(&self) -> f32 {
        self.pitch_shift
    }

    #[inline]
    pub fn formant_shift(&self) -> f32 {
        self.formant_shift
    }

    /// 最近一个分析帧。供 UI 以 30~60Hz 读取。
    #[inline]
    pub fn last_frame(&self) -> AnalysisFrame {
        self.frame
    }

    /// 累计输出欠载次数。稳态下必须恒为 0，非 0 说明实时链路有问题。
    #[inline]
    pub fn underruns(&self) -> u64 {
        self.psola.underruns
    }

    /// 处理一个音频块。`input` 与 `output` 长度必须一致。
    ///
    /// 块大小任意，内部按 `hop` 自行切分分析节奏。
    ///
    /// **实时安全**：无分配、无锁、无 panic。
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        let n = input.len().min(output.len());
        let mut off = 0;

        while off < n {
            // 推进到下一个分析点，或吃完整块（以先到者为准）
            let room = self.cfg.hop - self.since_analysis;
            let take = room.min(n - off);

            self.feed_analysis(&input[off..off + take]);
            self.psola.process(&input[off..off + take], &mut output[off..off + take]);

            // 频谱倾斜接在 PSOLA 之后：它塑形的是**输出**的明暗，
            // 而分析路径读的是原始输入，所以不会反过来干扰音高检测。
            //
            // 旁路时连它一起跳过 —— A/B 盲测必须比的是同一件事。
            if !self.bypass {
                self.tilt.process(&mut output[off..off + take]);
            }

            off += take;
            self.since_analysis += take;

            if self.since_analysis >= self.cfg.hop {
                self.since_analysis = 0;
                self.analyze();
            }
        }
    }

    /// 把新样本推入 YIN 的线性分析缓冲（左移 + 追加）。
    fn feed_analysis(&mut self, src: &[f32]) {
        let len = self.analysis.len();
        let n = src.len();
        if n >= len {
            self.analysis.copy_from_slice(&src[n - len..]);
        } else {
            self.analysis.copy_within(n.., 0);
            self.analysis[len - n..].copy_from_slice(src);
        }
    }

    /// 跑一次完整的分析 → 决策 → 参数下发。
    fn analyze(&mut self) {
        // --- 电平与削顶（UI 需要，且静音判据用得上）---
        let mut energy = 0.0f32;
        let mut peak = 0.0f32;
        for &s in &self.analysis {
            energy += s * s;
            peak = peak.max(s.abs());
        }
        let rms = (energy / self.analysis.len() as f32).sqrt();

        // --- 噪声门 ---
        //
        // 门关着就**根本不跑 YIN**。两个收益：
        //   1. 正确性：风扇、变压器电流声都有周期成分，YIN 会给出一个
        //      很自信的基频，然后 PSOLA 开始对着风扇修音
        //   2. CPU：静音段占实际使用时长的一大半，YIN 是这里最贵的一项
        let gate_open = self.gate.is_open(rms);
        if !gate_open {
            // 门关着 = 确定不是人声 → 可以拿它更新本底估计
            self.gate.update(rms, self.cfg.hop);
            self.unvoiced_frame(rms, peak, 1.0);
            return;
        }

        // --- 音高检测 ---
        let est = self.yin.analyze(&self.analysis);

        if !est.is_voiced {
            // VAD 说不是人声 → 允许更新本底。
            //
            // 清辅音（s / f / sh）会走到这里：它们响且非周期。
            // 靠 NoiseGate 的非对称时间常数（升得慢）把它们滤掉，
            // 而不是在这里特判 —— 特判清辅音要先能识别清辅音，是循环依赖。
            self.gate.update(rms, self.cfg.hop);
            self.unvoiced_frame(rms, peak, est.aperiodicity);
            return;
        }

        // 到这里说明本帧是人声 —— **本底冻结，一个字都不更新**。
        // 不冻结的话，一个 5 秒长音会把本底一路抬到人声电平，
        // 门随即关死，表现就是"唱着唱着修音没了"。

        // --- 调式量化 + retune 策略 ---
        let rt = self.retuner.process(est.f0_hz, self.cfg.hop);

        // 角色的移调与共振峰叠在修正之上。
        // bypass 要连角色一起旁路 —— 否则 A/B 盲测比的就不是同一件事了。
        let (ratio, formant) = if self.bypass {
            (1.0, 1.0)
        } else {
            (
                rt.ratio * (self.pitch_shift / 12.0).exp2(),
                (self.formant_shift / 12.0).exp2(),
            )
        };

        // --- 参数下发给 PSOLA ---
        self.psola.set_pitch(est.f0_hz, true);
        self.psola.set_ratio(ratio);
        self.psola.set_formant(formant);

        self.frame = AnalysisFrame {
            f0_hz: est.f0_hz,
            is_voiced: true,
            aperiodicity: est.aperiodicity,
            rms,
            clipping: peak >= 0.999,
            target_midi: rt.target_midi,
            cents_off: rt.cents_off,
            ratio,
            clamped: rt.clamped,
            noise_floor: self.gate.floor(),
            gate_open: true,
        };
    }

    /// 非人声帧的收尾：复位状态、写快照。
    ///
    /// 抽出来是因为它有两个入口（门关着 / YIN 判清音），
    /// 而「复位 retune 状态」这一步漏掉任何一个都会让下一次起音被上一句污染。
    fn unvoiced_frame(&mut self, rms: f32, peak: f32, aperiodicity: f32) {
        self.retuner.reset();
        self.psola.set_pitch(0.0, false);
        self.psola.set_ratio(1.0);
        self.psola.set_formant(1.0);
        self.frame = AnalysisFrame {
            f0_hz: 0.0,
            is_voiced: false,
            aperiodicity,
            rms,
            clipping: peak >= 0.999,
            ratio: 1.0,
            noise_floor: self.gate.floor(),
            gate_open: false,
            ..Default::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const SR: f32 = 48_000.0;

    fn sine(freq: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (TAU * freq * i as f32 / SR).sin()).collect()
    }

    fn run(c: &mut Corrector, input: &[f32], block: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(input.len());
        let mut buf = vec![0.0; block];
        for chunk in input.chunks(block) {
            let n = chunk.len();
            c.process(chunk, &mut buf[..n]);
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    #[test]
    fn detects_pitch_end_to_end() {
        let mut c = Corrector::new(CorrectorConfig::default());
        let input = sine(220.0, 48_000);
        let _ = run(&mut c, &input, 256);
        let f = c.last_frame();
        assert!(f.is_voiced, "稳定正弦应判为浊音");
        assert!(
            (f.f0_hz - 220.0).abs() < 3.0,
            "端到端检测到 {:.1} Hz，期望 220",
            f.f0_hz
        );
    }

    #[test]
    fn quantizes_toward_scale_tone() {
        let mut cfg = CorrectorConfig::default();
        cfg.key = Key { tonic: 0, scale: ScaleKind::Major };
        cfg.retune.retune_ms = 10.0;
        let mut c = Corrector::new(cfg);

        // 比 A4 低 45 cents
        let sung = midi_to_hz(69.0) * (-45.0f32 / 1200.0).exp2();
        let input = sine(sung, 48_000);
        let _ = run(&mut c, &input, 256);

        let f = c.last_frame();
        assert_eq!(f.target_midi, 69, "目标音应为 A4");
        // ratio 应把唱的音往上抬约 45 cents
        let applied = 1200.0 * f.ratio.log2();
        assert!(
            (applied - 45.0).abs() < 15.0,
            "施加的修正为 {applied:.1} cents，期望约 +45"
        );
    }

    /// 同样的块大小必须产生逐位相同的输出。
    ///
    /// 这条守的是**确定性**：任何未初始化状态、依赖地址/哈希顺序的逻辑
    /// 都会在这里露馅。将来做黄金样本回归测试，前提就是这条成立。
    #[test]
    fn same_block_size_is_bit_exact() {
        let input = sine(196.0, 32_000);
        let mut a = Corrector::new(CorrectorConfig::default());
        let mut b = Corrector::new(CorrectorConfig::default());
        let out_a = run(&mut a, &input, 256);
        let out_b = run(&mut b, &input, 256);
        assert_eq!(out_a, out_b, "同块大小两次运行结果不一致：存在非确定性");
    }

    /// 不同块大小的输出应当**听感等价**，但不要求逐位相同。
    ///
    /// # 为什么不是逐位相同
    ///
    /// 第一个基音标记的播种位置取自"当前可用数据的末尾"，而这个位置
    /// 天然随块大小变化。此后整个基音标记网格都会带上这个亚采样级的偏移。
    ///
    /// 实测差异约为 0.8 个样本的时移（峰值差 ~0.02，约 -34 dB），
    /// 完全不可闻。要消掉它得把标记网格锚定到绝对位置的固定栅格上，
    /// 但周期本身是随唱变化的，栅格也就跟着变 —— 代价远大于收益。
    ///
    /// **生产环境里块大小由设备固定**，所以这个差异永远不会在同一台机器上出现。
    #[test]
    fn different_block_sizes_are_perceptually_equivalent() {
        let input = sine(196.0, 32_000);
        let mut a = Corrector::new(CorrectorConfig::default());
        let mut b = Corrector::new(CorrectorConfig::default());
        let out_a = run(&mut a, &input, 64);
        let out_b = run(&mut b, &input, 480); // 非 2 的幂，且不是 hop 的整数倍

        // 跳过预热段
        let skip = 8_000;
        let mut diff_energy = 0.0f64;
        let mut sig_energy = 0.0f64;
        for (x, y) in out_a[skip..].iter().zip(out_b[skip..].iter()) {
            diff_energy += ((x - y) as f64).powi(2);
            sig_energy += (*x as f64).powi(2);
        }
        let rel = (diff_energy / sig_energy.max(1e-12)).sqrt();
        assert!(
            rel < 0.05,
            "不同块大小的差异达到信号的 {:.1}%，超出亚采样时移能解释的范围",
            rel * 100.0
        );

        // 更重要的是：检测到的音高必须一致
        let fa = a.last_frame().f0_hz;
        let fb = b.last_frame().f0_hz;
        assert!(
            (fa - fb).abs() < 1.0,
            "不同块大小检测到不同音高：{fa:.1} vs {fb:.1} Hz"
        );
    }

    /// 角色的移调必须叠在调式修正**之上**，而不是取代它。
    #[test]
    fn character_shift_is_applied_on_top_of_retune() {
        let cfg = CorrectorConfig {
            key: Key { tonic: 0, scale: ScaleKind::Chromatic },
            ..Default::default()
        };
        let mut c = Corrector::new(cfg);
        c.set_pitch_shift(4.0);

        // 唱得正好在 A4 上，所以修正量≈0，剩下的全是角色的移调
        let input = sine(midi_to_hz(69.0), 48_000);
        let _ = run(&mut c, &input, 256);

        let applied = 1200.0 * c.last_frame().ratio.log2();
        assert!(
            (applied - 400.0).abs() < 40.0,
            "实际施加 {applied:.0} cents，期望约 +400（4 个半音）"
        );
    }

    /// 旁路必须连角色一起旁路，否则 A/B 盲测比的不是同一件事。
    #[test]
    fn bypass_also_bypasses_the_character() {
        let mut c = Corrector::new(CorrectorConfig::default());
        c.set_pitch_shift(5.0);
        c.set_formant_shift(4.0);
        c.set_bypass(true);
        let _ = run(&mut c, &sine(220.0, 32_000), 256);
        assert!(
            (c.last_frame().ratio - 1.0).abs() < 1e-6,
            "旁路状态下仍在变调：ratio={}",
            c.last_frame().ratio
        );
    }

    /// 倾斜滤波是纯 IIR，没有缓冲 —— 端到端延迟必须一个样本都不涨。
    ///
    /// 这条是它能进实时链路的唯一理由（预算只剩 0.08ms），
    /// 值得用一条断言钉死，而不是靠"我知道它不加延迟"。
    #[test]
    fn tilt_does_not_change_latency() {
        let mut c = Corrector::new(CorrectorConfig::default());
        let before = c.latency_samples();
        c.set_tilt_db_per_oct(3.5);
        assert_eq!(c.latency_samples(), before);
        assert!((c.tilt_db_per_oct() - 3.5).abs() < 1e-6);
    }

    /// 旁路必须连倾斜一起旁路，否则 A/B 比的不是同一件事。
    #[test]
    fn bypass_also_bypasses_the_tilt() {
        let input = sine(220.0, 24_000);
        let mut plain = Corrector::new(CorrectorConfig::default());
        plain.set_bypass(true);
        let mut tilted = Corrector::new(CorrectorConfig::default());
        tilted.set_bypass(true);
        tilted.set_tilt_db_per_oct(4.0);

        assert_eq!(
            run(&mut plain, &input, 256),
            run(&mut tilted, &input, 256),
            "旁路状态下倾斜仍在生效"
        );
    }

    #[test]
    fn bypass_keeps_latency_identical() {
        let input = sine(196.0, 32_000);
        let mut on = Corrector::new(CorrectorConfig::default());
        let mut off = Corrector::new(CorrectorConfig::default());
        off.set_bypass(true);
        assert_eq!(on.latency_samples(), off.latency_samples());

        let a = run(&mut on, &input, 256);
        let b = run(&mut off, &input, 256);
        assert_eq!(a.len(), b.len());
    }

    #[test]
    fn silence_stays_silent_and_finite() {
        let mut c = Corrector::new(CorrectorConfig::default());
        let out = run(&mut c, &vec![0.0; 24_000], 256);
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(out.iter().all(|s| s.abs() < 1e-6));
        assert!(!c.last_frame().is_voiced);
    }

    #[test]
    fn reports_clipping() {
        let mut c = Corrector::new(CorrectorConfig::default());
        let input: Vec<f32> = sine(220.0, 24_000).iter().map(|s| s * 1.5).collect();
        let _ = run(&mut c, &input, 256);
        assert!(c.last_frame().clipping, "削顶未被检出");
    }

    #[test]
    fn no_underruns_end_to_end() {
        let mut c = Corrector::new(CorrectorConfig::default());
        c.set_key(Key { tonic: 0, scale: ScaleKind::Major });
        // 一段扫频，模拟真实演唱的音高移动
        let n = 96_000;
        let input: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / SR;
                let f = 180.0 + 40.0 * (TAU * 0.5 * t).sin();
                (TAU * f * t).sin()
            })
            .collect();
        let out = run(&mut c, &input, 128);
        assert_eq!(c.underruns(), 0, "出现了 {} 次欠载", c.underruns());
        assert!(out.iter().all(|s| s.is_finite()));
    }
}
