//! 流式 TD-PSOLA（时域基音同步叠加）音高修正。
//!
//! # 为什么是 PSOLA
//!
//! 相位声码器在人声上会产生"相位涣散"（听起来发糊），神经声码器延迟太大。
//! TD-PSOLA 在时域切片、按目标周期重叠相加，算力低、延迟只有 1~2 个基音周期，
//! 且对 ±200 cents 以内的小幅修正音质很好 —— 正是实时修音需要的特性。
//!
//! # 算法延迟（重要）
//!
//! 输出位置 `p` 的样本会收到 `[p-T, p+T]` 范围内所有合成基音标记的贡献，
//! 而合成标记 `p` 又需要输入数据覆盖到 `p+T`。两者叠加，**固定算法延迟 = 2×最大周期**。
//!
//! 这是 PSOLA 的物理下限，不是实现问题。它由 [`PsolaConfig::latency_f0_floor`]
//! 决定，而这个参数是**延迟与低音音质之间的直接权衡**：
//!
//! | `latency_f0_floor` | 算法延迟 @48kHz | 代价 |
//! |---|---|---|
//! | 70 Hz | 28.6 ms | 几乎无代价，但吃光延迟预算 |
//! | 100 Hz | 20.0 ms | 低于 100Hz 的音窗口被截断，轻微失真 |
//! | 130 Hz | 15.4 ms | 男低音明显劣化 |
//!
//! **Phase 0 线 A 的一项关键产出就是定下这个值。** 先量出硬件 I/O 占掉多少，
//! 剩下的才是留给 DSP 的预算。

/// Hann 窗查找表长度。用查表代替逐样本 cos，避免在音频回调里做超越函数。
const WINDOW_TABLE: usize = 2048;

/// 保存的历史基音标记数量。输出标记只会在邻近几个输入标记里找，64 足够。
const MAX_EPOCHS: usize = 64;

#[derive(Debug, Clone, Copy)]
pub struct PsolaConfig {
    pub sample_rate: f32,
    /// 决定固定算法延迟的基频下限（见模块文档的权衡表）。
    ///
    /// 注意这**不是**检测下限：比它更低的音依然能检测和修正，
    /// 只是 PSOLA 窗口会被截断到这个周期，音质略降。
    pub latency_f0_floor: f32,
    /// 最高基频，决定最小周期，用于参数钳位。
    pub f0_max: f32,
}

impl Default for PsolaConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            latency_f0_floor: 100.0,
            f0_max: 1100.0,
        }
    }
}

pub struct Psola {
    cfg: PsolaConfig,

    /// 输入历史环形缓冲（长度为 2 的幂）。
    ring: Vec<f32>,
    /// 输出 OLA 累加环形缓冲（与 `ring` 等长）。
    accum: Vec<f32>,
    mask: usize,

    /// 已写入的输入样本总数（绝对位置）。
    write_abs: u64,
    /// `accum` 中已完成合成、不会再收到新贡献的位置上界（绝对位置）。
    gen_abs: u64,
    /// 下一个待输出的绝对位置。
    out_abs: u64,

    /// 最近的输入基音标记（绝对位置），环形存放。
    epochs: [u64; MAX_EPOCHS],
    epoch_head: usize,
    epoch_len: usize,

    /// 下一个待合成的输出基音标记位置。
    next_syn_abs: u64,
    /// 是否正处于合成（浊音）状态。
    synth_active: bool,

    /// 固定算法延迟 = 2 × max_period。
    delay: u64,
    max_period: usize,
    min_period: usize,

    /// 当前基音周期（样本数）与移调比率。
    period: usize,
    ratio: f32,
    /// 共振峰平移系数（见 [`Psola::set_formant`]）。
    formant: f32,
    voiced: bool,

    /// 输出启动前的预热标志。
    primed: bool,

    hann: Vec<f32>,

    /// 输出欠载计数（accum 还没合成到就被读了）。正常运行应恒为 0。
    pub underruns: u64,
}

impl Psola {
    pub fn new(cfg: PsolaConfig) -> Self {
        let max_period = (cfg.sample_rate / cfg.latency_f0_floor).ceil() as usize;
        let min_period = (cfg.sample_rate / cfg.f0_max).floor().max(2.0) as usize;
        let delay = 2 * max_period as u64;

        // 环形缓冲需容纳：延迟 + 一个窗口的前后展开 + 一个处理块的余量
        let needed = 4 * max_period + 8192;
        let ring_len = needed.next_power_of_two();

        let hann = (0..WINDOW_TABLE)
            .map(|i| {
                let t = i as f32 / (WINDOW_TABLE - 1) as f32;
                0.5 * (1.0 - (std::f32::consts::TAU * t).cos())
            })
            .collect();

        Self {
            cfg,
            ring: vec![0.0; ring_len],
            accum: vec![0.0; ring_len],
            mask: ring_len - 1,
            write_abs: 0,
            gen_abs: 0,
            out_abs: 0,
            epochs: [0; MAX_EPOCHS],
            epoch_head: 0,
            epoch_len: 0,
            next_syn_abs: 0,
            synth_active: false,
            delay,
            max_period,
            min_period,
            period: max_period,
            ratio: 1.0,
            formant: 1.0,
            voiced: false,
            primed: false,
            hann,
            underruns: 0,
        }
    }

    /// 固定算法延迟（样本数）。这是 PSOLA 对端到端延迟预算的贡献。
    #[inline]
    pub fn latency_samples(&self) -> u64 {
        self.delay
    }

    /// 固定算法延迟（毫秒）。
    #[inline]
    pub fn latency_ms(&self) -> f32 {
        self.delay as f32 * 1000.0 / self.cfg.sample_rate
    }

    /// 更新当前帧的基频与清浊判定。
    pub fn set_pitch(&mut self, f0_hz: f32, voiced: bool) {
        if voiced && f0_hz > 0.0 {
            let p = (self.cfg.sample_rate / f0_hz).round() as usize;
            // 低于 latency_f0_floor 的音：窗口截断到 max_period（音质换延迟）
            self.period = p.clamp(self.min_period, self.max_period);
        }
        if self.voiced && !voiced {
            // 浊音结束：丢弃合成状态，避免下一次起音被上一句污染
            self.synth_active = false;
            self.epoch_len = 0;
        }
        self.voiced = voiced;
    }

    /// 设置移调比率（输出 f0 / 输入 f0）。1.0 = 不变调。
    pub fn set_ratio(&mut self, ratio: f32) {
        // 钳位到 ±1 个八度：超出这个范围 PSOLA 必然严重失真，
        // 上游的 Retuner 已有 300 cents 限制，这里只是兜底
        self.ratio = ratio.clamp(0.5, 2.0);
    }

    /// 设置共振峰平移系数。1.0 = 不动；>1 共振峰上移（声音变"细/小"）；
    /// <1 下移（变"粗/大"）。这是"声线"的主要维度，**与音高无关**。
    ///
    /// # 为什么这不增加任何延迟
    ///
    /// PSOLA 的输出音高只由**合成标记的间距**决定，与颗粒内容无关。
    /// 所以把颗粒自身按 α 重采样，缩放的是频谱包络（共振峰），音高原封不动。
    /// 唯一的代价是取样跨度从 ±T 变成 ±T·α。
    ///
    /// # 低音处会自动减弱（刻意如此）
    ///
    /// 延迟预算 `2×max_period` 只为每侧留了 `max_period` 的取样余量。
    /// 平时 T 远小于 max_period（比如 f0=200Hz 时 T=240，而 130Hz 下限给了 369），
    /// α 到 1.5 都绰绰有余；只有唱到贴近基频下限时才会顶到边界。
    ///
    /// 顶到边界时按 `max_period / T` **就地收窄**，而不是加延迟、也不是
    /// 破坏延迟不变式：低音的声线变形会平滑地变弱。这是个自觉的取舍 ——
    /// 为了极低音的声线效果去加几毫秒延迟，会把整个产品推过 DAF 阈值。
    pub fn set_formant(&mut self, f: f32) {
        self.formant = f.clamp(0.5, 2.0);
    }

    #[inline]
    fn idx(&self, abs: u64) -> usize {
        (abs as usize) & self.mask
    }

    /// 处理一个音频块。`input` 与 `output` 长度必须一致。
    ///
    /// **实时安全**：无分配、无锁、无 panic。
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        debug_assert_eq!(input.len(), output.len());
        let n = input.len().min(output.len());

        // --- 1. 写入输入环形缓冲 ---
        for (i, &s) in input.iter().enumerate().take(n) {
            let a = self.write_abs + i as u64;
            let idx = self.idx(a);
            self.ring[idx] = s;
        }
        self.write_abs += n as u64;

        // --- 2. 生成输出 ---
        if self.voiced {
            self.detect_epochs();
            self.synthesize();
        } else {
            self.passthrough();
        }

        // --- 3. 读出 ---
        //
        // 延迟必须**严格由 write_abs 锚定**，不能让它从 gen_abs 自然产生。
        // 理由有二：
        //   1. 清音透传的 gen_abs 直接等于 write_abs，若不锚定，
        //      透传段的延迟会短于浊音段 —— 清浊切换时输出时间轴就断了
        //   2. gen_abs 的推进时机与块大小有关，不锚定的话
        //      不同块大小会产生不同的对齐（测试 `arbitrary_block_sizes` 会抓到）
        //
        // 不变式：每次 process 返回时 `out_abs == write_abs - delay`。
        if !self.primed {
            let ready = self.gen_abs + self.delay >= self.write_abs;
            if ready && self.write_abs >= self.delay + n as u64 {
                self.primed = true;
                // 本次要消费 n 个样本，反推出起点，使消费完恰好满足不变式
                self.out_abs = self.write_abs - self.delay - n as u64;

                // 预热期内 [0, out_abs) 这段 accum 已被写入却永远不会被读到。
                // 环形缓冲回绕后，这些残值会被 `+=` 叠加进新数据里，
                // 表现为几秒后突然混入一段旧声音 —— 极难排查，必须在这里清掉。
                for a in 0..self.out_abs {
                    let idx = self.idx(a);
                    self.accum[idx] = 0.0;
                }
            } else {
                output[..n].fill(0.0);
                return;
            }
        }

        for slot in output.iter_mut().take(n) {
            if self.out_abs < self.gen_abs {
                let idx = self.idx(self.out_abs);
                *slot = self.accum[idx];
                self.accum[idx] = 0.0; // 读完即清零，供下一轮 OLA 累加
                self.out_abs += 1;
            } else {
                // 合成没跟上读出。稳态下不应发生；发生了说明
                // gen_abs 的推进被卡住（多半是基音标记断流）。
                //
                // 这里同样要清零：跳过的槽位若留有残值，
                // 回绕后会污染新数据（同上面预热期的道理）。
                let idx = self.idx(self.out_abs);
                self.accum[idx] = 0.0;
                *slot = 0.0;
                self.underruns += 1;
                self.out_abs += 1;
            }
        }
        debug_assert_eq!(
            self.out_abs,
            self.write_abs - self.delay,
            "延迟不变式被破坏"
        );
    }

    /// 检测输入基音标记。
    ///
    /// 策略：从上一个标记出发，按当前周期预测下一个位置，
    /// 在 ±T/4 的窗口内取信号极大值作为实际标记。
    fn detect_epochs(&mut self) {
        let t = self.period as u64;
        let search = (self.period / 4).max(1) as u64;
        // 只接受"窗口已完整可用"的标记，后续合成就无需再检查边界
        let guard = self.max_period as u64;

        if self.epoch_len == 0 {
            // 起音：在当前可用范围的末尾附近播下第一个标记
            if self.write_abs < guard + t + search + 1 {
                return;
            }
            let seed = self.write_abs - guard - t;
            let e = self.find_peak(seed, search);
            self.push_epoch(e);
        }

        loop {
            let last = self.epochs[(self.epoch_head + MAX_EPOCHS - 1) % MAX_EPOCHS];
            let predicted = last + t;
            if predicted + search + guard > self.write_abs {
                break;
            }
            let e = self.find_peak(predicted, search);
            // 防御：标记必须严格前进，否则会死循环
            if e <= last {
                self.push_epoch(last + t);
            } else {
                self.push_epoch(e);
            }
        }
    }

    /// 在 `[center-radius, center+radius]` 内寻找信号极大值的位置。
    fn find_peak(&self, center: u64, radius: u64) -> u64 {
        let lo = center.saturating_sub(radius);
        let hi = center + radius;
        let mut best = center;
        let mut best_val = f32::NEG_INFINITY;
        let mut a = lo;
        while a <= hi {
            let v = self.ring[self.idx(a)];
            if v > best_val {
                best_val = v;
                best = a;
            }
            a += 1;
        }
        best
    }

    #[inline]
    fn push_epoch(&mut self, e: u64) {
        self.epochs[self.epoch_head] = e;
        self.epoch_head = (self.epoch_head + 1) % MAX_EPOCHS;
        self.epoch_len = (self.epoch_len + 1).min(MAX_EPOCHS);
    }

    /// 找出距 `p` 最近的输入基音标记。
    fn nearest_epoch(&self, p: u64) -> Option<u64> {
        if self.epoch_len == 0 {
            return None;
        }
        let mut best = None;
        let mut best_d = u64::MAX;
        for k in 0..self.epoch_len {
            let i = (self.epoch_head + MAX_EPOCHS - 1 - k) % MAX_EPOCHS;
            let e = self.epochs[i];
            let d = e.abs_diff(p);
            if d < best_d {
                best_d = d;
                best = Some(e);
            }
        }
        best
    }

    /// 合成：按目标周期摆放输出基音标记，逐个做加窗叠加。
    fn synthesize(&mut self) {
        let t_in = self.period;
        // 输出周期 = 输入周期 / 比率。比率 > 1（升调）→ 输出标记更密。
        let t_out = ((t_in as f32 / self.ratio).round() as usize)
            .clamp(self.min_period, self.max_period) as u64;
        let guard = self.max_period as u64;

        if !self.synth_active {
            // 进入浊音：从尚未生成的位置接上，避免覆盖已完成的样本
            self.next_syn_abs = self.gen_abs.max(self.out_abs) + guard;
            self.synth_active = true;
        }

        while self.next_syn_abs + guard <= self.write_abs {
            let Some(src) = self.nearest_epoch(self.next_syn_abs) else {
                break;
            };
            self.overlap_add(src, self.next_syn_abs, t_in);
            self.next_syn_abs += t_out;
        }

        // 合成标记 p 的贡献范围是 [p-T, p+T]，所以
        // next_syn_abs - max_period 之前的样本不会再被写入
        let done = self.next_syn_abs.saturating_sub(guard);
        if done > self.gen_abs {
            self.gen_abs = done;
        }
    }

    /// 把以 `src` 为中心、长 2T 的 Hann 加窗片段叠加到输出位置 `dst`。
    ///
    /// 共振峰平移就发生在这里：输出偏移 `u` 取输入偏移 `u·α`（线性插值）。
    /// 窗依旧铺在输出的 2T 上，所以合成标记间距不受影响 —— 音高不变。
    fn overlap_add(&mut self, src: u64, dst: u64, t: usize) {
        let len = 2 * t;
        // 取样跨度不得越过延迟预算为最低音留下的 max_period（见 set_formant）
        let alpha = self.formant.min(self.max_period as f32 / t.max(1) as f32);
        // α=1 走整数路径：既省掉插值开销，也保证不变调时逐位可复现
        let unity = (alpha - 1.0).abs() < 1e-4;

        for i in 0..len {
            // 窗函数查表：把 [0, len) 映射到 [0, WINDOW_TABLE)
            let wi = i * (WINDOW_TABLE - 1) / len.max(1);
            let w = self.hann[wi];

            let d_abs = (dst + i as u64).wrapping_sub(t as u64);

            // 不写已经输出过的位置，否则会产生咔哒声
            if d_abs < self.out_abs {
                continue;
            }

            let v = if unity {
                let s_abs = (src + i as u64).wrapping_sub(t as u64);
                self.ring[self.idx(s_abs)] * w
            } else {
                let u = (i as f32 - t as f32) * alpha;
                let base = u.floor();
                let frac = u - base;
                // src 在起音初期可能小于 t·α，用 wrapping 走绝对位置算术，
                // 越界的读取会被 idx 的掩码兜住（读到的是零区）
                let s0 = src.wrapping_add(base as i64 as u64);
                let a0 = self.ring[self.idx(s0)];
                let a1 = self.ring[self.idx(s0.wrapping_add(1))];
                (a0 + (a1 - a0) * frac) * w
            };

            let di = self.idx(d_abs);
            self.accum[di] += v;
        }
    }

    /// 清音 / 静音段：原样透传，不做任何修正。
    ///
    /// 保持与浊音段相同的延迟，这样清浊切换时输出时间轴是连续的。
    fn passthrough(&mut self) {
        self.synth_active = false;
        let limit = self.write_abs;
        while self.gen_abs < limit {
            let idx = self.idx(self.gen_abs);
            self.accum[idx] += self.ring[idx];
            self.gen_abs += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const SR: f32 = 48_000.0;

    fn cfg() -> PsolaConfig {
        PsolaConfig {
            sample_rate: SR,
            latency_f0_floor: 100.0,
            f0_max: 1100.0,
        }
    }

    /// 跑一段信号，返回输出（已跳过预热段）。
    fn run(p: &mut Psola, input: &[f32], block: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(input.len());
        let mut buf = vec![0.0; block];
        for chunk in input.chunks(block) {
            let n = chunk.len();
            p.process(chunk, &mut buf[..n]);
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    fn sine(freq: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (TAU * freq * i as f32 / SR).sin()).collect()
    }

    /// 用自相关估计一段信号的基频。
    fn estimate_f0(x: &[f32]) -> f32 {
        let tau_min = (SR / 1100.0) as usize;
        let tau_max = (SR / 70.0) as usize;
        let w = x.len().saturating_sub(tau_max);
        assert!(w > tau_max, "样本不足以估计基频");
        let mut best_tau = tau_min;
        let mut best = f32::NEG_INFINITY;
        for tau in tau_min..=tau_max {
            let mut ac = 0.0;
            let mut n0 = 0.0;
            let mut n1 = 0.0;
            for j in 0..w {
                ac += x[j] * x[j + tau];
                n0 += x[j] * x[j];
                n1 += x[j + tau] * x[j + tau];
            }
            let norm = (n0 * n1).sqrt();
            let v = if norm > 0.0 { ac / norm } else { 0.0 };
            if v > best {
                best = v;
                best_tau = tau;
            }
        }
        SR / best_tau as f32
    }

    fn cents(a: f32, b: f32) -> f32 {
        1200.0 * (a / b).log2()
    }

    /// 冲激串（模拟声门脉冲）过一个二阶谐振器。
    /// 这样得到的信号既有明确基频，也有明确共振峰 —— 正弦是测不出声线的。
    fn voiced_with_formant(f0: f32, formant_hz: f32, n: usize) -> Vec<f32> {
        let period = (SR / f0).round().max(2.0) as usize;
        let r = 0.96f32;
        let theta = TAU * formant_hz / SR;
        let (a1, a2) = (2.0 * r * theta.cos(), -r * r);
        let (mut y1, mut y2) = (0.0f32, 0.0f32);
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let x = if i % period == 0 { 1.0 } else { 0.0 };
            let y = x + a1 * y1 + a2 * y2;
            out.push(y);
            y2 = y1;
            y1 = y;
        }
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        out.iter().map(|s| s / peak.max(1e-9)).collect()
    }

    /// 300~4000 Hz 内的频谱重心（Hz）。共振峰整体平移时它跟着走。
    fn spectral_centroid(x: &[f32]) -> f32 {
        use crate::fft::{Fft, C};
        const N: usize = 8192;
        assert!(x.len() >= N, "样本不足以做频谱分析");
        let fft = Fft::new(N);
        let mut buf = vec![C::default(); N];
        for (i, slot) in buf.iter_mut().enumerate() {
            let w = 0.5 * (1.0 - (TAU * i as f32 / (N - 1) as f32).cos());
            *slot = C::new(x[i] * w, 0.0);
        }
        fft.forward(&mut buf);

        let lo = (300.0 * N as f32 / SR) as usize;
        let hi = (4000.0 * N as f32 / SR) as usize;
        let (mut num, mut den) = (0.0f64, 0.0f64);
        for (k, c) in buf.iter().enumerate().take(hi + 1).skip(lo) {
            let mag = (c.re * c.re + c.im * c.im).sqrt() as f64;
            num += k as f64 * SR as f64 / N as f64 * mag;
            den += mag;
        }
        (num / den.max(1e-12)) as f32
    }

    /// 声线的核心命题：共振峰能独立于音高移动。
    ///
    /// 这两件事必须**同时**成立，否则做出来的不是"换声线"而是"变调"。
    #[test]
    fn formant_shift_moves_envelope_but_not_pitch() {
        let input = voiced_with_formant(160.0, 900.0, 60_000);

        let mut base = Psola::new(cfg());
        base.set_pitch(160.0, true);
        base.set_ratio(1.0);
        let out0 = run(&mut base, &input, 256);
        let c0 = spectral_centroid(&out0[30_000..]);

        for &(alpha, up) in &[(1.3f32, true), (0.77f32, false)] {
            let mut p = Psola::new(cfg());
            p.set_pitch(160.0, true);
            p.set_ratio(1.0);
            p.set_formant(alpha);
            let out = run(&mut p, &input, 256);

            let c1 = spectral_centroid(&out[30_000..]);
            if up {
                assert!(c1 > c0 * 1.08, "α={alpha}：重心只从 {c0:.0} 动到 {c1:.0} Hz");
            } else {
                assert!(c1 < c0 * 0.92, "α={alpha}：重心只从 {c0:.0} 动到 {c1:.0} Hz");
            }

            let f = estimate_f0(&out[30_000..]);
            let err = cents(f, 160.0).abs();
            assert!(err < 35.0, "α={alpha} 把音高带动了 {err:.1} cents（测得 {f:.1} Hz）");
        }
    }

    /// 唱到基频下限时取样跨度会顶到延迟预算的边界。
    /// 此时必须**就地收窄**，而不是越界读、欠载或让 OLA 失衡。
    #[test]
    fn extreme_formant_near_f0_floor_stays_safe() {
        let mut p = Psola::new(cfg());
        p.set_pitch(100.0, true); // 正好等于 latency_f0_floor
        p.set_ratio(1.0);
        p.set_formant(2.0); // 会被收窄到 1.0
        let out = run(&mut p, &sine(100.0, 96_000), 128);

        assert_eq!(p.underruns, 0, "顶到边界时出现了 {} 次欠载", p.underruns);
        assert!(out.iter().all(|s| s.is_finite()));
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak < 4.0, "OLA 失衡，峰值 {peak:.2}");
    }

    /// 共振峰平移不得改变固定算法延迟 —— 这是它相对声码器的全部价值。
    #[test]
    fn formant_shift_does_not_change_latency() {
        let mut p = Psola::new(cfg());
        let before = p.latency_samples();
        p.set_formant(1.45);
        assert_eq!(p.latency_samples(), before);
    }

    #[test]
    fn reports_expected_latency() {
        let p = Psola::new(cfg());
        // floor=100Hz → max_period=480 → delay=960 → 20ms
        assert_eq!(p.latency_samples(), 960);
        assert!((p.latency_ms() - 20.0).abs() < 0.1);
    }

    #[test]
    fn unity_ratio_preserves_pitch() {
        let mut p = Psola::new(cfg());
        p.set_pitch(220.0, true);
        p.set_ratio(1.0);
        let input = sine(220.0, 48_000);
        let out = run(&mut p, &input, 256);

        let tail = &out[24_000..];
        let f = estimate_f0(tail);
        let err = cents(f, 220.0).abs();
        assert!(err < 20.0, "不变调时基频漂了 {err:.1} cents（测得 {f:.1} Hz）");
    }

    #[test]
    fn shifts_pitch_up_and_down() {
        // +200 cents 与 -200 cents，覆盖实际修音的典型幅度
        for &semis in &[2.0f32, -2.0] {
            let ratio = (semis / 12.0).exp2();
            let mut p = Psola::new(cfg());
            p.set_pitch(220.0, true);
            p.set_ratio(ratio);
            let input = sine(220.0, 48_000);
            let out = run(&mut p, &input, 256);

            let tail = &out[24_000..];
            let f = estimate_f0(tail);
            let expected = 220.0 * ratio;
            let err = cents(f, expected).abs();
            assert!(
                err < 35.0,
                "移调 {semis:+} 半音：期望 {expected:.1} Hz，测得 {f:.1} Hz（差 {err:.1} cents）"
            );
        }
    }

    #[test]
    fn unvoiced_passes_through_cleanly() {
        let mut p = Psola::new(cfg());
        p.set_pitch(0.0, false);
        let input: Vec<f32> = (0..24_000)
            .map(|i| if i % 97 == 0 { 0.7 } else { -0.1 })
            .collect();
        let out = run(&mut p, &input, 256);

        let delay = p.latency_samples() as usize;
        // 跳过预热，逐样本比对：透传必须逐位一致
        let start = delay + 1024;
        let mut max_err: f32 = 0.0;
        for i in start..(input.len() - delay) {
            max_err = max_err.max((out[i + delay] - input[i]).abs());
        }
        assert!(max_err < 1e-6, "透传不是逐位一致，最大偏差 {max_err}");
    }

    #[test]
    fn no_underruns_in_steady_state() {
        let mut p = Psola::new(cfg());
        p.set_pitch(180.0, true);
        p.set_ratio(1.05);
        let input = sine(180.0, 96_000);
        let _ = run(&mut p, &input, 128);
        assert_eq!(p.underruns, 0, "稳态运行出现了 {} 次欠载", p.underruns);
    }

    #[test]
    fn survives_voiced_unvoiced_transitions() {
        let mut p = Psola::new(cfg());
        let block = 256;
        let mut buf = vec![0.0; block];
        let voiced = sine(200.0, 12_000);
        let mut out = Vec::new();

        for round in 0..6 {
            let is_voiced = round % 2 == 0;
            p.set_pitch(if is_voiced { 200.0 } else { 0.0 }, is_voiced);
            p.set_ratio(1.06);
            let src: Vec<f32> = if is_voiced {
                voiced.clone()
            } else {
                vec![0.02; 12_000]
            };
            for chunk in src.chunks(block) {
                let n = chunk.len();
                p.process(chunk, &mut buf[..n]);
                out.extend_from_slice(&buf[..n]);
            }
        }

        assert_eq!(p.underruns, 0, "清浊切换引发了欠载");
        assert!(out.iter().all(|s| s.is_finite()), "输出出现 NaN/Inf");
        // 输出不应爆掉：OLA 叠加失衡会表现为幅度失控
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak < 4.0, "输出峰值 {peak:.2}，OLA 叠加可能失衡");
    }

    #[test]
    fn output_is_finite_for_silence() {
        let mut p = Psola::new(cfg());
        p.set_pitch(0.0, false);
        let out = run(&mut p, &vec![0.0; 12_000], 256);
        assert!(out.iter().all(|s| s.is_finite()));
    }
}
