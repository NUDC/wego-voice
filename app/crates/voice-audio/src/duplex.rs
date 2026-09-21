//! 双工处理核心 —— **与音频后端无关**的那一半。
//!
//! # 为什么要单独抽出来
//!
//! 我们有两个后端，而且驱动方式是相反的：
//!
//! - **cpal**：回调驱动（库来调我们），跨平台，共享模式
//! - **wasapi 独占**：线程驱动（我们等事件句柄，自己读写），仅 Windows，低延迟
//!
//! 但真正难写、真正踩过坑的那些逻辑 —— 环形缓冲桥接、时钟漂移补偿、
//! 块大小自适应、预热对齐 —— 两者**完全一样**。
//! 抄一份到第二个后端里，就等于把 Phase 0 踩过的五个坑再踩一遍。
//!
//! 所以边界划在这里：后端只负责「把采集到的单声道 f32 喂进来」和
//! 「把要播放的单声道 f32 取走」，其余全在本模块。
//!
//! # 实时安全
//!
//! [`CaptureHalf::push`] 与 [`RenderHalf::pull`] 都在音频线程上跑：
//! 无堆分配、无锁、无日志、无 panic。所有缓冲在构造时预分配。

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use voice_core::{Corrector, CorrectorConfig};

use crate::latency::ImpulseProbe;
use crate::metrics::Metrics;
use crate::params::Params;
use crate::priority;

const REL: Ordering = Ordering::Relaxed;

#[derive(Debug, Clone, Copy)]
pub struct DuplexConfig {
    pub sample_rate: u32,
    /// 期望块大小。**仅用于给缓冲定容量** —— 真实块大小由后端在运行时决定，
    /// 水位参数一律按实测值算（见 [`RenderHalf::pull`] 里的教训注释）。
    pub hint_block_frames: u32,
    /// 环形缓冲目标水位，以**渲染块**为单位。
    pub target_fill_blocks: f32,
    /// 采集侧的块大小（帧）。0 = 未知（共享模式下要跑起来才知道，按渲染块估）。
    ///
    /// # 为什么必须单独给
    ///
    /// 环形缓冲要同时吸收两侧的抖动。只按渲染块算水位，在两侧周期不同时会算错 ——
    /// 独占模式下渲染块 144、采集块 96，目标水位 144×1.5=216，
    /// 渲染一次取走 144，只剩 72 —— **比一个采集周期（96）还小**。
    /// 于是采集线程晚到一拍就必然 xrun。
    ///
    /// 10 分钟连测实测 21 次 xrun，根因就是这个。60 秒测试完全看不到。
    pub capture_block_frames: u32,
    /// PSOLA 基频下限，决定 DSP 算法延迟。
    pub f0_floor: f32,
    /// 是否把音频线程提升到实时优先级。
    pub realtime_priority: bool,
}

/// 构造一对双工半环。
pub fn new(
    cfg: DuplexConfig,
    metrics: Arc<Metrics>,
    params: Arc<Params>,
    probe: Arc<ImpulseProbe>,
) -> (CaptureHalf, RenderHalf, f32) {
    let hint = cfg.hint_block_frames as usize;

    // 容量给足：后端实际给的块可能远大于 hint（WASAPI 共享模式实测
    // 请求 256 却给 480）。宁可多占几十 KB，也不要在运行时爆缓冲。
    let ring_cap = (hint * 8).max(16_384).next_power_of_two();
    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(ring_cap);
    let scratch = (hint * 4).max(8_192);

    let mut corrector_cfg = CorrectorConfig::for_sample_rate(cfg.sample_rate as f32);
    corrector_cfg.psola.latency_f0_floor = cfg.f0_floor;
    let corrector = Corrector::new(corrector_cfg);
    let algorithmic_ms = corrector.latency_ms();
    metrics
        .algorithmic_us
        .store((algorithmic_ms * 1000.0) as u64, REL);

    let t0 = Instant::now();

    let capture = CaptureHalf {
        producer,
        metrics: metrics.clone(),
        probe: probe.clone(),
        t0,
        promoted: false,
        realtime: cfg.realtime_priority,
        sample_rate: cfg.sample_rate,
        hint_block: cfg.hint_block_frames,
        last_call: None,
    };

    let render = RenderHalf {
        consumer,
        corrector,
        metrics,
        params,
        probe,
        t0,
        pull_buf: vec![0.0; scratch],
        dsp_buf: vec![0.0; scratch],
        observed_block: 0,
        typical_block: 0,
        target_fill: 0,
        slack: 8,
        budget_us: 0,
        started: false,
        smoothed_fill: 0.0,
        target_blocks: cfg.target_fill_blocks.max(1.0),
        capture_block: cfg.capture_block_frames as usize,
        sr_f: cfg.sample_rate as f64,
        promoted: false,
        realtime: cfg.realtime_priority,
        sample_rate: cfg.sample_rate,
        hint_block: cfg.hint_block_frames,
        last_call: None,
    };

    (capture, render, algorithmic_ms)
}

/// 采集侧。后端在采集线程上调用 [`push`](Self::push)。
pub struct CaptureHalf {
    producer: rtrb::Producer<f32>,
    metrics: Arc<Metrics>,
    probe: Arc<ImpulseProbe>,
    t0: Instant,
    promoted: bool,
    realtime: bool,
    sample_rate: u32,
    hint_block: u32,
    last_call: Option<Instant>,
}

impl CaptureHalf {
    /// 喂入一块已降混的单声道采集样本。**实时安全。**
    pub fn push(&mut self, mono: &[f32]) {
        // 记录两次采集之间的间隔。
        //
        // 回调**耗时**正常不代表没问题：线程被调度器挂起时耗时统计看不到，
        // 只有间隔能暴露。实测 10 分钟有 26 次 xrun 而回调耗时峰值仅占预算 34% ——
        // 就是靠这个指标区分「我们算得慢」和「我们被饿着了」。
        let now = Instant::now();
        if let Some(prev) = self.last_call {
            let gap = now.duration_since(prev).as_micros() as u64;
            self.metrics.capture_gap_max_us.fetch_max(gap, REL);
            let nominal = mono.len() as u64 * 1_000_000 / self.sample_rate.max(1) as u64;
            if nominal > 0 && gap > nominal * 3 {
                self.metrics.capture_stalls.fetch_add(1, REL);
            }
        }
        self.last_call = Some(now);

        if self.realtime && !self.promoted {
            self.promoted = true;
            if priority::promote_audio_thread(self.hint_block, self.sample_rate) {
                self.metrics.rt_promotions.fetch_add(1, REL);
            } else {
                self.metrics.rt_failures.fetch_add(1, REL);
            }
        }

        let n = mono.len();
        self.metrics.input_callbacks.fetch_add(1, REL);
        self.metrics.input_frames.fetch_add(n as u64, REL);
        self.metrics.actual_input_block.fetch_max(n as u32, REL);

        self.probe.scan_input(mono, self.t0);

        let mut dropped = 0u64;
        for &s in mono {
            if self.producer.push(s).is_err() {
                dropped += 1;
            }
        }
        if dropped > 0 {
            self.metrics.overflows.fetch_add(dropped, REL);
        }
    }
}

/// 渲染侧。后端在渲染线程上调用 [`pull`](Self::pull)。
pub struct RenderHalf {
    consumer: rtrb::Consumer<f32>,
    corrector: Corrector,
    metrics: Arc<Metrics>,
    params: Arc<Params>,
    probe: Arc<ImpulseProbe>,
    t0: Instant,

    pull_buf: Vec<f32>,
    dsp_buf: Vec<f32>,

    observed_block: usize,
    typical_block: usize,
    target_fill: usize,
    slack: usize,
    budget_us: u64,
    started: bool,
    smoothed_fill: f32,
    target_blocks: f32,
    capture_block: usize,
    sr_f: f64,

    promoted: bool,
    realtime: bool,
    sample_rate: u32,
    hint_block: u32,
    last_call: Option<Instant>,
}

impl RenderHalf {
    /// 取出一块要播放的单声道样本。**实时安全。**
    ///
    /// 内部依次完成：块大小学习 → 启动缓冲 → 漂移补偿 → DSP → 脉冲探针。
    pub fn pull(&mut self, out: &mut [f32]) {
        let cb_start = Instant::now();

        // 同采集侧：记录间隔，用于区分「算得慢」与「被饿着」
        if let Some(prev) = self.last_call {
            let gap = cb_start.duration_since(prev).as_micros() as u64;
            self.metrics.render_gap_max_us.fetch_max(gap, REL);
            let nominal = out.len() as u64 * 1_000_000 / self.sr_f.max(1.0) as u64;
            if nominal > 0 && gap > nominal * 3 {
                self.metrics.render_stalls.fetch_add(1, REL);
            }
        }
        self.last_call = Some(cb_start);

        if self.realtime && !self.promoted {
            self.promoted = true;
            if priority::promote_audio_thread(self.hint_block, self.sample_rate) {
                self.metrics.rt_promotions.fetch_add(1, REL);
            } else {
                self.metrics.rt_failures.fetch_add(1, REL);
            }
        }

        let frames = out.len();
        self.metrics.output_callbacks.fetch_add(1, REL);
        self.metrics.output_frames.fetch_add(frames as u64, REL);

        // --- 学习设备实际给的块大小 ---
        //
        // ⚠️ 用**典型值**（滑动平均）而不是峰值来定水位。
        //
        // 实测教训：偶尔会冒出一个远大于常态的回调（本机常态 480 帧，
        // 出现过 1056 帧）。早期版本用 `fetch_max` 定水位，被这一个离群值
        // 把目标顶到 1584，报出来的延迟也跟着虚高一倍 —— 结论完全错。
        //
        // 峰值仍然要记，但只用于上报和越界检查。
        if self.typical_block == 0 {
            self.typical_block = frames;
        } else {
            self.typical_block = (self.typical_block * 15 + frames) / 16;
        }
        if self.typical_block != self.observed_block {
            self.observed_block = self.typical_block;

            // 目标水位 = 渲染块 × 系数 + **一个采集块**。
            //
            // 后面那一项是关键：渲染每次取走一整个渲染块，剩下的余量
            // 必须至少能扛住采集线程晚到一个采集周期，否则单次调度抖动
            // 就直接变成 xrun。漏掉它的代价是 10 分钟 21 次 xrun，
            // 而 60 秒测试一次都看不到。
            let cap = if self.capture_block > 0 {
                self.capture_block
            } else {
                // 共享模式下采集块未知，按渲染块估（两者通常相同）
                self.typical_block
            };
            self.target_fill =
                (self.typical_block as f32 * self.target_blocks) as usize + cap;
            // 死区取半个块：平滑后的水位不该再有块级抖动，
            // 留半块余量避免在边界上反复横跳
            self.slack = (self.typical_block / 2).max(8);
            self.budget_us = (self.typical_block as f64 * 1_000_000.0 / self.sr_f) as u64;
            self.metrics
                .actual_output_block
                .store(self.typical_block as u32, REL);
            self.metrics.budget_us.store(self.budget_us, REL);
        }
        self.metrics.max_output_block.fetch_max(frames as u32, REL);

        let fill = self.consumer.slots();

        // --- 启动缓冲期 ---
        //
        // 等环形缓冲攒够目标水位再开始输出。比"构造时按请求块大小预填"稳健得多 ——
        // 真实块大小只有跑起来才知道，预填量猜错就会在启动瞬间连续 xrun。
        if !self.started {
            if fill < self.target_fill {
                out.fill(0.0);
                self.metrics.record_ring_fill(fill as u32);
                return;
            }
            // 一次性把多余的样本排掉，把水位硬对齐到目标。
            //
            // 不这样做的话，只能靠每回调 ±1 样本的漂移补偿慢慢收敛，
            // 实测要花 5 秒、累计 517 次修正 —— 而这段收敛过程会被
            // 水位跨度统计吃进去，把"启动收敛"误报成"时钟漂移"。
            while self.consumer.slots() > self.target_fill {
                if self.consumer.pop().is_err() {
                    break;
                }
            }
            self.started = true;
            // 水位刚被硬对齐，平滑值也要一并对齐，否则 EMA 会从
            // 启动瞬间那个偏高的读数慢慢衰减，这几秒里控制器一直在丢帧。
            self.smoothed_fill = self.target_fill as f32;
            self.metrics.reset_fill_extremes();
        }

        // --- 时钟漂移补偿 ---
        //
        // ⚠️ 必须用**平滑后**的水位判断，不能用瞬时值。
        //
        // 实测教训：输入和输出没有相位锁定，瞬时水位天然在 ±1 个块之间跳。
        // 早期版本拿瞬时值比对目标，结果每个回调都判定"积压"并丢一帧 ——
        // 25 秒丢了 516 次，看起来像严重时钟漂移，其实只是在和相位抖动搏斗。
        self.metrics.record_ring_fill(fill as u32);
        if self.smoothed_fill == 0.0 {
            self.smoothed_fill = fill as f32;
        } else {
            // 时间常数约 2.5 秒：滤掉相位抖动，又能跟上真实漂移
            self.smoothed_fill += (fill as f32 - self.smoothed_fill) / 256.0;
        }
        self.metrics
            .smoothed_fill
            .store(self.smoothed_fill as u32, REL);

        let mut want = frames;
        let dev = self.smoothed_fill - self.target_fill as f32;
        if dev > self.slack as f32 {
            want += 1;
            self.metrics.drift_drops.fetch_add(1, REL);
            self.metrics.net_drift.fetch_add(1, REL);
        } else if dev < -(self.slack as f32) && frames > 1 {
            want -= 1;
            self.metrics.drift_inserts.fetch_add(1, REL);
            self.metrics.net_drift.fetch_sub(1, REL);
        }
        let want = want.min(self.pull_buf.len());

        // --- 从环形缓冲取输入 ---
        let mut got = 0usize;
        while got < want {
            match self.consumer.pop() {
                Ok(s) => {
                    self.pull_buf[got] = s;
                    got += 1;
                }
                Err(_) => break,
            }
        }
        if got < want {
            self.metrics.xruns.fetch_add(1, REL);
            for s in &mut self.pull_buf[got..want] {
                *s = 0.0;
            }
        }

        // 若做了插帧补偿，把最后一个样本重复一次凑满输出长度
        let dsp_len = frames.min(self.pull_buf.len());
        if want < dsp_len {
            let last = if want > 0 { self.pull_buf[want - 1] } else { 0.0 };
            for s in &mut self.pull_buf[want..dsp_len] {
                *s = last;
            }
        }

        // --- 参数同步（仅在变化时下发）---
        let key = self.params.key();
        if key != self.corrector.key() {
            self.corrector.set_key(key);
        }
        self.corrector.set_retune_ms(self.params.retune_ms());
        self.corrector.set_pitch_shift(self.params.pitch_shift());
        self.corrector
            .set_formant_shift(self.params.formant_shift());
        self.corrector.set_bypass(self.params.bypass.load(REL));

        // --- DSP ---
        self.corrector
            .process(&self.pull_buf[..dsp_len], &mut self.dsp_buf[..dsp_len]);
        self.metrics.record_frame(&self.corrector.last_frame());
        self.metrics
            .dsp_underruns
            .store(self.corrector.underruns(), REL);

        // --- 输出 ---
        let gain = if self.params.monitor_muted.load(REL) {
            0.0
        } else {
            self.params.monitor_gain()
        };
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = if i < dsp_len { self.dsp_buf[i] * gain } else { 0.0 };
        }

        // --- 脉冲探针（延迟测量）---
        //
        // ⚠️ 刻意放在**增益之后**，直接写进最终输出。
        //
        // 放在增益之前的话，`--mute` 会把脉冲一起归零，导致
        // `--latency --mute` 永远测不到。而不静音就没法做声学回环测量：
        // 扬声器→麦克风→DSP→扬声器 会立刻啸叫。
        //
        // 放在这里之后，耳返静音与脉冲测量可以同时进行 ——
        // 这正是无回环线时唯一可行的测法。
        self.probe.emit_into(out, self.t0);

        self.metrics
            .record_callback(cb_start.elapsed().as_micros() as u64, self.budget_us);
    }
}
