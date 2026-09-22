//! WASAPI 独占模式后端 —— Windows 低延迟路线。
//!
//! # 为什么要它
//!
//! cpal 在共享模式下端到端 55ms，No-Go 线是 30ms。独占模式实开验证可到
//! 输入 96 帧（2ms）、输出 144 帧（3ms），端到端 29.92ms。
//!
//! # 与 cpal 后端的结构差异
//!
//! cpal 是**回调驱动**（库来调我们）；WASAPI 独占是**线程驱动** ——
//! 我们自己起线程，等事件句柄，然后读/写设备缓冲。
//!
//! 所以这里是两个 `std::thread`，各自跑一个 `wait_for_event` 循环，
//! 分别对接 [`crate::duplex`] 的采集侧与渲染侧。
//!
//! # 两个必须处理的现实
//!
//! 1. **独占模式几乎不接受 f32。** 实测本机输入只认 `int16`、
//!    输出只认 `int24-in-32` —— 两侧还不一样。所以格式必须
//!    **按设备分别协商**，转换层也要按协商结果分派。
//! 2. **独占会独占设备。** 运行期间系统其他声音静音。
//!    工具定位下可接受，但 UI 必须明确告知。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, bail, Context, Result};
use wasapi::{
    calculate_period_100ns, initialize_mta, Device, DeviceEnumerator, Direction, SampleType,
    StreamMode, WaveFormat,
};

use crate::backend::{Backend, BackendConfig, BackendInfo};
use crate::duplex::{self, CaptureHalf, DuplexConfig, RenderHalf};
use crate::latency::ImpulseProbe;
use crate::metrics::Metrics;
use crate::params::Params;

const REL: Ordering = Ordering::Relaxed;
/// 等事件句柄的超时。超过说明设备卡死了，线程退出而不是永久挂起。
const EVENT_TIMEOUT_MS: u32 = 1000;

/// `RPC_E_CHANGED_MODE` —— 本线程的 COM 套间模型已被别人定过了。
const RPC_E_CHANGED_MODE: i32 = 0x8001_0106u32 as i32;

/// 初始化 COM，**容忍"已被初始化成别的套间"**。
///
/// # 为什么必须容忍
///
/// 这是一个只有集成之后才会暴露的 bug：
///
/// 独立的 `wego-bench` 里没人碰过 COM，`initialize_mta()` 一次成功。
/// 但在 Tauri 应用里，外壳早就把主线程初始化成 **STA** 了，
/// 于是 `initialize_mta()` 返回 `RPC_E_CHANGED_MODE`。
///
/// 早期版本把它当致命错误 → 独占模式启动失败 → **静默退回共享模式**，
/// 延迟从 29.92 ms 变成 76.10 ms。而日志只有一行"提示"，
/// 用户和开发者都很容易忽略过去。
///
/// 实际上这个返回码完全无害：WASAPI 的真正工作都在我们自己 spawn 的线程上，
/// 那些线程各自调用 `initialize_mta()`，不受主线程套间影响。
/// 这里只需要 COM **可用**，不在乎它是哪种套间。
fn init_com_tolerant() -> Result<()> {
    let hr = initialize_mta();
    if hr.is_err() && hr.0 != RPC_E_CHANGED_MODE {
        bail!("COM 初始化失败：{hr:?}");
    }
    Ok(())
}

pub struct WasapiExclusiveBackend {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    info: BackendInfo,
}

impl Backend for WasapiExclusiveBackend {
    fn info(&self) -> &BackendInfo {
        &self.info
    }

    fn refresh(&mut self, metrics: &Metrics) {
        // 独占模式下块大小是我们定的，启动时就已知；
        // 这里仍与实测值对齐，以防驱动做了调整。
        let m_in = metrics.actual_input_block.load(REL);
        let m_out = metrics.actual_output_block.load(REL);
        if m_in > 0 {
            self.info.input_block_frames = m_in;
        }
        if m_out > 0 {
            self.info.output_block_frames = m_out;
        }
    }
}

impl Drop for WasapiExclusiveBackend {
    fn drop(&mut self) {
        self.stop.store(true, REL);
        for t in self.threads.drain(..) {
            // 线程最多等一个 EVENT_TIMEOUT_MS 就会看到 stop 标志
            let _ = t.join();
        }
    }
}

/// 协商好的格式 + 周期。
struct Negotiated {
    format: WaveFormat,
    /// 每帧字节数。
    bytes_per_frame: usize,
    channels: usize,
    /// 每个样本的存储位宽（16 / 24 / 32）。
    store_bits: usize,
    sample_type: SampleType,
    frames: u32,
    period_hns: i64,
    sample_rate: usize,
    label: String,
}

impl WasapiExclusiveBackend {
    pub fn start(
        cfg: &BackendConfig,
        metrics: Arc<Metrics>,
        params: Arc<Params>,
        probe: Arc<ImpulseProbe>,
        recorder_slot: crate::RecorderSlot,
    ) -> Result<Self> {
        init_com_tolerant()?;

        let enumerator = DeviceEnumerator::new().map_err(|e| anyhow!("枚举器创建失败：{e}"))?;
        let in_dev = pick_device(&enumerator, cfg.input_device.as_deref(), Direction::Capture)
            .context("找不到可用的输入设备")?;
        let out_dev = pick_device(&enumerator, cfg.output_device.as_deref(), Direction::Render)
            .context("找不到可用的输出设备")?;

        let in_name = in_dev.get_friendlyname().unwrap_or_else(|_| "<未知>".into());
        let out_name = out_dev.get_friendlyname().unwrap_or_else(|_| "<未知>".into());

        // ---- 先协商，确定采样率与块大小 ----
        //
        // 必须在建 duplex 之前做：真实块大小决定环形缓冲容量，
        // 而独占模式下这个值是**启动前就能确定**的（不像共享模式要边跑边学）。
        let in_neg = negotiate(&in_dev, Direction::Capture, cfg)
            .context("输入设备独占模式协商失败")?;
        let out_neg = negotiate(&out_dev, Direction::Render, cfg)
            .context("输出设备独占模式协商失败")?;

        if in_neg.sample_rate != out_neg.sample_rate {
            bail!(
                "输入与输出采样率不一致（{} vs {}），独占模式下无法自动重采样",
                in_neg.sample_rate,
                out_neg.sample_rate
            );
        }
        let sample_rate = in_neg.sample_rate as u32;

        let (capture, render, algorithmic_ms) = duplex::new(
            DuplexConfig {
                sample_rate,
                // 用较大的那个块给缓冲定容量
                hint_block_frames: in_neg.frames.max(out_neg.frames),
                target_fill_blocks: cfg.target_fill_blocks,
                capture_block_frames: in_neg.frames,
                f0_floor: cfg.f0_floor,
                realtime_priority: cfg.realtime_priority,
            },
            metrics.clone(),
            params,
            probe,
            recorder_slot,
        );

        let info = BackendInfo {
            backend: "WASAPI 独占模式".into(),
            exclusive: true,
            host: "WASAPI".into(),
            input_device: in_name,
            output_device: out_name,
            sample_rate,
            input_channels: in_neg.channels as u16,
            output_channels: out_neg.channels as u16,
            requested_buffer_frames: cfg.buffer_frames,
            input_block_frames: in_neg.frames,
            output_block_frames: out_neg.frames,
            input_format: in_neg.label.clone(),
            output_format: out_neg.label.clone(),
            target_fill_blocks: cfg.target_fill_blocks,
            algorithmic_ms,
            realtime_priority: cfg.realtime_priority,
            fallback_reason: None,
        };

        // ---- 起两个线程 ----
        //
        // 设备对象不是 Send，所以不能在这里建好 client 再搬进线程。
        // 改为把「选哪个设备」的信息传进去，在线程内部重新取设备并初始化。
        let stop = Arc::new(AtomicBool::new(false));
        let ready_in = Arc::new(AtomicU32::new(0));
        let ready_out = Arc::new(AtomicU32::new(0));

        let cap_thread = spawn_capture(
            cfg.clone(),
            stop.clone(),
            ready_in.clone(),
            metrics.clone(),
            capture,
        );
        let rnd_thread = spawn_render(cfg.clone(), stop.clone(), ready_out.clone(), render);

        // 等两个线程各自初始化完（或失败），避免把失败当成功返回
        for (label, flag) in [("输入", &ready_in), ("输出", &ready_out)] {
            let mut waited = 0;
            loop {
                match flag.load(REL) {
                    0 => {}
                    1 => break,
                    _ => {
                        stop.store(true, REL);
                        bail!("{label}线程独占模式初始化失败（详见上方日志）");
                    }
                }
                thread::sleep(std::time::Duration::from_millis(10));
                waited += 10;
                if waited > 3000 {
                    stop.store(true, REL);
                    bail!("{label}线程初始化超时");
                }
            }
        }

        Ok(Self {
            stop,
            threads: vec![cap_thread, rnd_thread],
            info,
        })
    }
}

/// 在独占模式下逐级退让，找一个设备接受的格式与周期。
///
/// 退让顺序从 f32 开始，是因为它无需转换最省事；
/// 但实测两侧设备都拒绝 f32，所以实际总会落到某个整数格式上。
fn negotiate(dev: &Device, direction: Direction, cfg: &BackendConfig) -> Result<Negotiated> {
    let mut client = dev
        .get_iaudioclient()
        .map_err(|e| anyhow!("取 AudioClient 失败：{e}"))?;

    let mix = client
        .get_mixformat()
        .map_err(|e| anyhow!("取混音格式失败：{e}"))?;
    let sr = mix.get_samplespersec() as usize;
    let channels = mix.get_nchannels() as usize;

    let (_default_hns, min_hns) = client
        .get_device_period()
        .map_err(|e| anyhow!("查设备周期失败：{e}"))?;
    let min_frames = ((min_hns as f64 * sr as f64 / 1e7).round() as u32).max(32);

    let candidates = [
        (32usize, 32usize, SampleType::Float, "f32"),
        (32, 24, SampleType::Int, "int24-in-32"),
        (24, 24, SampleType::Int, "int24"),
        (16, 16, SampleType::Int, "int16"),
    ];

    for (store, valid, ty, name) in candidates {
        let want = WaveFormat::new(store, valid, &ty, sr, channels, None);
        // `..._with_quirks` 会自动试不同的声道掩码与格式变体 ——
        // 这正是独占模式最容易无声卡住的地方，别自己重写
        let Ok(fmt) = client.is_supported_exclusive_with_quirks(&want) else {
            continue;
        };

        // 从硬件最小周期开始试；驱动经常声称支持却在 Initialize 时拒绝
        for mult in [1u32, 2, 3] {
            let frames = min_frames * mult;
            let period = calculate_period_100ns(frames as i64, sr as i64);
            let mode = StreamMode::EventsExclusive { period_hns: period };
            if client.initialize_client(&fmt, &direction, &mode).is_ok() {
                let actual = client.get_buffer_size().unwrap_or(frames);
                return Ok(Negotiated {
                    bytes_per_frame: (store / 8) * channels,
                    channels,
                    store_bits: store,
                    sample_type: ty,
                    frames: actual,
                    period_hns: period,
                    sample_rate: sr,
                    label: format!("{name} @ {sr}Hz {channels}ch"),
                    format: fmt,
                });
            }
            // 初始化失败后 client 不保证可复用
            client = dev
                .get_iaudioclient()
                .map_err(|e| anyhow!("重建 AudioClient 失败：{e}"))?;
        }
    }

    let _ = cfg;
    bail!("没有任何格式/周期组合被独占模式接受（设备可能正被其他程序占用）")
}

fn pick_device(
    enumerator: &DeviceEnumerator,
    name: Option<&str>,
    direction: Direction,
) -> Option<Device> {
    if let Some(want) = name {
        let want = want.to_lowercase();
        if let Ok(coll) = enumerator.get_device_collection(&direction) {
            let n = coll.get_nbr_devices().unwrap_or(0);
            for i in 0..n {
                if let Ok(d) = coll.get_device_at_index(i) {
                    if d.get_friendlyname()
                        .map(|s| s.to_lowercase().contains(&want))
                        .unwrap_or(false)
                    {
                        return Some(d);
                    }
                }
            }
        }
        return None;
    }
    enumerator.get_default_device(&direction).ok()
}

fn spawn_capture(
    cfg: BackendConfig,
    stop: Arc<AtomicBool>,
    ready: Arc<AtomicU32>,
    metrics: Arc<Metrics>,
    mut capture: CaptureHalf,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let _ = initialize_mta();
        let mut run = || -> Result<()> {
            let enumerator =
                DeviceEnumerator::new().map_err(|e| anyhow!("枚举器创建失败：{e}"))?;
            let dev = pick_device(&enumerator, cfg.input_device.as_deref(), Direction::Capture)
                .context("找不到输入设备")?;
            let neg = negotiate(&dev, Direction::Capture, &cfg)?;

            // negotiate 内部已经 initialize 过一次，但那个 client 没带出来。
            // 这里重新按协商结果初始化一次，确保拿到的是本线程持有的 client。
            let mut client = dev
                .get_iaudioclient()
                .map_err(|e| anyhow!("取 AudioClient 失败：{e}"))?;
            client
                .initialize_client(
                    &neg.format,
                    &Direction::Capture,
                    &StreamMode::EventsExclusive {
                        period_hns: neg.period_hns,
                    },
                )
                .map_err(|e| anyhow!("独占初始化失败：{e}"))?;

            let event = client
                .set_get_eventhandle()
                .map_err(|e| anyhow!("取事件句柄失败：{e}"))?;
            let capture_client = client
                .get_audiocaptureclient()
                .map_err(|e| anyhow!("取 capture client 失败：{e}"))?;
            client
                .start_stream()
                .map_err(|e| anyhow!("启动采集流失败：{e}"))?;

            metrics.actual_input_block.store(neg.frames, REL);
            ready.store(1, REL);

            // 预分配：线程循环里不做任何分配
            let cap_frames = (neg.frames as usize) * 4;
            let mut raw = vec![0u8; cap_frames * neg.bytes_per_frame];
            let mut mono = vec![0.0f32; cap_frames];

            while !stop.load(REL) {
                if event.wait_for_event(EVENT_TIMEOUT_MS).is_err() {
                    continue; // 超时：回到循环顶部检查 stop
                }

                // ⚠️ 每次唤醒要把积压的包都读掉，不能只读一个。
                //
                // 只读一个包时，线程一旦被调度晚了一拍，设备侧就积压两个包；
                // 而事件仍按周期到来，我们永远追不回来 —— 环形缓冲被抽干，
                // 渲染侧 xrun。10 分钟连测里表现为零星单发 xrun 加偶尔一串。
                //
                // ⚠️⚠️ 但**不能用 `get_next_packet_size()` 来判断有没有积压**：
                // 那是**共享模式**的概念，独占模式下它不返回有效值，
                // 照着写会让采集直接读成 0 帧 —— 而且整条链路看起来"没报错"，
                // 只有吞吐量统计能看出输入是 0（这个坑踩过一次）。
                //
                // 独占模式下的正确做法：直接读，读到 0 帧为止。
                // 上限 4 次是防跑飞，正常情况下第 1 次就读完了。
                for _ in 0..4 {
                    let (frames, _info) = match capture_client.read_from_device(&mut raw) {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let frames = (frames as usize).min(cap_frames);
                    if frames == 0 {
                        break;
                    }
                    decode_to_mono(
                        &raw[..frames * neg.bytes_per_frame],
                        &mut mono[..frames],
                        neg.channels,
                        neg.store_bits,
                        neg.sample_type,
                    );
                    capture.push(&mono[..frames]);
                }
            }
            Ok(())
        };

        if let Err(e) = run() {
            log::error!("WASAPI 采集线程：{e}");
            ready.store(2, REL);
        }
    })
}

fn spawn_render(
    cfg: BackendConfig,
    stop: Arc<AtomicBool>,
    ready: Arc<AtomicU32>,
    mut render: RenderHalf,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let _ = initialize_mta();
        let mut run = || -> Result<()> {
            let enumerator =
                DeviceEnumerator::new().map_err(|e| anyhow!("枚举器创建失败：{e}"))?;
            let dev = pick_device(&enumerator, cfg.output_device.as_deref(), Direction::Render)
                .context("找不到输出设备")?;
            let neg = negotiate(&dev, Direction::Render, &cfg)?;

            let mut client = dev
                .get_iaudioclient()
                .map_err(|e| anyhow!("取 AudioClient 失败：{e}"))?;
            client
                .initialize_client(
                    &neg.format,
                    &Direction::Render,
                    &StreamMode::EventsExclusive {
                        period_hns: neg.period_hns,
                    },
                )
                .map_err(|e| anyhow!("独占初始化失败：{e}"))?;

            let event = client
                .set_get_eventhandle()
                .map_err(|e| anyhow!("取事件句柄失败：{e}"))?;
            let render_client = client
                .get_audiorenderclient()
                .map_err(|e| anyhow!("取 render client 失败：{e}"))?;

            let block = neg.frames as usize;
            let mut mono = vec![0.0f32; block];
            let mut raw = vec![0u8; block * neg.bytes_per_frame];

            // 独占事件模式要求：启动前先填满一个缓冲，否则立刻欠载
            mono.fill(0.0);
            encode_from_mono(
                &mono,
                &mut raw,
                neg.channels,
                neg.store_bits,
                neg.sample_type,
            );
            let _ = render_client.write_to_device(block, &raw, None);

            client
                .start_stream()
                .map_err(|e| anyhow!("启动渲染流失败：{e}"))?;
            ready.store(1, REL);

            while !stop.load(REL) {
                if event.wait_for_event(EVENT_TIMEOUT_MS).is_err() {
                    continue;
                }
                render.pull(&mut mono);
                encode_from_mono(
                    &mono,
                    &mut raw,
                    neg.channels,
                    neg.store_bits,
                    neg.sample_type,
                );
                if render_client.write_to_device(block, &raw, None).is_err() {
                    continue;
                }
            }
            Ok(())
        };

        if let Err(e) = run() {
            log::error!("WASAPI 渲染线程：{e}");
            ready.store(2, REL);
        }
    })
}

// ---------------------------------------------------------------------------
// 采样格式转换
//
// 独占模式绕过系统混音器，设备只认自己的原生格式。
// 实测本机输入只接受 int16、输出只接受 int24-in-32 —— **两侧不同**，
// 所以转换必须按各自协商的结果分派，不能写死一种。
// ---------------------------------------------------------------------------

/// 设备原生交错格式 → 单声道 f32。
fn decode_to_mono(
    raw: &[u8],
    mono: &mut [f32],
    channels: usize,
    store_bits: usize,
    ty: SampleType,
) {
    let bytes = store_bits / 8;
    let stride = bytes * channels;
    for (f, slot) in mono.iter_mut().enumerate() {
        let base = f * stride;
        let mut acc = 0.0f32;
        for c in 0..channels {
            let o = base + c * bytes;
            if o + bytes > raw.len() {
                break;
            }
            acc += decode_sample(&raw[o..o + bytes], store_bits, ty);
        }
        *slot = acc / channels as f32;
    }
}

/// 单声道 f32 → 设备原生交错格式（复制到所有声道）。
fn encode_from_mono(
    mono: &[f32],
    raw: &mut [u8],
    channels: usize,
    store_bits: usize,
    ty: SampleType,
) {
    let bytes = store_bits / 8;
    let stride = bytes * channels;
    for (f, &v) in mono.iter().enumerate() {
        let base = f * stride;
        for c in 0..channels {
            let o = base + c * bytes;
            if o + bytes > raw.len() {
                return;
            }
            encode_sample(v, &mut raw[o..o + bytes], store_bits, ty);
        }
    }
}

#[inline]
fn decode_sample(b: &[u8], store_bits: usize, ty: SampleType) -> f32 {
    match (ty, store_bits) {
        (SampleType::Float, 32) => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        (SampleType::Int, 16) => i16::from_le_bytes([b[0], b[1]]) as f32 / 32_768.0,
        (SampleType::Int, 24) => {
            // 24 位小端打包：补一个字节凑成 i32 再右移回来
            let v = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
            v as f32 / 8_388_608.0
        }
        (SampleType::Int, 32) => {
            // int24-in-32：有效位在高 24 位
            i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0
        }
        _ => 0.0,
    }
}

#[inline]
fn encode_sample(v: f32, out: &mut [u8], store_bits: usize, ty: SampleType) {
    // 钳位是必须的：DSP 输出可能略微过冲，整数格式一旦回绕
    // 就会产生刺耳的爆音（不是柔和的削顶）
    let v = v.clamp(-1.0, 1.0);
    match (ty, store_bits) {
        (SampleType::Float, 32) => out.copy_from_slice(&v.to_le_bytes()),
        (SampleType::Int, 16) => {
            let i = (v * 32_767.0) as i16;
            out.copy_from_slice(&i.to_le_bytes());
        }
        (SampleType::Int, 24) => {
            let i = (v * 8_388_607.0) as i32;
            let b = i.to_le_bytes();
            out.copy_from_slice(&b[0..3]);
        }
        (SampleType::Int, 32) => {
            let i = (v as f64 * 2_147_483_647.0) as i32;
            out.copy_from_slice(&i.to_le_bytes());
        }
        _ => out.fill(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: f32, store_bits: usize, ty: SampleType) -> f32 {
        let mut buf = vec![0u8; store_bits / 8];
        encode_sample(v, &mut buf, store_bits, ty);
        decode_sample(&buf, store_bits, ty)
    }

    /// 每种格式的往返误差必须小于该格式一个量化步长的两倍。
    #[test]
    fn sample_roundtrip_within_quantization_error() {
        let cases = [
            (16usize, SampleType::Int, 1.0 / 32_768.0),
            (24, SampleType::Int, 1.0 / 8_388_608.0),
            (32, SampleType::Int, 1.0 / 2_147_483_648.0),
            (32, SampleType::Float, 1e-7),
        ];
        for (bits, ty, step) in cases {
            for v in [-0.9f32, -0.5, -0.001, 0.0, 0.001, 0.5, 0.9] {
                let back = roundtrip(v, bits, ty);
                let err = (back - v).abs();
                assert!(
                    err <= step * 2.0,
                    "{bits}bit {ty:?}: {v} → {back}，误差 {err} 超出 {}",
                    step * 2.0
                );
            }
        }
    }

    /// 过冲必须被钳位，不能回绕 —— 回绕在整数格式上是刺耳爆音。
    #[test]
    fn clamps_instead_of_wrapping() {
        for (bits, ty) in [
            (16usize, SampleType::Int),
            (24, SampleType::Int),
            (32, SampleType::Int),
        ] {
            let hot = roundtrip(1.8, bits, ty);
            assert!(hot > 0.9, "{bits}bit 正向过冲回绕了：得到 {hot}");
            let cold = roundtrip(-1.8, bits, ty);
            assert!(cold < -0.9, "{bits}bit 负向过冲回绕了：得到 {cold}");
        }
    }

    /// 交错 → 单声道降混，以及单声道 → 交错的展开。
    #[test]
    fn interleave_roundtrip_stereo() {
        let mono_in = [0.25f32, -0.5, 0.75, 0.0];
        let channels = 2;
        let bits = 16;
        let ty = SampleType::Int;
        let mut raw = vec![0u8; mono_in.len() * channels * (bits / 8)];
        encode_from_mono(&mono_in, &mut raw, channels, bits, ty);

        let mut mono_out = vec![0.0f32; mono_in.len()];
        decode_to_mono(&raw, &mut mono_out, channels, bits, ty);

        for (a, b) in mono_in.iter().zip(mono_out.iter()) {
            assert!((a - b).abs() < 1e-3, "{a} → {b}");
        }
    }

    /// 缓冲长度不足时必须安全退出，不能越界 panic ——
    /// 这在音频线程里是致命的。
    #[test]
    fn handles_short_buffers_without_panic() {
        let mono = [0.5f32; 8];
        let mut raw = vec![0u8; 4]; // 远小于所需
        encode_from_mono(&mono, &mut raw, 2, 16, SampleType::Int);

        let raw = vec![0u8; 4];
        let mut mono_out = vec![0.0f32; 8];
        decode_to_mono(&raw, &mut mono_out, 2, 16, SampleType::Int);
    }
}
