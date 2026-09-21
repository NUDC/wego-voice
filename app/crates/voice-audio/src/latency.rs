//! 端到端往返延迟的脉冲实测。
//!
//! # 原理
//!
//! 在输出流里注入一个短促的脉冲串，在输入流里检测它回来的时刻，
//! 两者之差即往返延迟。这测的是**真实的完整链路**：
//! DSP + 环形缓冲 + 驱动 + 硬件 + 声学路径，没有任何估算成分。
//!
//! # 两种测法
//!
//! | 方式 | 怎么做 | 测到的 |
//! |---|---|---|
//! | **回环线** | 一根线把耳机口接到麦克风口 | 纯电气链路，最准，推荐 |
//! | **声学** | 把耳机贴着麦克风 | 额外含声波飞行时间（约 3ms/米），需扣除 |
//!
//! # 精度说明（重要，别拿它当精密仪器）
//!
//! 时间戳取自回调内的 [`Instant::now`]，因此包含回调调度抖动，
//! 单次测量的不确定度约在 ±1ms。对策是**连测多次取中位数**，
//! 并同时上报最小值与离散度 —— 见 [`LatencyStats`]。
//!
//! 若 Phase 0 需要更高精度，升级路径是改用 cpal 的
//! `StreamInstant`（输入输出共用同一时钟），代价是要重做跨回调的时间基准传递。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

const REL: Ordering = Ordering::Relaxed;

/// 脉冲串长度（样本）。太短了容易被环境噪声淹没，太长了起始点定位不准。
/// 3ms @48kHz。
const BURST_LEN: usize = 144;

/// 脉冲载波频率（Hz）。
///
/// ⚠️ **这个值不能想当然。**
///
/// 最初用的是"逐样本交替极性"的方波 —— 那等于 24kHz，正好落在奈奎斯特频率上。
/// 数字回环线能测到，**声学回环完全测不到**：扬声器放不出 24kHz，
/// 语音麦克风（常带 8kHz 低通）更收不到。实测 25 轮全部超时才发现。
///
/// 2kHz 在扬声器与麦克风的共同工作区正中间，且远离人声基频区，
/// 不容易和环境说话声混淆。
const BURST_HZ: f32 = 2000.0;

/// 脉冲幅度。声学路径衰减大，给足。
const BURST_AMP: f32 = 0.8;
/// 检测的绝对下限：低于这个电平一律不认，避免被本底噪声触发。
const DETECT_THRESHOLD: f32 = 0.02;
/// 相对背景噪声的倍数门限，用于嘈杂环境。
const DETECT_SNR: f32 = 6.0;

/// 探针状态机。输出回调发脉冲，输入回调找脉冲。
pub struct ImpulseProbe {
    sample_rate: f32,
    /// 是否已武装（控制线程置位，等待下一次输出回调发射）。
    armed: AtomicBool,
    /// 已发射、等待检测。
    pending: AtomicBool,
    /// 发射时刻（相对引擎启动的纳秒）。
    emit_ns: AtomicU64,
    /// 最近一次测得的往返延迟（微秒）。0 = 无效。
    last_us: AtomicU64,
    /// 完成的测量次数。
    completed: AtomicU64,
    /// 背景噪声电平的跟踪值（f32 位模式）。
    noise_floor: AtomicU32,
    /// 发射后经过的样本数，用于超时放弃。
    elapsed_samples: AtomicU64,
    /// 等待期间观察到的输入峰值。
    ///
    /// 测不到时，这个数字能直接区分两种失败：
    /// 「电平根本没上来」（音量/静音问题）还是「上来了但没过阈值」（阈值问题）。
    /// 没有它就只能盲猜。
    peak_seen: AtomicU32,
}

impl ImpulseProbe {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate,
            armed: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            emit_ns: AtomicU64::new(0),
            last_us: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            noise_floor: AtomicU32::new(0.001f32.to_bits()),
            elapsed_samples: AtomicU64::new(0),
            peak_seen: AtomicU32::new(0),
        }
    }

    /// 武装一次测量。控制线程调用，非阻塞。
    pub fn arm(&self) {
        self.pending.store(false, REL);
        self.elapsed_samples.store(0, REL);
        self.peak_seen.store(0, REL);
        self.armed.store(true, REL);
    }

    /// 等待期间观察到的输入峰值电平。
    pub fn peak_seen(&self) -> f32 {
        f32::from_bits(self.peak_seen.load(REL))
    }

    /// 当前背景噪声电平。
    pub fn noise_floor(&self) -> f32 {
        f32::from_bits(self.noise_floor.load(REL))
    }

    /// 当前生效的检测阈值。
    pub fn threshold(&self) -> f32 {
        DETECT_THRESHOLD.max(self.noise_floor() * DETECT_SNR)
    }

    /// 最近一次测得的往返延迟（微秒）。0 表示尚无有效结果。
    pub fn last_us(&self) -> u64 {
        self.last_us.load(REL)
    }

    pub fn completed(&self) -> u64 {
        self.completed.load(REL)
    }

    /// 本次测量是否已经出结果（或已超时作废）。
    pub fn is_idle(&self) -> bool {
        !self.armed.load(REL) && !self.pending.load(REL)
    }

    /// 在输出缓冲开头写入脉冲串。**音频线程调用，实时安全。**
    pub fn emit_into(&self, out: &mut [f32], t0: Instant) {
        if !self.armed.swap(false, REL) {
            return;
        }
        let n = BURST_LEN.min(out.len());
        // 2kHz 正弦短促音，**突起**（不做淡入）以保证起始沿锐利，
        // 线性衰减收尾避免拖尾振铃影响下一轮测量。
        let step = std::f32::consts::TAU * BURST_HZ / self.sample_rate;
        for (i, s) in out.iter_mut().take(n).enumerate() {
            let decay = 1.0 - i as f32 / n as f32;
            *s = (step * i as f32).sin() * BURST_AMP * decay;
        }
        self.emit_ns
            .store(t0.elapsed().as_nanos() as u64, REL);
        self.elapsed_samples.store(0, REL);
        self.pending.store(true, REL);
    }

    /// 在输入缓冲里寻找脉冲。**音频线程调用，实时安全。**
    pub fn scan_input(&self, input: &[f32], t0: Instant) {
        // 无论是否在测量，都持续跟踪背景噪声电平
        let mut sum = 0.0f32;
        for &s in input {
            sum += s.abs();
        }
        if !input.is_empty() {
            let mean = sum / input.len() as f32;
            let prev = f32::from_bits(self.noise_floor.load(REL));
            // 慢速跟踪，避免被脉冲本身拉高
            let updated = prev * 0.995 + mean * 0.005;
            self.noise_floor.store(updated.to_bits(), REL);
        }

        if !self.pending.load(REL) {
            return;
        }

        // 超时保护：0.5 秒还没回来就作废，避免永远 pending
        let elapsed = self
            .elapsed_samples
            .fetch_add(input.len() as u64, REL)
            + input.len() as u64;
        if elapsed as f32 > self.sample_rate * 0.5 {
            self.pending.store(false, REL);
            return;
        }

        let floor = f32::from_bits(self.noise_floor.load(REL));
        let threshold = DETECT_THRESHOLD.max(floor * DETECT_SNR);

        // 记录等待期间的峰值，供测不到时诊断用
        let mut peak = 0.0f32;
        for &s in input {
            peak = peak.max(s.abs());
        }
        let prev_peak = f32::from_bits(self.peak_seen.load(REL));
        if peak > prev_peak {
            self.peak_seen.store(peak.to_bits(), REL);
        }

        for (i, &s) in input.iter().enumerate() {
            if s.abs() >= threshold {
                let now_ns = t0.elapsed().as_nanos() as u64;
                // 扣掉脉冲在本缓冲内的偏移，换算回它真正到达的时刻
                let offset_ns = (i as f64 / self.sample_rate as f64 * 1e9) as u64;
                let arrival = now_ns.saturating_add(offset_ns);
                let emit = self.emit_ns.load(REL);
                if arrival > emit {
                    let rt_us = (arrival - emit) / 1000;
                    self.last_us.store(rt_us, REL);
                    self.completed.fetch_add(1, REL);
                }
                self.pending.store(false, REL);
                return;
            }
        }
    }
}

/// 多次测量的统计结果。
///
/// **看中位数，不要看单次值** —— 回调调度抖动会让个别样本偏出好几毫秒。
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct LatencyStats {
    pub samples: usize,
    pub min_ms: f32,
    pub median_ms: f32,
    pub p90_ms: f32,
    pub max_ms: f32,
    /// 最大值与最小值之差。离散度大说明系统调度不稳，
    /// 本身就是个值得警惕的信号。
    pub spread_ms: f32,
}

impl LatencyStats {
    pub fn from_micros(mut v: Vec<u64>) -> Self {
        if v.is_empty() {
            return Self::default();
        }
        v.sort_unstable();
        let to_ms = |us: u64| us as f32 / 1000.0;
        let n = v.len();
        let p90_idx = ((n as f32 * 0.9).ceil() as usize).saturating_sub(1).min(n - 1);
        Self {
            samples: n,
            min_ms: to_ms(v[0]),
            median_ms: to_ms(v[n / 2]),
            p90_ms: to_ms(v[p90_idx]),
            max_ms: to_ms(v[n - 1]),
            spread_ms: to_ms(v[n - 1] - v[0]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_from_micros() {
        let s = LatencyStats::from_micros(vec![20_000, 21_000, 22_000, 23_000, 40_000]);
        assert_eq!(s.samples, 5);
        assert!((s.min_ms - 20.0).abs() < 0.01);
        assert!((s.median_ms - 22.0).abs() < 0.01);
        assert!((s.max_ms - 40.0).abs() < 0.01);
        assert!((s.spread_ms - 20.0).abs() < 0.01);
    }

    #[test]
    fn empty_stats_are_zero() {
        let s = LatencyStats::from_micros(vec![]);
        assert_eq!(s.samples, 0);
        assert_eq!(s.median_ms, 0.0);
    }

    #[test]
    fn emit_writes_burst_only_when_armed() {
        let p = ImpulseProbe::new(48_000.0);
        let mut buf = vec![0.0f32; BURST_LEN * 2];
        let t0 = Instant::now();

        // 未武装：不应写入任何东西
        p.emit_into(&mut buf, t0);
        assert!(buf.iter().all(|&s| s == 0.0));

        p.arm();
        p.emit_into(&mut buf, t0);
        assert!(buf[..BURST_LEN].iter().any(|&s| s.abs() > 0.5));
        // 脉冲串之后应保持原样
        assert!(buf[BURST_LEN..].iter().all(|&s| s == 0.0));
    }

    /// 载波频率必须落在扬声器与麦克风的共同工作区。
    ///
    /// 这条守的是一个真实的 bug：原先用逐样本交替极性的方波，
    /// 那等于奈奎斯特频率（24kHz @48k）—— 数字回环测得到，
    /// **声学回环一次都测不到**，而所有没有回环线的用户都只能走声学。
    #[test]
    fn burst_frequency_is_acoustically_reproducible() {
        assert!(
            (200.0..8_000.0).contains(&BURST_HZ),
            "载波 {BURST_HZ} Hz 超出扬声器/语音麦克风的共同工作区"
        );

        // 用过零次数反推实际频率，确认波形真的是这个频率
        let p = ImpulseProbe::new(48_000.0);
        let mut buf = vec![0.0f32; BURST_LEN];
        p.arm();
        p.emit_into(&mut buf, Instant::now());

        let crossings = buf.windows(2).filter(|w| w[0] * w[1] < 0.0).count();
        let secs = BURST_LEN as f32 / 48_000.0;
        let measured = crossings as f32 / 2.0 / secs;
        assert!(
            (measured - BURST_HZ).abs() < BURST_HZ * 0.2,
            "实际波形频率约 {measured:.0} Hz，与声称的 {BURST_HZ} Hz 不符"
        );
    }

    /// 设备块可能小于脉冲长度（独占模式下输出块只有 144 帧），
    /// 截断必须安全，且不能在末尾留下突变造成咔哒声。
    #[test]
    fn truncates_safely_for_small_blocks() {
        for block in [32usize, 64, 96, BURST_LEN] {
            let p = ImpulseProbe::new(48_000.0);
            let mut buf = vec![0.0f32; block];
            p.arm();
            p.emit_into(&mut buf, Instant::now());

            assert!(buf.iter().all(|s| s.is_finite()));
            assert!(buf.iter().any(|&s| s.abs() > 0.3), "块 {block}：脉冲太弱");
            // 末尾应已衰减到接近 0，否则截断处会有咔哒声
            let tail = buf[block - 1].abs();
            assert!(tail < 0.1, "块 {block}：末尾残留 {tail:.3}，会产生咔哒声");
        }
    }

    #[test]
    fn detects_round_trip() {
        let p = ImpulseProbe::new(48_000.0);
        let t0 = Instant::now();
        let mut out = vec![0.0f32; 128];

        p.arm();
        p.emit_into(&mut out, t0);
        assert!(!p.is_idle(), "发射后应处于 pending");

        // 模拟回环：把输出原样当输入喂回去
        p.scan_input(&out, t0);
        assert_eq!(p.completed(), 1, "应完成一次测量");
        assert!(p.is_idle(), "检测到之后应回到 idle");
    }

    #[test]
    fn times_out_when_nothing_returns() {
        let p = ImpulseProbe::new(48_000.0);
        let t0 = Instant::now();
        let mut out = vec![0.0f32; 128];
        p.arm();
        p.emit_into(&mut out, t0);

        // 持续喂静音超过 0.5 秒的样本量
        let silence = vec![0.0f32; 4800];
        for _ in 0..6 {
            p.scan_input(&silence, t0);
        }
        assert!(p.is_idle(), "超时后应放弃，不能永远 pending");
        assert_eq!(p.completed(), 0);
    }
}
