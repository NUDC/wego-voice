//! 引擎的持有与生命周期。
//!
//! # 线程模型
//!
//! - [`AudioEngine`] 不是 `Sync`，所以放在 `Mutex` 里，只在启停时短暂加锁。
//! - **指标与参数不走这把锁** —— 它们是 `Arc<Metrics>` / `Arc<Params>`，
//!   克隆出来直接用原子量读写。否则 UI 每秒 30 次轮询就会去抢音频启停的锁。
//! - 一个独立的推送线程按固定频率把指标快照发给前端（见 [`spawn_pusher`]）。
//!
//! # 架构红线
//!
//! **采样点绝不经过这里。** IPC 只传控制指令（低频）和标量快照（30~60Hz）。
//! 48kHz / 144 帧的块意味着每秒 333 次回调、每次 3ms 预算 ——
//! Tauri 的 command/event 是 JSON 过 WebView bridge，扛不住这个速率。

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use voice_audio::{
    AudioEngine, BackendInfo, EngineConfig, ImpulseProbe, Metrics, MetricsSnapshot, Params,
};

/// 前端每帧收到的东西。
///
/// 刻意做成扁平结构：前端拿到就能直接渲染，不必二次组装。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tick {
    pub running: bool,
    pub metrics: MetricsSnapshot,
    /// 实测时钟失配（ppm）。正 = 输入快于输出。
    pub drift_ppm: f32,
    /// 若不补偿，10 分钟会累积多少毫秒偏差。
    pub uncompensated_10min_ms: f32,
    /// 理论端到端延迟（毫秒）。
    pub latency_ms: f32,
    /// 延迟档位判语，直接显示给用户。
    pub latency_verdict: String,
    /// 环形缓冲目标水位（样本），用于把当前水位画成进度条。
    pub target_fill: u32,
}

#[derive(Default)]
pub struct AppState {
    engine: Mutex<Option<AudioEngine>>,
    /// 引擎启停时同步更新的共享句柄，让推送线程无锁读取。
    shared: Mutex<Option<Shared>>,
    /// 离线任务状态。**不跟引擎走** —— 它恰恰只在引擎停止时才能跑。
    job: Arc<voice_audio::JobState>,
    /// 声线转换任务。同上，而且它跑的是一个**子进程**。
    clone: Arc<voice_audio::CloneState>,
}

#[derive(Clone)]
struct Shared {
    metrics: Arc<Metrics>,
    params: Arc<Params>,
    probe: Arc<ImpulseProbe>,
    info: BackendInfo,
    /// 录音器槽位。和 params/probe 一样是 Arc，引擎换了就跟着换。
    recorder: voice_audio::RecorderSlot,
}

impl AppState {
    /// 离线任务状态。
    ///
    /// **不放进 `shared`**：那个跟着引擎的生命周期走，而离线任务恰恰
    /// 只在引擎停止时才能跑（架构红线 3）—— 放进去就永远取不到了。
    pub fn job(&self) -> std::sync::Arc<voice_audio::JobState> {
        self.job.clone()
    }

    /// 启动离线重新校准。
    ///
    /// ⚠️ **引擎在跑时直接拒绝**（架构红线 3：推理绝不与实时音频线程抢 CPU）。
    /// 离线处理满载单核，而实时链路每 3 ms 就要交一次货 ——
    /// 同时跑必然 xrun，而失败形式是用户耳朵里的爆音。
    ///
    /// 不用"降低线程优先级"糊弄：优先级只降低概率，不消除冲突。
    ///
    /// 这个守卫放在 `AppState` 而不是 command 里，是为了让 `--selftest`
    /// 能走**同一条代码**验证它 —— 命令层要靠 WebView 才跑得起来，验不了。
    pub fn start_offline(
        &self,
        input: std::path::PathBuf,
        cfg: voice_core::RecorrectConfig,
    ) -> Result<(), String> {
        if self.is_running() {
            return Err("离线处理会占满一个核，和实时引擎抢 CPU 会导致爆音。请先在「调音」页停止引擎。".into());
        }
        voice_audio::job::start_recorrect(self.job(), input, cfg)
    }

    /// 声线转换任务状态。
    pub fn clone_job(&self) -> Arc<voice_audio::CloneState> {
        self.clone.clone()
    }

    /// 启动声线转换。
    ///
    /// ⚠️ 两道守卫，都是红线 3 的落实：
    ///
    /// 1. **引擎在跑就拒绝** —— 推理比离线校准更吃 CPU，这条只会更严。
    /// 2. **离线校准在跑也拒绝** —— 两个都是满载任务，同时跑只会让
    ///    两个都变慢，而用户看到的是两条都快不起来的进度条。
    ///
    /// 子进程那边还会再压一层优先级（`BELOW_NORMAL_PRIORITY_CLASS`），
    /// 那是万一有人绕过界面时的兜底 —— **不是**这两道守卫的替代。
    ///
    /// 和 `start_offline` 一样，守卫放在 `AppState` 而不是 command 里，
    /// 好让 `--selftest` 走同一条代码验证它。
    pub fn start_clone(
        &self,
        exe: std::path::PathBuf,
        models: std::path::PathBuf,
        input: std::path::PathBuf,
        speaker: usize,
    ) -> Result<(), String> {
        if self.is_running() {
            return Err("声线转换会占满一个核，和实时引擎抢 CPU 会导致爆音。请先在「调音」页停止引擎。".into());
        }
        if self.job.is_running() {
            return Err("离线校准正在跑。两个都是满载任务，等它跑完再来。".into());
        }
        voice_audio::clone::start(self.clone_job(), exe, models, input, speaker)
    }

    pub fn start(&self, cfg: EngineConfig) -> anyhow::Result<BackendInfo> {
        // 先停掉旧的：独占模式下设备被自己占着会导致新引擎起不来
        self.stop();

        let mut engine = AudioEngine::start(&cfg)?;
        // 共享模式下实际块大小要跑几个回调才知道，等一下再取 info
        std::thread::sleep(std::time::Duration::from_millis(300));
        engine.refresh();
        let info = engine.info().clone();

        *self.shared.lock().unwrap() = Some(Shared {
            metrics: engine.metrics.clone(),
            params: engine.params.clone(),
            probe: engine.probe.clone(),
            recorder: engine.recorder.clone(),
            info: info.clone(),
        });
        *self.engine.lock().unwrap() = Some(engine);
        Ok(info)
    }

    pub fn stop(&self) {
        // 先丢引擎（会停流、收线程），再清共享句柄
        *self.engine.lock().unwrap() = None;
        *self.shared.lock().unwrap() = None;
    }

    pub fn is_running(&self) -> bool {
        self.engine.lock().unwrap().is_some()
    }

    pub fn info(&self) -> Option<BackendInfo> {
        self.shared.lock().unwrap().as_ref().map(|s| s.info.clone())
    }

    pub fn params(&self) -> Option<Arc<Params>> {
        self.shared.lock().unwrap().as_ref().map(|s| s.params.clone())
    }

    pub fn probe(&self) -> Option<Arc<ImpulseProbe>> {
        self.shared.lock().unwrap().as_ref().map(|s| s.probe.clone())
    }

    pub fn recorder(&self) -> Option<voice_audio::RecorderSlot> {
        self.shared.lock().unwrap().as_ref().map(|s| s.recorder.clone())
    }

    pub fn metrics(&self) -> Option<Arc<Metrics>> {
        self.shared.lock().unwrap().as_ref().map(|s| s.metrics.clone())
    }

    pub fn tick(&self) -> Tick {
        let shared = self.shared.lock().unwrap().clone();
        match shared {
            Some(s) => {
                let m = s.metrics.snapshot();
                let sr = s.info.sample_rate as f32;
                let latency = s.info.theoretical_latency_ms();
                let target_fill = (s.info.output_block_frames as f32 * s.info.target_fill_blocks)
                    as u32
                    + s.info.input_block_frames;
                Tick {
                    running: true,
                    drift_ppm: m.drift_ppm(sr),
                    uncompensated_10min_ms: m.uncompensated_drift_ms(sr, 10.0),
                    latency_ms: latency,
                    latency_verdict: voice_audio::thresholds::verdict(latency).to_string(),
                    target_fill,
                    metrics: m,
                }
            }
            None => Tick {
                running: false,
                metrics: MetricsSnapshot::default(),
                drift_ppm: 0.0,
                uncompensated_10min_ms: 0.0,
                latency_ms: 0.0,
                latency_verdict: String::new(),
                target_fill: 0,
            },
        }
    }
}

/// 起一个后台线程，按固定频率把指标快照推给前端。
///
/// # 为什么是推送而不是让前端轮询
///
/// 轮询要么太慢（看不到瞬态），要么太快（每次都过一遍 IPC 往返）。
/// 推送只有单向开销，而且频率由我们控制 —— 音高条要 60Hz，
/// 诊断页其实 10Hz 就够，将来可以分成两路不同频率。
///
/// **注意这里推的全是标量**，没有任何采样点（架构红线 1）。
pub fn spawn_pusher(
    app: AppHandle,
    hz: u32,
    tray: Option<std::sync::Arc<crate::tray::Tray<tauri::Wry>>>,
) {
    let interval = std::time::Duration::from_millis((1000 / hz.max(1)).max(1) as u64);
    std::thread::spawn(move || loop {
        std::thread::sleep(interval);
        let state = app.state::<AppState>();
        let tick = state.tick();

        // 托盘跟着一起刷。它内部会去重，只在内容真的变化时才写下去 ——
        // 20Hz 地改托盘图标既浪费，也会在部分 Windows 版本上闪烁。
        if let Some(t) = &tray {
            let muted = state
                .params()
                .map(|p| p.monitor_muted.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(true);
            t.update(&tick, muted);
        }

        // 前端没监听时 emit 失败是正常的，忽略即可
        let _ = app.emit("tick", tick);
    });
}
