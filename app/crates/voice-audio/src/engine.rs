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
    /// 干声录音器。见 `recorder.rs` 与架构红线 2。
    ///
    /// 用共享槽位而不是直接持有：录音器的采集端要交给 `duplex` 里的
    /// `CaptureHalf`，而后端为了抢独占设备会重试 —— 每次重试都会
    /// 重建一对，成功那次留在槽位里。锁只被控制线程碰，音频线程永不加锁。
    pub recorder: crate::RecorderSlot,
}

impl AudioEngine {
    pub fn start(cfg: &EngineConfig) -> Result<Self> {
        let metrics = Arc::new(Metrics::new());
        let params = Arc::new(Params::default());
        let sr = cfg.sample_rate.unwrap_or(48_000) as f32;
        let probe = Arc::new(ImpulseProbe::new(sr));

        let recorder: crate::RecorderSlot = Default::default();

        let backend = backend::start(
            &cfg.to_backend(),
            metrics.clone(),
            params.clone(),
            probe.clone(),
            recorder.clone(),
        )?;

        Ok(Self {
            backend,
            metrics,
            params,
            probe,
            recorder,
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

    /// 开始录干声。返回落盘路径。
    ///
    /// 录的是**采集侧的原始输入**，不是耳返里那个修正过的声音 ——
    /// 架构红线 2。详见 `recorder.rs` 的模块文档。
    pub fn start_recording(&self, path: impl AsRef<std::path::Path>) -> Result<()> {
        let mut slot = self
            .recorder
            .lock()
            .map_err(|_| anyhow::anyhow!("录音器锁已中毒"))?;
        let rec = slot
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("录音器尚未就绪"))?;
        rec.start(path)
    }

    /// 停止录音，返回文件路径。会等写入线程把尾巴写完并补好 WAV 头。
    pub fn stop_recording(&self) -> Result<Option<std::path::PathBuf>> {
        let mut slot = self
            .recorder
            .lock()
            .map_err(|_| anyhow::anyhow!("录音器锁已中毒"))?;
        match slot.as_mut() {
            Some(rec) => rec.stop(),
            None => Ok(None),
        }
    }

    /// 录音状态：(是否在录, 已录秒数, 丢弃样本数, 当前文件)。
    ///
    /// 丢弃数必须一路暴露到 UI：**录音悄悄丢帧比录不上更糟** ——
    /// 用户会拿着一份有细微断裂的素材去做后续处理，而且永远查不出原因。
    pub fn recording_status(&self) -> (bool, f32, u64, Option<std::path::PathBuf>) {
        let Ok(slot) = self.recorder.lock() else {
            return (false, 0.0, 0, None);
        };
        match slot.as_ref() {
            Some(r) => (
                r.is_recording(),
                r.elapsed_secs(),
                r.state().dropped.load(std::sync::atomic::Ordering::Relaxed),
                r.current_path().map(|p| p.to_path_buf()),
            ),
            None => (false, 0.0, 0, None),
        }
    }

    /// 输出侧实际块大小（帧）。
    pub fn actual_block_frames(&self) -> u32 {
        self.backend.info().output_block_frames
    }

    pub fn buffer_size_honored(&self) -> bool {
        self.backend.info().buffer_size_honored()
    }
}
