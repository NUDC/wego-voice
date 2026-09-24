//! 声线转换任务：**跑一个子进程**，而不是在本进程里推理。
//!
//! # 为什么是子进程
//!
//! 推理引擎和模型是按需下载的，主程序里一行推理代码都没有 ——
//! 6.5 MB 免安装单文件这件事不能为一个大多数人不用的功能让步。
//!
//! 顺带解决两件事：
//!
//! - **架构红线 3 从"一道守卫"升级成"进程级隔离"。** 以前是代码里
//!   拦一下不让它和音频线程抢 CPU；现在推理连碰那个线程的机会都没有，
//!   而且我们可以整体压低子进程的优先级。
//! - **它崩了主程序不跟着崩。** 模型是用户下载来的几百 MB 文件，
//!   坏掉的概率不低，而用户正在录的东西不该被它带走。
//!
//! # 和子进程的约定
//!
//! 子进程 stdout 逐行输出，本模块按前缀解析：
//!
//! ```text
//! STAGE 提取音高轨
//! PROGRESS 0.32
//! OK <产物路径>          或者   ERR <人话>
//! ```
//!
//! 取消就是**直接杀掉它** —— 所以子进程被要求"最后一步才落盘"，
//! 中途被杀不会留下半个文件。一个"存在但内容是残的"产物，
//! 比没有产物糟得多。

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

const REL: Ordering = Ordering::Relaxed;

/// 任务的共享状态。控制线程读，工作线程写。
#[derive(Debug, Default)]
pub struct CloneState {
    running: AtomicBool,
    cancel: AtomicBool,
    /// 0~1，f32 位模式。
    progress: AtomicU32,
    /// 当前阶段的中文名。
    stage: Mutex<String>,
    output: Mutex<Option<PathBuf>>,
    error: Mutex<Option<String>>,
    /// 正在跑的子进程，取消时要杀它。
    child: Mutex<Option<Child>>,
}

impl CloneState {
    pub fn is_running(&self) -> bool {
        self.running.load(REL)
    }
    pub fn progress(&self) -> f32 {
        f32::from_bits(self.progress.load(REL))
    }
    pub fn stage(&self) -> String {
        self.stage.lock().map(|g| g.clone()).unwrap_or_default()
    }
    pub fn output(&self) -> Option<PathBuf> {
        self.output.lock().ok().and_then(|g| g.clone())
    }
    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|g| g.clone())
    }

    /// 请求取消：**直接杀掉子进程**。
    ///
    /// 不走协商（发个信号等它自己收摊）：推理是一段长时间的纯计算，
    /// 中间没有自然的检查点，等它"自己停"可能要等十几秒。
    /// 而它被要求最后一步才落盘，所以杀掉是安全的。
    pub fn cancel(&self) {
        self.cancel.store(true, REL);
        if let Ok(mut g) = self.child.lock() {
            if let Some(c) = g.as_mut() {
                let _ = c.kill();
            }
        }
    }

    fn set_stage(&self, s: &str) {
        if let Ok(mut g) = self.stage.lock() {
            *g = s.to_string();
        }
    }
    fn set_progress(&self, p: f32) {
        self.progress.store(p.clamp(0.0, 1.0).to_bits(), REL);
    }
}

/// 产物路径：`<原名>-cloned.wav`，与原录音放在一起。
///
/// 与离线校准的 `-corrected.wav` 并列 —— 用户一眼能分清是哪一步的产物。
pub fn output_path(input: &Path) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    input.with_file_name(format!("{stem}-cloned.wav"))
}

/// 组装子进程命令。
///
/// 抽出来单独一个函数是为了**能测**：参数拼错（少一个 `--models`、
/// 路径没引号）在 Windows 上的表现是子进程一启动就报参数错误，
/// 而那条错误会被当成"模型坏了"。
pub fn build_command(exe: &Path, models: &Path, input: &Path, output: &Path, speaker: usize) -> Command {
    let mut c = Command::new(exe);
    c.arg("--models")
        .arg(models)
        .arg("--input")
        .arg(input)
        .arg("--output")
        .arg(output)
        .arg("--speaker")
        .arg(speaker.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// 不要弹控制台窗口。
        ///
        /// 漏掉这一条的表现是：用户点「换声」，屏幕上闪过一个黑框 ——
        /// 看着像中了什么东西。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        /// 压低优先级。
        ///
        /// 红线 3 的进程级落实：即便用户绕过界面守卫同时开了引擎，
        /// 调度器也会先喂音频线程。
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        c.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
    }
    c
}

/// 解析子进程的一行输出。返回 `None` 表示这行不认识（忽略）。
#[derive(Debug, PartialEq)]
pub enum Line {
    Stage(String),
    Progress(f32),
    Done(PathBuf),
    Failed(String),
}

pub fn parse_line(s: &str) -> Option<Line> {
    let s = s.trim();
    if let Some(v) = s.strip_prefix("STAGE ") {
        return Some(Line::Stage(v.to_string()));
    }
    if let Some(v) = s.strip_prefix("PROGRESS ") {
        return v.trim().parse::<f32>().ok().map(Line::Progress);
    }
    if let Some(v) = s.strip_prefix("OK ") {
        return Some(Line::Done(PathBuf::from(v.trim())));
    }
    if let Some(v) = s.strip_prefix("ERR ") {
        return Some(Line::Failed(v.trim().to_string()));
    }
    None
}

/// 启动一次声线转换。立刻返回；进度与结果通过 `state` 读取。
pub fn start(
    state: Arc<CloneState>,
    exe: PathBuf,
    models: PathBuf,
    input: PathBuf,
    speaker: usize,
) -> Result<(), String> {
    if !exe.is_file() {
        return Err(format!("找不到推理程序：{}", exe.display()));
    }
    if state.running.swap(true, REL) {
        return Err("已有一个转换任务在跑".into());
    }

    state.cancel.store(false, REL);
    *state.output.lock().map_err(|_| "锁已中毒")? = None;
    *state.error.lock().map_err(|_| "锁已中毒")? = None;
    state.set_progress(0.0);
    state.set_stage("准备中");

    let output = output_path(&input);
    std::thread::spawn(move || {
        let r = run(&state, &exe, &models, &input, &output, speaker);
        if let Err(e) = r {
            // 被取消时不报错 —— 那是用户自己按的
            if !state.cancel.load(REL) {
                if let Ok(mut g) = state.error.lock() {
                    *g = Some(e);
                }
            }
            state.set_stage("失败");
        }
        if let Ok(mut g) = state.child.lock() {
            *g = None;
        }
        state.running.store(false, REL);
    });
    Ok(())
}

fn run(
    state: &CloneState,
    exe: &Path,
    models: &Path,
    input: &Path,
    output: &Path,
    speaker: usize,
) -> Result<(), String> {
    let mut child = build_command(exe, models, input, output, speaker)
        .spawn()
        .map_err(|e| format!("启动推理程序失败：{e}"))?;

    let stdout = child.stdout.take().ok_or("拿不到子进程输出")?;
    if let Ok(mut g) = state.child.lock() {
        *g = Some(child);
    }

    let mut failure: Option<String> = None;
    let mut done: Option<PathBuf> = None;
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        match parse_line(&line) {
            Some(Line::Stage(s)) => state.set_stage(&s),
            Some(Line::Progress(p)) => state.set_progress(p),
            Some(Line::Done(p)) => done = Some(p),
            Some(Line::Failed(e)) => failure = Some(e),
            None => {}
        }
    }

    // 等它真的退出，顺手收掉僵尸进程
    let status = match state.child.lock() {
        Ok(mut g) => match g.as_mut() {
            Some(c) => c.wait().map_err(|e| format!("等待推理程序失败：{e}"))?.code(),
            None => None,
        },
        Err(_) => None,
    };

    if state.cancel.load(REL) {
        state.set_stage("已取消");
        return Ok(());
    }
    if let Some(e) = failure {
        return Err(e);
    }
    match done {
        Some(p) => {
            if let Ok(mut g) = state.output.lock() {
                *g = Some(p);
            }
            state.set_stage("完成");
            state.set_progress(1.0);
            Ok(())
        }
        None => Err(format!(
            "推理程序没有给出结果就退出了（{}）——\
             多半是模型文件损坏，或者这台机器缺少运行库",
            status
                .map(|c| format!("退出码 {c}"))
                .unwrap_or_else(|| "退出码未知".into())
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_protocol() {
        assert_eq!(parse_line("STAGE 提取音高轨"), Some(Line::Stage("提取音高轨".into())));
        assert_eq!(parse_line("PROGRESS 0.3200"), Some(Line::Progress(0.32)));
        assert_eq!(
            parse_line(r"OK D:\Music\a-cloned.wav"),
            Some(Line::Done(PathBuf::from(r"D:\Music\a-cloned.wav")))
        );
        assert_eq!(
            parse_line("ERR 模型文件不存在"),
            Some(Line::Failed("模型文件不存在".into()))
        );
    }

    /// 认不出来的行必须**忽略**，不能当成错误。
    ///
    /// 推理库自己可能往 stdout 打日志（tract、ONNX 都干过这事）。
    /// 把那些当错误的话，任务会在一切正常的情况下"失败"。
    #[test]
    fn unknown_lines_are_ignored_not_errors() {
        for s in [
            "",
            "   ",
            "[tract] loading graph...",
            "warning: something",
            "PROGRESSbutnotreally",
            "OKAY",
        ] {
            assert_eq!(parse_line(s), None, "{s:?} 不该被解析成消息");
        }
    }

    /// 进度里的脏数据不能让状态跳掉。
    #[test]
    fn malformed_progress_is_dropped() {
        assert_eq!(parse_line("PROGRESS abc"), None);
        assert_eq!(parse_line("PROGRESS "), None);
    }

    #[test]
    fn output_sits_next_to_the_input_with_its_own_suffix() {
        assert_eq!(
            output_path(Path::new(r"C:\Music\wego-voice\wego-1.wav")),
            PathBuf::from(r"C:\Music\wego-voice\wego-1-cloned.wav")
        );
        // 与离线校准的产物并列，不互相覆盖
        assert_ne!(
            output_path(Path::new(r"C:\a\x.wav")),
            PathBuf::from(r"C:\a\x-corrected.wav")
        );
    }

    /// 缺推理程序要**当场**报错，而不是起一个进程再失败。
    #[test]
    fn a_missing_companion_fails_immediately() {
        let st = Arc::new(CloneState::default());
        let e = start(
            st.clone(),
            PathBuf::from("definitely-not-here.exe"),
            PathBuf::from("."),
            PathBuf::from("x.wav"),
            1,
        )
        .unwrap_err();
        assert!(e.contains("找不到推理程序"), "{e}");
        assert!(!st.is_running(), "失败了却把状态置成了运行中");
    }

    /// 命令行参数要拼全 —— 少一个 `--models` 的表现是子进程一启动
    /// 就报参数错误，而那条错误会被当成"模型坏了"。
    #[test]
    fn the_command_carries_every_argument() {
        let c = build_command(
            Path::new("wego-clone.exe"),
            Path::new(r"C:\models"),
            Path::new(r"C:\a.wav"),
            Path::new(r"C:\b.wav"),
            3,
        );
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        for want in ["--models", "--input", "--output", "--speaker"] {
            assert!(args.iter().any(|a| a == want), "少了 {want}：{args:?}");
        }
        assert!(args.iter().any(|a| a == "3"), "说话人编号没传：{args:?}");
        assert!(args.iter().any(|a| a.contains("models")), "{args:?}");
    }
}
