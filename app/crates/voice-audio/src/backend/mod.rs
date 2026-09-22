//! 音频后端抽象。
//!
//! # 为什么需要两个后端
//!
//! Phase 0 实测结论（见 `docs/Phase0-实测记录.md`）：
//!
//! - **cpal** 在 WASAPI 共享模式下恒定拿到 480 帧（10ms），端到端 55ms，
//!   远超 30ms 的 No-Go 线。查源码确认 cpal（0.15 与 0.18）硬编码
//!   `AUDCLNT_SHAREMODE_SHARED`，共享模式的周期由音频引擎决定、改不了。
//! - **WASAPI 独占模式**实开验证可拿到输入 96 帧（2ms）、输出 144 帧（3ms），
//!   I/O 合计 9.5ms，端到端 29.5ms —— 达标。
//!
//! 项目已收窄为**仅 Windows**，所以两个后端的分工也随之变了：
//!
//! | 后端 | 角色 | 何时用 |
//! |---|---|---|
//! | [`BackendKind::WasapiExclusive`] | **主力** | 默认。唯一能进 30ms 的路 |
//! | [`BackendKind::Cpal`] | **兜底** | 独占模式开不成时（设备被别的程序占着）。延迟达不到，但至少能用 |
//!
//! ⚠️ cpal 现在**只剩「兜底」这一个职责**了 —— 它原本还承担跨平台，
//! 但平台收窄后那条理由没了。
//!
//! **可以考虑的简化**：`wasapi` crate 本身也支持共享模式
//! （`EventsShared` / `PollingShared`），用它实现兜底即可**彻底去掉 cpal 依赖**，
//! 并统一成一条代码路径。额外好处是能用上 IAudioClient3 低延迟共享 ——
//! 实测本机输入设备支持到 96 帧（2ms），比 cpal 拿到的 480 帧好得多。
//! 尚未做，因为 cpal 兜底已能工作，属于净整理而非必需。
//!
//! # 边界划在哪
//!
//! 后端**只管 I/O**：开设备、跑线程或回调、做声道与采样格式转换。
//! 环形缓冲、漂移补偿、块大小自适应、DSP 全在 [`crate::duplex`] 里共享 ——
//! 那些是 Phase 0 踩过五个坑才调对的逻辑，绝不能在第二个后端里再抄一遍。

use std::sync::Arc;

use anyhow::Result;

use crate::latency::ImpulseProbe;
use crate::metrics::Metrics;
use crate::params::Params;

pub mod cpal_backend;

#[cfg(windows)]
pub mod wasapi_backend;

/// 选哪个后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendKind {
    /// 默认：先试 WASAPI 独占，开不成再退回 cpal 共享。
    ///
    /// 独占开不成最常见的原因是**设备正被别的程序占用**。
    /// 退回之后延迟达不到 30ms，诊断页会照实显示，不隐瞒。
    #[default]
    Auto,
    /// 共享模式兜底。延迟高（实测 55ms），但不独占设备、兼容性最好。
    Cpal,
    /// Windows 独占模式。低延迟，但**运行期间独占声卡**。
    #[cfg(windows)]
    WasapiExclusive,
}

impl BackendKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "cpal" | "shared" => Some(Self::Cpal),
            #[cfg(windows)]
            "wasapi" | "exclusive" | "excl" => Some(Self::WasapiExclusive),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BackendConfig {
    pub kind: BackendKind,
    /// 期望块大小（帧）。**共享模式下会被忽略**（设备说了算），
    /// 独占模式下是真正生效的周期请求。
    pub buffer_frames: u32,
    pub sample_rate: Option<u32>,
    /// 环形缓冲目标水位，以**渲染块**为单位（实际水位还会再加一个采集块）。
    ///
    /// # 2.5 是实测出来的，不是拍的
    ///
    /// 10 分钟连测 + 三档对照实验（`--target-fill`）的结果：
    ///
    /// | 取值 | xrun / 180s | 水位最低 | 端到端 |
    /// |---|---|---|---|
    /// | 1.5 | **15** | 0（被抽干） | 26.92 ms |
    /// | **2.5** | **0** | 265 | **29.92 ms** |
    /// | 3.5 | 0 | 456 | 32.92 ms |
    ///
    /// 2.5 是不产生 xrun 的最小值。低于它，偶发调度停顿会把环形缓冲抽干。
    ///
    /// ⚠️ **这个值直接决定了 `f0_floor` 的可选范围**：
    /// 水位 2.5 时 I/O 占 14.5ms，30ms 预算只剩 15.5ms 给 DSP，
    /// 对应 `f0_floor ≥ 约 130Hz`。想用更低的 f0_floor（更好的男低音音质）
    /// 就必须接受超过 30ms。两者不可兼得。
    pub target_fill_blocks: f32,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub realtime_priority: bool,
    /// PSOLA 基频下限，决定 DSP 算法延迟。
    pub f0_floor: f32,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            kind: BackendKind::default(),
            buffer_frames: 256,
            sample_rate: Some(48_000),
            target_fill_blocks: 2.5,
            input_device: None,
            output_device: None,
            realtime_priority: true,
            f0_floor: 100.0,
        }
    }
}

/// 后端启动后确定下来的事实。请求值未必被接受，以此为准。
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct BackendInfo {
    /// 后端名，直接显示给用户（诊断页要用）。
    pub backend: String,
    /// 是否独占设备。独占时系统其他声音会静音，UI 必须明确告知。
    pub exclusive: bool,
    pub host: String,
    pub input_device: String,
    pub output_device: String,
    pub sample_rate: u32,
    pub input_channels: u16,
    pub output_channels: u16,
    /// 我们请求的块大小。
    pub requested_buffer_frames: u32,
    /// 输入侧实际块大小（帧）。0 = 尚未跑起来。
    pub input_block_frames: u32,
    /// 输出侧实际块大小（帧）。
    pub output_block_frames: u32,
    /// 实际使用的采样格式描述（独占模式下常被迫退让到整数格式）。
    pub input_format: String,
    pub output_format: String,
    pub target_fill_blocks: f32,
    pub algorithmic_ms: f32,
    pub realtime_priority: bool,
    /// 若从首选后端退回了兜底后端，这里是原因。
    ///
    /// # 为什么这个必须往上报
    ///
    /// 退回一次，延迟从 29.92 ms 变成 76.10 ms —— 差了 2.5 倍，
    /// 而且直接越过 50 ms 的 DAF 阈值，产品在那一档是失效的。
    /// 早期版本只在 stderr 打一行提示，UI 上完全看不出来，
    /// 用户只会觉得"这软件好难用"，根本不知道发生了什么。
    pub fallback_reason: Option<String>,
}

impl BackendInfo {
    /// 理论端到端延迟（毫秒）。
    ///
    /// 构成：输入块 + 环形缓冲目标水位 + 输出块 + DSP，
    /// 其中 **环形水位 = 输出块 × 系数 + 输入块**。
    ///
    /// ⚠️ 两处都不能想当然：
    ///
    /// 1. **不能用单一块大小去乘** —— 独占模式下输入输出周期不同
    ///    （本机输入 96 帧、输出 144 帧）。
    /// 2. **水位里必须含一个完整输入块** —— 渲染每次取走一整个输出块，
    ///    剩下的余量要能扛住采集晚到一个周期。早期版本漏掉这一项，
    ///    10 分钟连测出现 21 次 xrun（60 秒测试看不到）。
    pub fn theoretical_latency_ms(&self) -> f32 {
        let sr = self.sample_rate as f32;
        if sr <= 0.0 {
            return 0.0;
        }
        let inb = if self.input_block_frames > 0 {
            self.input_block_frames
        } else {
            self.requested_buffer_frames
        } as f32;
        let outb = if self.output_block_frames > 0 {
            self.output_block_frames
        } else {
            self.requested_buffer_frames
        } as f32;
        let ring = outb * self.target_fill_blocks + inb;
        (inb + ring + outb) / sr * 1000.0 + self.algorithmic_ms
    }

    /// 设备是否接受了我们请求的块大小。
    pub fn buffer_size_honored(&self) -> bool {
        self.output_block_frames != 0 && self.output_block_frames == self.requested_buffer_frames
    }
}

/// 一个跑起来的后端。Drop 时负责停流 / 收线程。
pub trait Backend: Send {
    fn info(&self) -> &BackendInfo;
    /// 刷新 info 里那些要跑起来才知道的字段（实际块大小等）。
    fn refresh(&mut self, _metrics: &Metrics) {}
}

/// 按配置启动后端。
pub fn start(
    cfg: &BackendConfig,
    metrics: Arc<Metrics>,
    params: Arc<Params>,
    probe: Arc<ImpulseProbe>,
    recorder_slot: crate::RecorderSlot,
) -> Result<Box<dyn Backend>> {
    match cfg.kind {
        BackendKind::Cpal => Ok(Box::new(cpal_backend::CpalBackend::start(
            cfg,
            metrics,
            params,
            probe,
            recorder_slot,
        )?)),

        #[cfg(windows)]
        BackendKind::WasapiExclusive => Ok(Box::new(
            wasapi_backend::WasapiExclusiveBackend::start(
                cfg,
                metrics,
                params,
                probe,
                recorder_slot,
            )?,
        )),

        BackendKind::Auto => {
            let mut reason: Option<String> = None;

            #[cfg(windows)]
            {
                // 独占是唯一能进 30ms 的路，优先试 —— 而且要**重试几次**。
                //
                // 实测踩过：杀掉旧进程后立刻重启，独占设备还没被系统释放，
                // 首次尝试必然失败，于是静默退回共享模式，延迟从 29.92ms
                // 变成 76.5ms。而「刚重启应用」恰恰是最常见的场景。
                //
                // 退避重试几百毫秒就能盖住这个窗口，代价只是启动慢一点点
                // （而且只在真的失败时才会慢）。
                const RETRIES: u32 = 4;
                for attempt in 0..RETRIES {
                    match wasapi_backend::WasapiExclusiveBackend::start(
                        cfg,
                        metrics.clone(),
                        params.clone(),
                        probe.clone(),
                        // 槽位可以反复传：每次尝试都会覆盖成一对新的录音器，
                        // 成功那次留下来的才是接在真正跑起来的采集线程上的
                        recorder_slot.clone(),
                    ) {
                        Ok(b) => return Ok(Box::new(b)),
                        Err(e) => {
                            let last = attempt + 1 == RETRIES;
                            if last {
                                log::warn!(
                                    "独占模式启动失败（已重试 {RETRIES} 次），\
                                     退回共享模式，延迟会高很多：{e}"
                                );
                                reason = Some(e.to_string());
                            } else {
                                log::debug!("独占模式第 {} 次尝试失败，重试：{e}", attempt + 1);
                                std::thread::sleep(std::time::Duration::from_millis(
                                    150 * (attempt as u64 + 1),
                                ));
                            }
                        }
                    }
                }
            }

            // 退回兜底。⚠️ 必须把原因带上去 —— 延迟会差 2.5 倍，
            // UI 得能明确告诉用户"你现在跑在降级模式上，以及为什么"。
            let mut b =
                cpal_backend::CpalBackend::start(cfg, metrics, params, probe, recorder_slot)?;
            b.set_fallback_reason(reason);
            Ok(Box::new(b))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_backend_names() {
        assert_eq!(BackendKind::parse("auto"), Some(BackendKind::Auto));
        assert_eq!(BackendKind::parse("cpal"), Some(BackendKind::Cpal));
        assert_eq!(BackendKind::parse("SHARED"), Some(BackendKind::Cpal));
        assert_eq!(BackendKind::parse("nonsense"), None);
        #[cfg(windows)]
        assert_eq!(
            BackendKind::parse("exclusive"),
            Some(BackendKind::WasapiExclusive)
        );
    }

    fn info_fill(in_frames: u32, out_frames: u32, dsp: f32, fill: f32) -> BackendInfo {
        BackendInfo {
            target_fill_blocks: fill,
            ..info(in_frames, out_frames, dsp)
        }
    }

    fn info(in_frames: u32, out_frames: u32, dsp: f32) -> BackendInfo {
        BackendInfo {
            backend: "test".into(),
            exclusive: false,
            host: "test".into(),
            input_device: "in".into(),
            output_device: "out".into(),
            sample_rate: 48_000,
            input_channels: 2,
            output_channels: 2,
            requested_buffer_frames: 256,
            input_block_frames: in_frames,
            output_block_frames: out_frames,
            input_format: "f32".into(),
            output_format: "f32".into(),
            target_fill_blocks: 2.5,
            algorithmic_ms: dsp,
            realtime_priority: true,
            fallback_reason: None,
        }
    }

    /// 复现 Phase 0 实测过的几组配置，守住延迟公式不被改坏。
    #[test]
    fn latency_formula_matches_measured_cases() {
        // ① 生产配置：独占 96/144，水位 2.5，f0_floor=130（DSP 15.42ms）
        //    10 分钟连测 0 xrun —— **这是唯一通过验证的配置**
        let prod = info_fill(96, 144, 15.42, 2.5);
        assert!(
            (prod.theoretical_latency_ms() - 29.92).abs() < 0.02,
            "生产配置应为 29.92ms，实得 {}",
            prod.theoretical_latency_ms()
        );
        assert!(prod.theoretical_latency_ms() < 30.0, "生产配置必须在 30ms 以内");

        // ② 曾经被当成"达标"的配置：水位 1.5，f0_floor=100。
        //
        //    当时报的是 29.5ms，那个数字**错了两次**：
        //      · 旧公式漏算了水位里的一个采集块 —— 真值是 31.5ms
        //      · 而且这个配置 180 秒实测 15 次 xrun（环形缓冲被抽干）
        //
        //    留在这里是为了记住：**延迟数字达标不等于配置可用**，
        //    而且算出来的数字本身也要有测试守着。
        let under_buffered = info_fill(96, 144, 20.0, 1.5);
        assert!(
            (under_buffered.theoretical_latency_ms() - 31.5).abs() < 0.02,
            "该配置真值为 31.5ms（当年误报 29.5ms），实得 {}",
            under_buffered.theoretical_latency_ms()
        );

        // ③ cpal 共享模式 480/480：完全不可用
        let shared = info_fill(480, 480, 20.0, 2.5);
        assert!(
            shared.theoretical_latency_ms() > 50.0,
            "共享模式应远超预算，实得 {}",
            shared.theoretical_latency_ms()
        );
    }

    /// 水位系数每加 1，延迟应增加正好一个输出块的时间。
    ///
    /// 这条把「缓冲深度 ↔ 延迟」的兑换率钉死：
    /// 调水位时能一眼算出代价，不必重新推公式。
    #[test]
    fn each_fill_block_costs_one_output_block() {
        let a = info_fill(96, 144, 0.0, 1.5);
        let b = info_fill(96, 144, 0.0, 2.5);
        let expected = 144.0 / 48_000.0 * 1000.0; // 3ms
        let diff = b.theoretical_latency_ms() - a.theoretical_latency_ms();
        assert!(
            (diff - expected).abs() < 0.01,
            "水位 +1 应增加 {expected:.2}ms（一个输出块），实得 {diff:.2}ms"
        );
    }

    /// 环形缓冲的目标水位里**必须含一个完整输入块**。
    ///
    /// 这条守的是一个 10 分钟连测才抓到的真实 bug：
    /// 水位只按输出块算时，渲染取走一整块后剩下的余量比一个采集周期还小，
    /// 采集线程晚到一拍就 xrun。60 秒测试完全看不到。
    #[test]
    fn ring_target_absorbs_a_full_capture_block() {
        let sr = 48_000.0f32;
        let i = info(96, 144, 0.0);
        let io_frames = i.theoretical_latency_ms() / 1000.0 * sr;

        // 渲染取走 144 之后，水位余量必须 ≥ 一个采集块（96）
        let ring = 144.0 * i.target_fill_blocks + 96.0;
        assert!(
            ring - 144.0 >= 96.0,
            "取走一个输出块后只剩 {:.0} 样本，小于采集周期 96",
            ring - 144.0
        );
        assert!((io_frames - (96.0 + ring + 144.0)).abs() < 1.0);
    }

    /// 输入块变大时，延迟应增加两份（输入项本身 + 水位里的那一份）。
    #[test]
    fn larger_capture_block_costs_twice() {
        let a = info(96, 144, 0.0);
        let b = info(144, 144, 0.0);
        let expected = 2.0 * (144.0 - 96.0) / 48_000.0 * 1000.0;
        let diff = b.theoretical_latency_ms() - a.theoretical_latency_ms();
        assert!(
            (diff - expected).abs() < 0.01,
            "期望增加 {expected:.3}ms（输入项 + 水位项各一份），实得 {diff:.3}ms"
        );
    }

    #[test]
    fn falls_back_to_requested_before_startup() {
        let pending = info(0, 0, 20.0);
        // 尚未跑起来时用请求值估算，不应返回 0 或 NaN
        assert!(pending.theoretical_latency_ms() > 20.0);
        assert!(!pending.buffer_size_honored());
    }
}
