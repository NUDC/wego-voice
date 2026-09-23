//! 离线处理任务。
//!
//! # 为什么要一个任务运行器，而不是直接在命令里跑
//!
//! 一首五分钟的歌要跑十几秒。在 Tauri command 里同步跑会把整个 IPC 卡住 ——
//! 界面假死，用户连取消都点不了。所以：独立线程 + 共享进度 + 取消标志。
//!
//! # 架构红线 3 在这里的具体含义
//!
//! 「推理线程绝不与实时音频线程抢 CPU」。离线处理是满载单核跑的，
//! 而实时链路每 3 ms 就要交一次货 —— 两者同时跑，xrun 是必然的。
//!
//! 所以**引擎在跑的时候直接拒绝启动离线任务**，并说清为什么。
//! 不是降低线程优先级了事：优先级只降低概率，不消除冲突，
//! 而这里的失败形式是用户耳朵里的爆音。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

const REL: Ordering = Ordering::Relaxed;

/// 任务阶段。进度条只有一条，但用户需要知道现在在干什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Idle,
    Reading,
    Tracking,
    Correcting,
    Writing,
    Done,
    Failed,
    Cancelled,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Idle => "空闲",
            Stage::Reading => "读取音频",
            Stage::Tracking => "提取音高轨",
            Stage::Correcting => "重新校准",
            Stage::Writing => "写入文件",
            Stage::Done => "完成",
            Stage::Failed => "失败",
            Stage::Cancelled => "已取消",
        }
    }
}

/// 任务的共享状态。控制线程读，工作线程写。
#[derive(Debug)]
pub struct JobState {
    running: AtomicBool,
    cancel: AtomicBool,
    /// 0~1，f32 位模式。
    progress: AtomicU32,
    stage: AtomicU32,
    /// 完成后的产物路径。
    output: Mutex<Option<PathBuf>>,
    /// 失败原因。人话，直接显示给用户。
    error: Mutex<Option<String>>,
    /// 音高轨的统计，跑完之后给用户看"离线到底多做了什么"。
    octave_fixes: AtomicU32,
    gap_fills: AtomicU32,
}

impl Default for JobState {
    fn default() -> Self {
        Self {
            running: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            progress: AtomicU32::new(0),
            stage: AtomicU32::new(Stage::Idle as u32),
            output: Mutex::new(None),
            error: Mutex::new(None),
            octave_fixes: AtomicU32::new(0),
            gap_fills: AtomicU32::new(0),
        }
    }
}

impl JobState {
    pub fn is_running(&self) -> bool {
        self.running.load(REL)
    }

    pub fn progress(&self) -> f32 {
        f32::from_bits(self.progress.load(REL))
    }

    pub fn stage(&self) -> Stage {
        match self.stage.load(REL) {
            1 => Stage::Reading,
            2 => Stage::Tracking,
            3 => Stage::Correcting,
            4 => Stage::Writing,
            5 => Stage::Done,
            6 => Stage::Failed,
            7 => Stage::Cancelled,
            _ => Stage::Idle,
        }
    }

    pub fn output(&self) -> Option<PathBuf> {
        self.output.lock().ok().and_then(|g| g.clone())
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|g| g.clone())
    }

    pub fn octave_fixes(&self) -> u32 {
        self.octave_fixes.load(REL)
    }

    pub fn gap_fills(&self) -> u32 {
        self.gap_fills.load(REL)
    }

    /// 请求取消。工作线程在下一个汇报点看到就停。
    pub fn cancel(&self) {
        self.cancel.store(true, REL);
    }

    fn set(&self, stage: Stage, p: f32) {
        self.stage.store(stage as u32, REL);
        self.progress.store(p.clamp(0.0, 1.0).to_bits(), REL);
    }
}

/// 产物文件名：`<原名>-corrected.wav`，与原录音放在同一目录。
///
/// 放一起而不是另开目录：用户刚录完就在那儿找，隔一层就得多点两下。
pub fn output_path(input: &Path) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    input.with_file_name(format!("{stem}-corrected.wav"))
}

/// 启动一次离线重新校准。
///
/// 立刻返回；进度与结果通过 `state` 读取。
/// 已有任务在跑时返回 `Err`，不排队 —— 排队意味着要管队列，
/// 而这个场景下用户只会一次处理一条。
pub fn start_recorrect(
    state: Arc<JobState>,
    input: PathBuf,
    cfg: voice_core::RecorrectConfig,
) -> Result<(), String> {
    if state.running.swap(true, REL) {
        return Err("已有离线任务在跑".into());
    }

    state.cancel.store(false, REL);
    *state.output.lock().map_err(|_| "锁已中毒")? = None;
    *state.error.lock().map_err(|_| "锁已中毒")? = None;
    state.octave_fixes.store(0, REL);
    state.gap_fills.store(0, REL);
    state.set(Stage::Reading, 0.0);

    std::thread::spawn(move || {
        let outcome = run(&state, &input, cfg);
        match outcome {
            Ok(Some(path)) => {
                if let Ok(mut g) = state.output.lock() {
                    *g = Some(path);
                }
                state.set(Stage::Done, 1.0);
            }
            // None = 用户取消
            Ok(None) => state.set(Stage::Cancelled, 0.0),
            Err(e) => {
                if let Ok(mut g) = state.error.lock() {
                    *g = Some(e);
                }
                state.set(Stage::Failed, 0.0);
            }
        }
        state.running.store(false, REL);
    });

    Ok(())
}

/// 真正干活的部分。返回 `Ok(None)` 表示被取消。
fn run(
    state: &JobState,
    input: &Path,
    cfg: voice_core::RecorrectConfig,
) -> Result<Option<PathBuf>, String> {
    let src = crate::wav::read(input).map_err(|e| e.to_string())?;
    if src.samples.is_empty() {
        return Err("文件里没有音频数据".into());
    }
    let sr = src.sample_rate as f32;

    // 两个阶段各占进度条的一半。音高提取通常比校准慢一点，
    // 但差得不多，五五开比按实测比例分更好预测。
    let track = {
        let s = state;
        voice_core::track_pitch(&src.samples, sr, |p| {
            s.set(Stage::Tracking, p * 0.5);
            !s.cancel.load(REL)
        })
    };
    let Some(track) = track else {
        return Ok(None);
    };
    if track.voiced_count() == 0 {
        return Err("整段音频里没有检测到人声 —— 确认录的是干声，不是伴奏".into());
    }
    state.octave_fixes.store(track.octave_fixes as u32, REL);
    state.gap_fills.store(track.gap_fills as u32, REL);

    let out = {
        let s = state;
        voice_core::recorrect(&src.samples, sr, &track, cfg, |p| {
            s.set(Stage::Correcting, 0.5 + p * 0.5);
            !s.cancel.load(REL)
        })
    };
    let Some(out) = out else {
        return Ok(None);
    };

    state.set(Stage::Writing, 1.0);
    let path = output_path(input);
    crate::wav::write(&path, &out, src.sample_rate).map_err(|e| e.to_string())?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const SR: u32 = 48_000;

    fn write_tone(name: &str, hz: f32, secs: f32) -> PathBuf {
        let n = (secs * SR as f32) as usize;
        let x: Vec<f32> = (0..n)
            .map(|i| (TAU * hz * i as f32 / SR as f32).sin() * 0.4)
            .collect();
        let p = std::env::temp_dir().join(name);
        crate::wav::write(&p, &x, SR).unwrap();
        p
    }

    /// 等任务结束，最多 `secs` 秒。返回是否等到。
    fn wait(state: &JobState, secs: f32) -> bool {
        let t0 = std::time::Instant::now();
        while state.is_running() {
            if t0.elapsed().as_secs_f32() > secs {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        true
    }

    #[test]
    fn runs_to_completion_and_writes_output() {
        let input = write_tone("wego-job-ok.wav", 220.0, 1.0);
        let out = output_path(&input);
        let _ = std::fs::remove_file(&out);

        let state = Arc::new(JobState::default());
        start_recorrect(state.clone(), input.clone(), Default::default()).unwrap();
        assert!(wait(&state, 30.0), "任务超时");

        assert_eq!(state.stage(), Stage::Done, "错误：{:?}", state.error());
        assert_eq!(state.output().as_deref(), Some(out.as_path()));
        assert!(out.exists(), "产物文件不存在");

        // 产物长度应与输入一致 —— 否则没法和干声叠在一起听
        let a = crate::wav::read(&input).unwrap();
        let b = crate::wav::read(&out).unwrap();
        assert_eq!(a.samples.len(), b.samples.len());

        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(&out);
    }

    /// 同一时刻只允许一个任务。第二个必须被拒，而不是悄悄排队 ——
    /// 排队会让用户以为点了没反应。
    #[test]
    fn refuses_a_second_job() {
        let input = write_tone("wego-job-busy.wav", 200.0, 3.0);
        let state = Arc::new(JobState::default());
        start_recorrect(state.clone(), input.clone(), Default::default()).unwrap();

        let second = start_recorrect(state.clone(), input.clone(), Default::default());
        assert!(second.is_err(), "第二个任务没被拒绝");

        state.cancel();
        wait(&state, 30.0);
        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(output_path(&input));
    }

    /// 取消要真的停下来，而且状态要如实反映。
    #[test]
    fn cancellation_stops_the_job() {
        let input = write_tone("wego-job-cancel.wav", 180.0, 8.0);
        let out = output_path(&input);
        let _ = std::fs::remove_file(&out);

        let state = Arc::new(JobState::default());
        start_recorrect(state.clone(), input.clone(), Default::default()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(60));
        state.cancel();

        assert!(wait(&state, 30.0), "取消之后没停下来");
        assert_eq!(state.stage(), Stage::Cancelled);
        assert!(state.output().is_none(), "取消了却还产出了文件路径");
        assert!(!out.exists(), "取消了却写出了文件");

        let _ = std::fs::remove_file(&input);
    }

    /// 全是静音时给人话，而不是产出一个空文件让用户自己纳闷。
    #[test]
    fn reports_a_useful_error_for_silence() {
        let input = write_tone("wego-job-silent.wav", 0.0, 1.0);
        let state = Arc::new(JobState::default());
        start_recorrect(state.clone(), input.clone(), Default::default()).unwrap();
        assert!(wait(&state, 30.0));

        assert_eq!(state.stage(), Stage::Failed);
        let e = state.error().unwrap_or_default();
        assert!(e.contains("人声"), "错误信息没说到点子上：{e}");

        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn missing_file_fails_cleanly() {
        let state = Arc::new(JobState::default());
        start_recorrect(
            state.clone(),
            std::env::temp_dir().join("wego-does-not-exist.wav"),
            Default::default(),
        )
        .unwrap();
        assert!(wait(&state, 10.0));
        assert_eq!(state.stage(), Stage::Failed);
        assert!(state.error().is_some());
    }

    #[test]
    fn output_path_sits_next_to_the_input() {
        let p = Path::new(r"C:\Music\wego-voice\wego-123.wav");
        assert_eq!(
            output_path(p),
            Path::new(r"C:\Music\wego-voice\wego-123-corrected.wav")
        );
    }
}
