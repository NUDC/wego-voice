//! 音频引擎 —— 后端之上的薄封装。
//!
//! 真正的逻辑分在两处：
//!
//! - [`crate::duplex`]：与后端无关的那一半（环形缓冲、漂移补偿、
//!   块大小自适应、DSP）。Phase 0 踩过的坑都在里面，两个后端共享。
//! - [`crate::backend`]：后端特定的 I/O（开设备、跑线程或回调、格式转换）。
//!
//! 本模块只负责：装配、持有、对外暴露指标与参数。

use std::sync::Arc;

use anyhow::Result;

use crate::backend::{self, Backend, BackendConfig, BackendInfo, BackendKind};
use crate::latency::ImpulseProbe;
use crate::metrics::Metrics;
use crate::params::Params;

pub use crate::backend::cpal_backend::{list_devices, DeviceList};

/// 引擎配置。字段与 [`BackendConfig`] 一一对应，是给调用方的稳定门面。
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub backend: BackendKind,
    /// 期望块大小（帧）。
    ///
    /// ⚠️ **共享模式下会被忽略** —— Windows 音频引擎按自己的设备周期给块。
    /// 独占模式下才是真正生效的周期请求。
    pub buffer_frames: u32,
    pub sample_rate: Option<u32>,
    /// 环形缓冲目标水位，以块为单位。
    pub target_fill_blocks: f32,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub realtime_priority: bool,
    /// PSOLA 基频下限，直接决定 DSP 算法延迟（= 2 × 采样率 / 该值）。
    ///
    /// **延迟与低音音质的直接权衡**：100Hz → 20ms，130Hz → 15.4ms，160Hz → 12.5ms。
    pub f0_floor: f32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            backend: BackendKind::default(),
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

impl EngineConfig {
    fn to_backend(&self) -> BackendConfig {
        BackendConfig {
            kind: self.backend,
            buffer_frames: self.buffer_frames,
            sample_rate: self.sample_rate,
            target_fill_blocks: self.target_fill_blocks,
            input_device: self.input_device.clone(),
            output_device: self.output_device.clone(),
            realtime_priority: self.realtime_priority,
            f0_floor: self.f0_floor,
        }
    }
}

pub struct AudioEngine {
    backend: Box<dyn Backend>,
    pub metrics: Arc<Metrics>,
    pub params: Arc<Params>,
    pub probe: Arc<ImpulseProbe>,
}

impl AudioEngine {
    pub fn start(cfg: &EngineConfig) -> Result<Self> {
        let metrics = Arc::new(Metrics::new());
        let params = Arc::new(Params::default());
        let sr = cfg.sample_rate.unwrap_or(48_000) as f32;
        let probe = Arc::new(ImpulseProbe::new(sr));

        let backend = backend::start(
            &cfg.to_backend(),
            metrics.clone(),
            params.clone(),
            probe.clone(),
        )?;

        Ok(Self {
            backend,
            metrics,
            params,
            probe,
        })
    }

    /// 后端启动后确定下来的事实。
    ///
    /// 调用前先 [`refresh`](Self::refresh)，否则共享模式下的实际块大小
    /// 可能还是 0（那个值要跑几个回调才知道）。
    pub fn info(&self) -> &BackendInfo {
        self.backend.info()
    }

    /// 从指标里回填那些「跑起来才知道」的字段。
    pub fn refresh(&mut self) {
        let m = self.metrics.clone();
        self.backend.refresh(&m);
    }

    /// 理论端到端延迟（毫秒）。不含驱动与硬件固有延迟，是乐观下界。
    pub fn theoretical_latency_ms(&self) -> f32 {
        self.backend.info().theoretical_latency_ms()
    }

    /// 输出侧实际块大小（帧）。
    pub fn actual_block_frames(&self) -> u32 {
        self.backend.info().output_block_frames
    }

    pub fn buffer_size_honored(&self) -> bool {
        self.backend.info().buffer_size_honored()
    }
}
