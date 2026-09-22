//! cpal 后端 —— 共享模式**兜底**。
//!
//! # 它只在一种情况下被用到
//!
//! WASAPI 独占模式开不成时（最常见：设备正被别的程序占用）。
//! 此时延迟达不到 30ms，但至少能让用户把东西跑起来。
//!
//! # 已知限制（不是 bug，是设计使然）
//!
//! cpal 硬编码 `AUDCLNT_SHAREMODE_SHARED`（0.15 与 0.18 都查过源码），
//! 而共享模式的块周期由 Windows 音频引擎决定 —— `BufferSize::Fixed` 会被直接忽略。
//! 本机实测恒定 480 帧（10ms），端到端 55ms。
//!
//! 低延迟一律走 [`super::wasapi_backend`]。
//!
//! # 这个文件将来可能整个删掉
//!
//! 项目已收窄为仅 Windows，cpal 的跨平台价值随之消失。
//! `wasapi` crate 自带共享模式，用它实现兜底即可去掉整个 cpal 依赖 ——
//! 详见 [`super`] 的模块文档。

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig};

use crate::backend::{Backend, BackendConfig, BackendInfo};
use crate::duplex::{self, DuplexConfig};
use crate::latency::ImpulseProbe;
use crate::metrics::Metrics;
use crate::params::Params;

pub struct CpalBackend {
    // Stream 不是 Send/Sync，但它必须活到后端被 drop。
    // cpal 的 Stream 在 drop 时自动停流。
    _input: cpal::Stream,
    _output: cpal::Stream,
    info: BackendInfo,
}

// SAFETY: cpal::Stream 内部持有平台句柄，不是 Send。
// 我们只在构造它的线程上创建，之后仅持有、不跨线程使用，
// drop 也发生在持有它的结构被丢弃时。把整个后端标记为 Send
// 是为了能放进 `Box<dyn Backend>`；期间不会对 Stream 做任何操作。
unsafe impl Send for CpalBackend {}

impl Backend for CpalBackend {
    fn info(&self) -> &BackendInfo {
        &self.info
    }

    fn refresh(&mut self, metrics: &Metrics) {
        use std::sync::atomic::Ordering::Relaxed;
        self.info.input_block_frames = metrics.actual_input_block.load(Relaxed);
        self.info.output_block_frames = metrics.actual_output_block.load(Relaxed);
    }
}

impl CpalBackend {
    /// 记录「为什么退到了这个后端」，供 UI 明确告知用户。
    pub fn set_fallback_reason(&mut self, reason: Option<String>) {
        self.info.fallback_reason = reason;
    }

    pub fn start(
        cfg: &BackendConfig,
        metrics: Arc<Metrics>,
        params: Arc<Params>,
        probe: Arc<ImpulseProbe>,
        recorder_slot: crate::RecorderSlot,
    ) -> Result<Self> {
        let host = cpal::default_host();

        let input_dev = pick_device(&host, cfg.input_device.as_deref(), true)
            .context("找不到可用的输入设备（麦克风）")?;
        let output_dev = pick_device(&host, cfg.output_device.as_deref(), false)
            .context("找不到可用的输出设备")?;

        let in_name = device_name(&input_dev);
        let out_name = device_name(&output_dev);

        let in_supported = input_dev
            .default_input_config()
            .context("读取输入设备默认配置失败")?;
        let out_supported = output_dev
            .default_output_config()
            .context("读取输出设备默认配置失败")?;

        // 采样率必须两边一致，否则还要再插一层重采样
        let sample_rate = cfg
            .sample_rate
            .filter(|&sr| {
                device_supports_rate(&input_dev, sr, true)
                    && device_supports_rate(&output_dev, sr, false)
            })
            .unwrap_or_else(|| out_supported.sample_rate());

        if in_supported.sample_format() != SampleFormat::F32
            || out_supported.sample_format() != SampleFormat::F32
        {
            return Err(anyhow!(
                "cpal 后端暂只支持 f32 采样格式（输入 {:?}，输出 {:?}）",
                in_supported.sample_format(),
                out_supported.sample_format()
            ));
        }

        let in_channels = in_supported.channels();
        let out_channels = out_supported.channels();

        let buffer = BufferSize::Fixed(cfg.buffer_frames);
        let in_cfg = StreamConfig {
            channels: in_channels,
            sample_rate,
            buffer_size: buffer,
        };
        let out_cfg = StreamConfig {
            channels: out_channels,
            sample_rate,
            buffer_size: buffer,
        };

        let (mut capture, mut render, algorithmic_ms) = duplex::new(
            DuplexConfig {
                sample_rate,
                hint_block_frames: cfg.buffer_frames,
                target_fill_blocks: cfg.target_fill_blocks,
                // 共享模式下采集块要跑起来才知道，交给 duplex 按渲染块估
                capture_block_frames: 0,
                f0_floor: cfg.f0_floor,
                realtime_priority: cfg.realtime_priority,
            },
            metrics,
            params,
            probe,
            recorder_slot,
        );

        // 回调里不许分配，预留足够大的暂存区
        let scratch = (cfg.buffer_frames as usize * 4).max(8_192);
        let mut downmix = vec![0.0f32; scratch];
        let mut mono_out = vec![0.0f32; scratch];

        let input_stream = input_dev
            .build_input_stream(
                in_cfg,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let ch = in_channels as usize;
                    let frames = (data.len() / ch).min(downmix.len());
                    // 多声道降混为单声道：人声只需要一路
                    for f in 0..frames {
                        let base = f * ch;
                        let mut acc = 0.0f32;
                        for c in 0..ch {
                            acc += data[base + c];
                        }
                        downmix[f] = acc / ch as f32;
                    }
                    capture.push(&downmix[..frames]);
                },
                move |e| log::error!("cpal 输入流错误：{e}"),
                None,
            )
            .context("创建输入流失败")?;

        let output_stream = output_dev
            .build_output_stream(
                out_cfg,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let ch = out_channels as usize;
                    let frames = (data.len() / ch).min(mono_out.len());
                    render.pull(&mut mono_out[..frames]);
                    for f in 0..frames {
                        let v = mono_out[f];
                        let base = f * ch;
                        for c in 0..ch {
                            data[base + c] = v;
                        }
                    }
                },
                move |e| log::error!("cpal 输出流错误：{e}"),
                None,
            )
            .context("创建输出流失败")?;

        input_stream.play().context("启动输入流失败")?;
        output_stream.play().context("启动输出流失败")?;

        Ok(Self {
            _input: input_stream,
            _output: output_stream,
            info: BackendInfo {
                backend: "cpal（共享模式）".into(),
                exclusive: false,
                host: format!("{:?}", host.id()),
                input_device: in_name,
                output_device: out_name,
                sample_rate,
                input_channels: in_channels,
                output_channels: out_channels,
                requested_buffer_frames: cfg.buffer_frames,
                input_block_frames: 0,
                output_block_frames: 0,
                input_format: "f32".into(),
                output_format: "f32".into(),
                target_fill_blocks: cfg.target_fill_blocks,
                algorithmic_ms,
                realtime_priority: cfg.realtime_priority,
                fallback_reason: None,
            },
        })
    }
}

fn pick_device(host: &cpal::Host, name: Option<&str>, input: bool) -> Option<cpal::Device> {
    if let Some(want) = name {
        let want = want.to_lowercase();
        let devices = if input {
            host.input_devices().ok()?.collect::<Vec<_>>()
        } else {
            host.output_devices().ok()?.collect::<Vec<_>>()
        };
        for d in devices {
            if device_name(&d).to_lowercase().contains(&want) {
                return Some(d);
            }
        }
        return None;
    }
    if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    }
}

fn device_supports_rate(dev: &cpal::Device, rate: u32, input: bool) -> bool {
    let configs: Vec<_> = if input {
        match dev.supported_input_configs() {
            Ok(c) => c.collect(),
            Err(_) => return false,
        }
    } else {
        match dev.supported_output_configs() {
            Ok(c) => c.collect(),
            Err(_) => return false,
        }
    };
    configs
        .iter()
        .any(|c| c.min_sample_rate() <= rate && rate <= c.max_sample_rate())
}

/// 取设备名。cpal 0.18 把 `Device::name()` 换成了 `description()`。
pub fn device_name(dev: &cpal::Device) -> String {
    dev.description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<未知>".into())
}

/// 枚举可用设备，供 UI 的设备选择与诊断页使用。
pub fn list_devices() -> Result<DeviceList> {
    let host = cpal::default_host();
    let inputs = host.input_devices()?.map(|d| device_name(&d)).collect();
    let outputs = host.output_devices()?.map(|d| device_name(&d)).collect();
    Ok(DeviceList {
        host: format!("{:?}", host.id()),
        default_input: host.default_input_device().map(|d| device_name(&d)),
        default_output: host.default_output_device().map(|d| device_name(&d)),
        inputs,
        outputs,
    })
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct DeviceList {
    pub host: String,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}
