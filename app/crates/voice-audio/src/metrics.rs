//! 实时链路的观测指标。
//!
//! 全部用原子量，因为写入方是音频回调 —— 那里不许加锁（实施方案 §9.2）。
//! 读取方是控制线程与 UI，以 30~60Hz 采样。
//!
//! 单机 App 没有遥测，这些数字就是我们唯一的可观测性来源，
//! 它们直接喂给诊断页（P0-5）。

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, Ordering};

const REL: Ordering = Ordering::Relaxed;

/// f32 以位模式存进 AtomicU32，避免引入额外依赖。
#[inline]
fn store_f32(a: &AtomicU32, v: f32) {
    a.store(v.to_bits(), REL);
}

#[inline]
fn load_f32(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(REL))
}

#[derive(Default)]
pub struct Metrics {
    // ---- 吞吐 ----
    pub input_frames: AtomicU64,
    pub output_frames: AtomicU64,
    pub input_callbacks: AtomicU64,
    pub output_callbacks: AtomicU64,

    // ---- 实时健康度 ----
    /// 输出回调取不到足够输入样本的次数。**稳态必须为 0。**
    pub xruns: AtomicU64,
    /// 输入回调推不进环形缓冲（已满）的次数。
    pub overflows: AtomicU64,
    /// voice-core 内部的 OLA 欠载次数。
    pub dsp_underruns: AtomicU64,

    // ---- 时钟漂移（实施方案 §4.1 的头号工程风险）----
    /// 环形缓冲当前水位（样本数，瞬时值）。
    ///
    /// ⚠️ 瞬时水位天然带 ±1 个块的相位抖动（输入输出回调没有相位锁定），
    /// **不要拿它的极差当漂移证据** —— 那измер的是抖动不是漂移。
    pub ring_fill: AtomicU32,
    pub ring_fill_min: AtomicU32,
    pub ring_fill_max: AtomicU32,
    /// 平滑后的水位。漂移判断以它为准。
    pub smoothed_fill: AtomicU32,
    /// 漂移补偿：丢帧次数（输入快于输出）。
    pub drift_drops: AtomicU64,
    /// 漂移补偿：插帧次数（输入慢于输出）。
    pub drift_inserts: AtomicU64,
    /// 净漂移 = 丢帧 - 插帧（样本）。
    ///
    /// **这才是真正的时钟漂移指标。** 两个时钟匹配时，丢与插会大致抵消，
    /// 净值围绕 0 摆动；持续单向累积则说明两个设备的实际采样率不同，
    /// 其斜率可直接换算成 ppm。
    pub net_drift: AtomicI64,

    // ---- 设备实际给的块大小 ----
    //
    // **不等于我们请求的值。** WASAPI 共享模式会直接忽略请求，
    // 按设备周期给块。一切水位参数都必须以这两个实测值为准。
    /// 输入回调的块大小（峰值）。
    pub actual_input_block: AtomicU32,
    /// 输出回调的**典型**块大小（滑动平均）。水位参数以它为准。
    pub actual_output_block: AtomicU32,
    /// 输出回调见过的**最大**块大小。偶发离群值，只用于上报与越界检查。
    pub max_output_block: AtomicU32,
    /// 实际块大小对应的回调时间预算（微秒）。
    pub budget_us: AtomicU64,

    // ---- 实时优先级提权结果 ----
    //
    // 没有这两个数就完全看不出提权到底生效没有。
    // 提权失败的表现是「偶发爆音」，而且往往只在用户机器上复现 ——
    // 必须能一眼看出来，不能靠猜。
    /// 成功提权的音频线程数。正常应为 2（采集 + 渲染）。
    pub rt_promotions: AtomicU32,
    /// 提权失败的次数。
    pub rt_failures: AtomicU32,

    // ---- 回调耗时（xrun 的先行指标）----
    /// 输出回调的最长耗时（微秒）。超过缓冲周期就会产生 xrun。
    pub callback_max_us: AtomicU64,
    /// 耗时超过预算 50% 的回调次数 —— 比 max 更能反映"经常性紧张"。
    pub callback_over_half_budget: AtomicU64,
    // ---- 线程间隔（定位停顿来源）----
    //
    // 回调**耗时**正常，不代表没问题 —— 线程被调度器挂起时，
    // 耗时统计是看不到的，只有「两次回调之间隔了多久」能暴露。
    //
    // 正常间隔 = 一个块的时长（采集 2ms / 渲染 3ms）。
    // 若某一侧出现远超该值的间隔，就定位到是哪个线程被饿着了。
    /// 采集回调之间的最大间隔（微秒）。
    pub capture_gap_max_us: AtomicU64,
    /// 渲染回调之间的最大间隔（微秒）。
    pub render_gap_max_us: AtomicU64,
    /// 采集间隔超过 3 倍正常值的次数。
    pub capture_stalls: AtomicU64,
    /// 渲染间隔超过 3 倍正常值的次数。
    pub render_stalls: AtomicU64,

    /// 回调耗时累计与次数，用于算平均值。
    ///
    /// 平均值比峰值更能说明 CPU 余量：峰值往往是启动首帧（提权、预热、
    /// 冷缓存）一次性造成的，拿它判断稳态负载会严重高估。
    pub callback_total_us: AtomicU64,
    pub callback_samples: AtomicU64,

    // ---- 延迟 ----
    /// 实测往返延迟（微秒），见 `latency` 模块。0 = 尚未测得。
    pub measured_rt_us: AtomicU64,
    /// DSP 固定算法延迟（微秒），由 voice-core 直接给出，非测量值。
    pub algorithmic_us: AtomicU64,

    // ---- 供 UI 显示的分析帧 ----
    pub f0_hz: AtomicU32,
    pub cents_off: AtomicU32,
    pub rms: AtomicU32,
    pub target_midi: AtomicI32,
    pub voiced: AtomicBool,
    pub clipping: AtomicBool,
    /// 房间噪声本底估计（线性 RMS）。诊断页用它判断设备/环境够不够安静。
    pub noise_floor: AtomicU32,
    /// 本帧是否越过噪声门。为 false 说明 YIN 根本没跑。
    pub gate_open: AtomicBool,
}

impl Metrics {
    pub fn new() -> Self {
        let m = Self::default();
        m.ring_fill_min.store(u32::MAX, REL);
        m
    }

    /// 记录一次环形缓冲水位，同时维护最值。
    #[inline]
    pub fn record_ring_fill(&self, fill: u32) {
        self.ring_fill.store(fill, REL);
        self.ring_fill_max.fetch_max(fill, REL);
        self.ring_fill_min.fetch_min(fill, REL);
    }

    /// 记录一次输出回调耗时。`budget_us` 为该缓冲大小对应的时间预算。
    #[inline]
    pub fn record_callback(&self, elapsed_us: u64, budget_us: u64) {
        self.callback_max_us.fetch_max(elapsed_us, REL);
        self.callback_total_us.fetch_add(elapsed_us, REL);
        self.callback_samples.fetch_add(1, REL);
        if budget_us > 0 && elapsed_us * 2 > budget_us {
            self.callback_over_half_budget.fetch_add(1, REL);
        }
    }

    /// 只重置水位最值。启动缓冲期结束时调用 —— 启动瞬间的水位
    /// 不代表运行时的漂移状况，混进去会让漂移结论失真。
    #[inline]
    pub fn reset_fill_extremes(&self) {
        self.ring_fill_min.store(u32::MAX, REL);
        self.ring_fill_max.store(0, REL);
    }

    #[inline]
    pub fn record_frame(&self, f: &voice_core::AnalysisFrame) {
        store_f32(&self.f0_hz, f.f0_hz);
        store_f32(&self.cents_off, f.cents_off);
        store_f32(&self.rms, f.rms);
        self.target_midi.store(f.target_midi, REL);
        self.voiced.store(f.is_voiced, REL);
        self.clipping.store(f.clipping, REL);
        store_f32(&self.noise_floor, f.noise_floor);
        self.gate_open.store(f.gate_open, REL);
    }

    /// 快照。控制线程/UI 调用，音频线程不调用。
    pub fn snapshot(&self) -> MetricsSnapshot {
        let min = self.ring_fill_min.load(REL);
        MetricsSnapshot {
            input_frames: self.input_frames.load(REL),
            output_frames: self.output_frames.load(REL),
            input_callbacks: self.input_callbacks.load(REL),
            output_callbacks: self.output_callbacks.load(REL),
            xruns: self.xruns.load(REL),
            overflows: self.overflows.load(REL),
            dsp_underruns: self.dsp_underruns.load(REL),
            ring_fill: self.ring_fill.load(REL),
            ring_fill_min: if min == u32::MAX { 0 } else { min },
            ring_fill_max: self.ring_fill_max.load(REL),
            smoothed_fill: self.smoothed_fill.load(REL),
            drift_drops: self.drift_drops.load(REL),
            drift_inserts: self.drift_inserts.load(REL),
            net_drift: self.net_drift.load(REL),
            actual_input_block: self.actual_input_block.load(REL),
            actual_output_block: self.actual_output_block.load(REL),
            max_output_block: self.max_output_block.load(REL),
            budget_us: self.budget_us.load(REL),
            rt_promotions: self.rt_promotions.load(REL),
            rt_failures: self.rt_failures.load(REL),
            capture_gap_max_us: self.capture_gap_max_us.load(REL),
            render_gap_max_us: self.render_gap_max_us.load(REL),
            capture_stalls: self.capture_stalls.load(REL),
            render_stalls: self.render_stalls.load(REL),
            callback_max_us: self.callback_max_us.load(REL),
            callback_over_half_budget: self.callback_over_half_budget.load(REL),
            callback_avg_us: {
                let n = self.callback_samples.load(REL);
                if n > 0 {
                    self.callback_total_us.load(REL) / n
                } else {
                    0
                }
            },
            measured_rt_us: self.measured_rt_us.load(REL),
            algorithmic_us: self.algorithmic_us.load(REL),
            f0_hz: load_f32(&self.f0_hz),
            cents_off: load_f32(&self.cents_off),
            rms: load_f32(&self.rms),
            target_midi: self.target_midi.load(REL),
            voiced: self.voiced.load(REL),
            clipping: self.clipping.load(REL),
            noise_floor: load_f32(&self.noise_floor),
            gate_open: self.gate_open.load(REL),
        }
    }

    /// 重置累计量。开始一轮新测量前调用。
    pub fn reset(&self) {
        self.xruns.store(0, REL);
        self.overflows.store(0, REL);
        self.dsp_underruns.store(0, REL);
        self.drift_drops.store(0, REL);
        self.drift_inserts.store(0, REL);
        self.net_drift.store(0, REL);
        self.callback_max_us.store(0, REL);
        self.callback_over_half_budget.store(0, REL);
        self.callback_total_us.store(0, REL);
        self.callback_samples.store(0, REL);
        self.capture_gap_max_us.store(0, REL);
        self.render_gap_max_us.store(0, REL);
        self.capture_stalls.store(0, REL);
        self.render_stalls.store(0, REL);
        self.ring_fill_min.store(u32::MAX, REL);
        self.ring_fill_max.store(0, REL);
    }
}

#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct MetricsSnapshot {
    pub input_frames: u64,
    pub output_frames: u64,
    pub input_callbacks: u64,
    pub output_callbacks: u64,
    pub xruns: u64,
    pub overflows: u64,
    pub dsp_underruns: u64,
    pub ring_fill: u32,
    pub ring_fill_min: u32,
    pub ring_fill_max: u32,
    pub smoothed_fill: u32,
    pub drift_drops: u64,
    pub drift_inserts: u64,
    pub net_drift: i64,
    pub actual_input_block: u32,
    pub actual_output_block: u32,
    pub max_output_block: u32,
    pub budget_us: u64,
    pub rt_promotions: u32,
    pub rt_failures: u32,
    pub capture_gap_max_us: u64,
    pub render_gap_max_us: u64,
    pub capture_stalls: u64,
    pub render_stalls: u64,
    pub callback_max_us: u64,
    pub callback_over_half_budget: u64,
    pub callback_avg_us: u64,
    pub measured_rt_us: u64,
    pub algorithmic_us: u64,
    pub f0_hz: f32,
    pub cents_off: f32,
    pub rms: f32,
    pub target_midi: i32,
    pub voiced: bool,
    pub clipping: bool,
    pub noise_floor: f32,
    pub gate_open: bool,
}

impl MetricsSnapshot {
    /// 环形缓冲水位的峰谷差（样本数）。
    ///
    /// ⚠️ **这不是时钟漂移指标。** 它主要反映输入/输出回调的相位抖动，
    /// 稳态下就约等于一个块的大小，属于正常现象。
    /// 真正的漂移看 [`drift_ppm`](Self::drift_ppm)。
    pub fn ring_fill_span(&self) -> u32 {
        self.ring_fill_max.saturating_sub(self.ring_fill_min)
    }

    /// 实测时钟失配（ppm）。
    ///
    /// 由净漂移补偿量除以已处理的总时长得出：
    /// 每丢掉一个样本，意味着输入时钟比输出时钟多跑了一个样本。
    ///
    /// 参考量级：消费级声卡标称 ±100ppm 以内；两台独立设备之间
    /// 几十 ppm 很常见。50ppm 意味着 10 分钟累积 ~29ms —— 这正是
    /// 必须做漂移补偿的原因。
    ///
    /// 正值 = 输入快于输出。
    pub fn drift_ppm(&self, sample_rate: f32) -> f32 {
        let secs = self.output_frames as f32 / sample_rate;
        if secs < 1.0 {
            return 0.0;
        }
        self.net_drift as f32 / (sample_rate * secs) * 1e6
    }

    /// 若不做补偿，按当前失配率在 `minutes` 分钟内会累积多少毫秒偏差。
    ///
    /// 这个数字回答的是"补偿到底有没有必要" —— 也是实施方案 §4.1
    /// 那条 🔴 级风险的量化答案。
    pub fn uncompensated_drift_ms(&self, sample_rate: f32, minutes: f32) -> f32 {
        self.drift_ppm(sample_rate) * 1e-6 * minutes * 60.0 * 1000.0
    }

    /// 实时链路是否健康。
    pub fn is_healthy(&self) -> bool {
        self.xruns == 0 && self.overflows == 0 && self.dsp_underruns == 0
    }
}
