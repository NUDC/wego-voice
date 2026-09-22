//! Tauri command 层。
//!
//! 刻意保持**很薄**：参数校验 + 转发，逻辑全在 `voice-audio` 里。
//! 这样 headless 的 `--bench` 模式与 UI 走的是同一套代码，
//! 两者测出来的数字可以直接相减得到外壳的净开销。

use serde::{Deserialize, Serialize};
use tauri::State;
use voice_audio::{
    list_devices, parse_key, BackendInfo, BackendKind, DeviceList, EngineConfig, LatencyStats,
};

use crate::characters::{self, Character, CharacterStore};
use crate::state::{AppState, Tick};

/// 前端传来的启动参数。
///
/// 全部可选：不填就用实测验证过的生产默认值
/// （WASAPI 独占、水位 2.5、f0_floor 130 —— 见 docs/Phase0-实测记录.md）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartRequest {
    pub backend: Option<String>,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub buffer_frames: Option<u32>,
    pub sample_rate: Option<u32>,
    pub target_fill_blocks: Option<f32>,
    pub f0_floor: Option<f32>,
    pub realtime_priority: Option<bool>,
}

impl StartRequest {
    fn to_config(&self) -> Result<EngineConfig, String> {
        let mut cfg = EngineConfig::default();
        if let Some(b) = &self.backend {
            cfg.backend = BackendKind::parse(b)
                .ok_or_else(|| format!("未知后端：{b}（可选 auto / cpal / wasapi）"))?;
        }
        cfg.input_device = self.input_device.clone().filter(|s| !s.is_empty());
        cfg.output_device = self.output_device.clone().filter(|s| !s.is_empty());
        if let Some(v) = self.buffer_frames {
            cfg.buffer_frames = v.clamp(32, 4096);
        }
        if let Some(v) = self.sample_rate {
            cfg.sample_rate = Some(v);
        }
        if let Some(v) = self.target_fill_blocks {
            // 下限 1.0：低于一个渲染块必然欠载。
            // 上限 6.0：再高延迟就完全失控了，属于误操作。
            cfg.target_fill_blocks = v.clamp(1.0, 6.0);
        }
        if let Some(v) = self.f0_floor {
            // 60Hz 以下已低于人声基频范围；260Hz 以上会连女声都截断
            cfg.f0_floor = v.clamp(60.0, 260.0);
        }
        if let Some(v) = self.realtime_priority {
            cfg.realtime_priority = v;
        }
        Ok(cfg)
    }
}

#[tauri::command]
pub fn devices() -> Result<DeviceList, String> {
    list_devices().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn start(state: State<AppState>, req: StartRequest) -> Result<BackendInfo, String> {
    let cfg = req.to_config()?;
    state.start(cfg).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn stop(state: State<AppState>) {
    state.stop();
}

#[tauri::command]
pub fn engine_info(state: State<AppState>) -> Option<BackendInfo> {
    state.info()
}

/// 拉一次快照。推送流之外的补充，主要给页面首次加载用。
#[tauri::command]
pub fn tick(state: State<AppState>) -> Tick {
    state.tick()
}

#[tauri::command]
pub fn reset_metrics(state: State<AppState>) {
    if let Some(m) = state.metrics() {
        m.reset();
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamUpdate {
    pub retune_ms: Option<f32>,
    /// 调名，如 "C"、"Am"、"F#m"。
    pub key: Option<String>,
    pub bypass: Option<bool>,
    pub monitor_muted: Option<bool>,
    pub monitor_gain: Option<f32>,
    /// 角色：整体移调（半音）。
    pub pitch_shift: Option<f32>,
    /// 角色：共振峰平移（半音）。
    pub formant_shift: Option<f32>,
    /// 噪声门余量（dB）。人声要高出实测本底这么多才进入音高检测。
    pub noise_gate_db: Option<f32>,
}

#[tauri::command]
pub fn set_params(state: State<AppState>, upd: ParamUpdate) -> Result<(), String> {
    let Some(p) = state.params() else {
        return Err("引擎未启动".into());
    };
    if let Some(v) = upd.retune_ms {
        p.set_retune_ms(v.clamp(0.0, 500.0));
    }
    if let Some(k) = &upd.key {
        let key = parse_key(k).ok_or_else(|| format!("无法解析调名：{k}"))?;
        p.set_key(key);
    }
    if let Some(v) = upd.bypass {
        p.bypass.store(v, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(v) = upd.monitor_muted {
        p.monitor_muted
            .store(v, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(v) = upd.monitor_gain {
        p.set_monitor_gain(v);
    }
    if let Some(v) = upd.pitch_shift {
        p.set_pitch_shift(v);
    }
    if let Some(v) = upd.formant_shift {
        p.set_formant_shift(v);
    }
    if let Some(v) = upd.noise_gate_db {
        p.set_noise_gate_db(v);
    }
    Ok(())
}

// ─────────────────────────── 录音 ───────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingStatus {
    pub recording: bool,
    pub seconds: f32,
    /// 因缓冲满而丢弃的样本数。
    ///
    /// **必须显示。** 悄悄丢帧比录不上更糟 —— 用户会拿着一份有细微断裂的
    /// 素材去做后续处理，而且永远查不出原因。
    pub dropped: u64,
    pub path: Option<String>,
}

/// 录音存放目录：`<音频目录>/wego-voice/`。
///
/// 放系统音频目录而不是 App 数据目录：录下来的是**用户的素材**，
/// 卸载软件不该把它带走，用户也该能在文件管理器里直接找到。
fn recordings_dir(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    use tauri::Manager;
    let base = app
        .path()
        .audio_dir()
        .or_else(|_| app.path().document_dir())
        .map_err(|e| format!("取音频目录失败：{e}"))?;
    Ok(base.join("wego-voice"))
}

#[tauri::command]
pub fn start_recording(
    app: tauri::AppHandle,
    state: State<AppState>,
) -> Result<RecordingStatus, String> {
    let slot = state.recorder().ok_or("引擎未启动")?;
    let dir = recordings_dir(&app)?;

    // 用 UNIX 时间戳命名。序号方案要先扫目录，而且用户删掉中间某个之后会重复。
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("系统时钟异常：{e}"))?
        .as_secs();
    let path = dir.join(voice_audio::timestamped_name(secs));

    {
        let mut g = slot.lock().map_err(|_| "录音器锁已中毒")?;
        let rec = g.as_mut().ok_or("录音器尚未就绪")?;
        rec.start(&path).map_err(|e| e.to_string())?;
    }
    Ok(recording_status(state))
}

#[tauri::command]
pub fn stop_recording(state: State<AppState>) -> Result<RecordingStatus, String> {
    let slot = state.recorder().ok_or("引擎未启动")?;
    let saved = {
        let mut g = slot.lock().map_err(|_| "录音器锁已中毒")?;
        match g.as_mut() {
            Some(rec) => rec.stop().map_err(|e| e.to_string())?,
            None => None,
        }
    };
    Ok(RecordingStatus {
        recording: false,
        seconds: 0.0,
        dropped: 0,
        path: saved.map(|p| p.to_string_lossy().into_owned()),
    })
}

#[tauri::command]
pub fn recording_status(state: State<AppState>) -> RecordingStatus {
    let none = RecordingStatus {
        recording: false,
        seconds: 0.0,
        dropped: 0,
        path: None,
    };
    let Some(slot) = state.recorder() else {
        return none;
    };
    let Ok(g) = slot.lock() else { return none };
    match g.as_ref() {
        Some(r) => RecordingStatus {
            recording: r.is_recording(),
            seconds: r.elapsed_secs(),
            dropped: r.state().dropped.load(std::sync::atomic::Ordering::Relaxed),
            path: r.current_path().map(|p| p.to_string_lossy().into_owned()),
        },
        None => none,
    }
}

/// 在文件管理器里打开录音目录。
///
/// 录完之后"文件在哪"是第一个问题，不该让用户自己去猜路径。
#[tauri::command]
pub fn reveal_recordings(app: tauri::AppHandle) -> Result<String, String> {
    let dir = recordings_dir(&app)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败：{e}"))?;
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(&dir)
            .spawn()
            .map_err(|e| format!("打开资源管理器失败：{e}"))?;
    }
    Ok(dir.to_string_lossy().into_owned())
}

// ─────────────────────────── 角色 ───────────────────────────

/// 读角色库。任何失败都退回内置角色，不向前端报错 ——
/// 配置文件坏了不该让用户连唱都唱不了。
#[tauri::command]
pub fn characters_load(app: tauri::AppHandle) -> CharacterStore {
    characters::load(&app)
}

/// 整库写回。角色数量以十计，没必要做增量。
#[tauri::command]
pub fn characters_save(app: tauri::AppHandle, store: CharacterStore) -> Result<(), String> {
    characters::save(&app, &store)
}

/// 内置角色的出厂值。前端用它做「复位」，不必自己内嵌一份副本。
#[tauri::command]
pub fn characters_builtins() -> Vec<Character> {
    characters::builtins()
}

/// 把一个角色整体下发给引擎。
///
/// 不复用 `set_params` 是因为语义不同：这里是"换角色"这一个动作，
/// 四个参数必须一起生效，不能出现调换了、共振峰还没跟上的中间态。
#[tauri::command]
pub fn apply_character(state: State<AppState>, character: Character) -> Result<(), String> {
    let Some(p) = state.params() else {
        return Err("引擎未启动".into());
    };
    let key = parse_key(&character.key)
        .ok_or_else(|| format!("无法解析调名：{}", character.key))?;
    p.set_key(key);
    p.set_retune_ms(character.retune_ms.clamp(0.0, 500.0));
    p.set_pitch_shift(character.pitch_shift);
    p.set_formant_shift(character.formant_shift);
    Ok(())
}

/// 脉冲往返延迟实测。
///
/// ⚠️ 需要回环装置：回环线、外置声卡直通，或把耳机贴住麦克风。
/// 没有的话会全部超时 —— 返回值里的诊断字段会说明卡在哪一步。
///
/// 阻塞若干秒，前端要在 UI 上给出进度提示。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LatencyResult {
    pub stats: LatencyStats,
    /// 实际测到的轮数。为 0 时看下面三个诊断值。
    pub detected: usize,
    pub requested: usize,
    /// 等待期间观察到的输入峰值电平。
    pub peak_seen: f32,
    pub noise_floor: f32,
    pub threshold: f32,
    /// 人话版的失败原因。成功时为空。
    pub diagnosis: String,
}

#[tauri::command]
pub fn measure_latency(state: State<AppState>, rounds: usize) -> Result<LatencyResult, String> {
    let Some(probe) = state.probe() else {
        return Err("引擎未启动".into());
    };
    let rounds = rounds.clamp(1, 200);
    let mut samples = Vec::with_capacity(rounds);
    let mut last_seen = probe.completed();

    for _ in 0..rounds {
        probe.arm();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(800);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(5));
            let done = probe.completed();
            if done > last_seen {
                last_seen = done;
                samples.push(probe.last_us());
                break;
            }
            if std::time::Instant::now() > deadline {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(60));
    }

    let peak = probe.peak_seen();
    let floor = probe.noise_floor();
    let thr = probe.threshold();
    let detected = samples.len();

    // 测不到时给出可操作的原因，而不是只说"超时"
    let diagnosis = if detected > 0 {
        String::new()
    } else if peak < 0.005 {
        "输入几乎没有信号。检查麦克风是否静音、输入设备是否选对。".into()
    } else if peak < thr {
        "有信号但没过阈值。提高扬声器音量，或把耳机贴紧麦克风。".into()
    } else {
        "电平够但没触发，可能被设备的硬件降噪/回声消除吃掉了。建议改用回环线。".into()
    };

    if let (Some(m), true) = (state.metrics(), detected > 0) {
        let stats = LatencyStats::from_micros(samples.clone());
        m.measured_rt_us.store(
            (stats.median_ms * 1000.0) as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    Ok(LatencyResult {
        stats: LatencyStats::from_micros(samples),
        detected,
        requested: rounds,
        peak_seen: peak,
        noise_floor: floor,
        threshold: thr,
        diagnosis,
    })
}
