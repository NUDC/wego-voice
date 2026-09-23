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
/// （WASAPI 独占、水位 2.5、f0_floor 130）。
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
    /// 角色：频谱倾斜（dB/八度）。
    pub tilt_db_per_oct: Option<f32>,
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
    if let Some(v) = upd.tilt_db_per_oct {
        p.set_tilt_db_per_oct(v);
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

// ─────────────────────── 离线处理 ───────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfflineStatus {
    pub running: bool,
    /// 0~1。
    pub progress: f32,
    /// 阶段中文名，直接显示。
    pub stage: String,
    pub output: Option<String>,
    pub error: Option<String>,
    /// 音高轨后处理修了多少 —— 让"离线到底多做了什么"看得见。
    pub octave_fixes: u32,
    pub gap_fills: u32,
}

/// 启动离线重新校准。守卫在 `AppState::start_offline`（见那里的说明）。
#[tauri::command]
pub fn offline_start(
    state: State<AppState>,
    input: String,
    character: Character,
) -> Result<(), String> {
    let key = parse_key(&character.key)
        .ok_or_else(|| format!("无法解析调名：{}", character.key))?;
    let cfg = voice_core::RecorrectConfig {
        key,
        retune_ms: character.retune_ms.clamp(0.0, 500.0),
        intent_ms: 150.0,
        pitch_shift: character.pitch_shift,
        formant_shift: character.formant_shift,
    };

    state.start_offline(std::path::PathBuf::from(input), cfg)
}

#[tauri::command]
pub fn offline_status(state: State<AppState>) -> OfflineStatus {
    let j = state.job();
    OfflineStatus {
        running: j.is_running(),
        progress: j.progress(),
        stage: j.stage().label().to_string(),
        output: j.output().map(|p| p.to_string_lossy().into_owned()),
        error: j.error(),
        octave_fixes: j.octave_fixes(),
        gap_fills: j.gap_fills(),
    }
}

#[tauri::command]
pub fn offline_cancel(state: State<AppState>) {
    state.job().cancel();
}

// ────────────────────── 参考音频 → 角色 ──────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TakeInfo {
    pub name: String,
    pub path: String,
    pub seconds: f32,
}

/// 列出录音目录里的所有 take，新的在前。
#[tauri::command]
pub fn list_recordings(app: tauri::AppHandle) -> Vec<TakeInfo> {
    let Ok(dir) = recordings_dir(&app) else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<(std::time::SystemTime, TakeInfo)> = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("wav") {
            continue;
        }
        // 只读文件头拿时长 —— 列目录不该把每个文件都读进内存
        let secs = voice_audio::wav::probe(&p).map(|(_, s)| s).unwrap_or(0.0);
        let modified = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
        out.push((
            modified,
            TakeInfo {
                name: p.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                path: p.to_string_lossy().into_owned(),
                seconds: secs,
            },
        ));
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|(_, t)| t).collect()
}

/// 参考音频分析结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimbreSuggestion {
    /// 建议的共振峰平移（半音）。**这是声线的主维度。**
    pub formant_shift: f32,
    /// 建议的整体移调（半音）。恒为 0 —— 改了就不是这首歌了。
    pub pitch_shift: f32,
    /// 实测音高差（半音），仅供参考。正 = 参考音源更高。
    pub pitch_delta: f32,
    /// 0~1。低于 0.4 时 UI 必须明说"没把握"。
    pub confidence: f32,
    /// 频谱倾斜差（dB/八度）。⚠️ **当前引擎补不了这个差异**。
    pub tilt_delta: f32,

    pub source_f0: f32,
    pub reference_f0: f32,
    pub source_voiced_secs: f32,
    pub reference_voiced_secs: f32,
    /// 素材不合格时的人话说明；合格时为空串。
    pub warning: String,
}

/// 比较「你的干声」与「参考音频」，给出角色参数建议。
///
/// # 为什么需要两份素材
///
/// 共振峰平移是个**相对量** —— "把你的声道缩放到它那么长"。
/// 只给参考音频是算不出来的，必须知道你自己的起点在哪。
///
/// 所以源素材用你自己的录音（干声）。这也是录制功能存在的另一个理由。
#[tauri::command]
pub fn suggest_character(
    reference_path: String,
    source_path: String,
) -> Result<TimbreSuggestion, String> {
    let refr = voice_audio::wav::read(&reference_path).map_err(|e| e.to_string())?;
    let src = voice_audio::wav::read(&source_path).map_err(|e| e.to_string())?;

    let a = voice_core::analyze_timbre(&src.samples, src.sample_rate as f32);
    let b = voice_core::analyze_timbre(&refr.samples, refr.sample_rate as f32);
    let m = voice_core::match_to(&a, &b);

    // 素材不合格时**明确说出来**，而不是给一个悄悄不准的数字 ——
    // 用户拿到错的结果只会怪工具。
    let min = voice_core::timbre::MIN_VOICED_SECS;
    let warning = if !a.is_usable() {
        format!(
            "你的录音里只有 {:.1} 秒浊音（至少要 {min:.1} 秒）—— 多唱几句再试",
            a.voiced_secs
        )
    } else if !b.is_usable() {
        format!(
            "参考音频里只有 {:.1} 秒浊音（至少要 {min:.1} 秒）—— 换一段有人声的素材",
            b.voiced_secs
        )
    } else if m.confidence < 0.4 {
        "两段素材的谱包络差异不明显，结果把握不大；建议两边都用更长、更干净的素材".into()
    } else {
        String::new()
    };

    Ok(TimbreSuggestion {
        formant_shift: m.formant_shift,
        pitch_shift: m.pitch_shift,
        pitch_delta: m.pitch_delta,
        confidence: m.confidence,
        tilt_delta: m.tilt_delta,
        source_f0: a.median_f0,
        reference_f0: b.median_f0,
        source_voiced_secs: a.voiced_secs,
        reference_voiced_secs: b.voiced_secs,
        warning,
    })
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
    p.set_tilt_db_per_oct(character.tilt_db_per_oct);
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
