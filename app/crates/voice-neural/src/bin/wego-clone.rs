//! `wego-clone.exe` —— 声线转换的伴生程序。
//!
//! # 为什么推理要单开一个进程
//!
//! 主程序是 6.5 MB 的免安装单文件，这是产品的卖点。把推理引擎链进去
//! 会让**所有人**都为一个大多数人不用的功能付体积。
//!
//! 单开一个进程还顺带解决两件事：
//!
//! - **架构红线 3 升级成进程级隔离。** "推理绝不与实时音频线程抢 CPU"
//!   以前靠代码里一道守卫，现在推理连碰那个线程的机会都没有，
//!   而且父进程可以整体压低它的优先级。
//! - **它崩了主程序不跟着崩。** 模型是用户下载来的几百 MB 文件，
//!   坏掉的可能性不低。
//!
//! # 和父进程的约定
//!
//! stdout **逐行**输出，父进程按前缀解析。设计成人能读的，
//! 因为出问题时第一件事就是手工跑一遍看它说什么：
//!
//! ```text
//! STAGE 提取音高轨
//! PROGRESS 0.32
//! OK D:\Music\wego-voice\wego-123-cloned.wav
//! ```
//!
//! 失败一律 `ERR <人话>` 然后退出码 1。**不把错误写进 stderr** ——
//! 那会和推理库自己的日志混在一起，父进程分不清哪句是给用户看的。
//!
//! 取消不需要协议：父进程直接杀掉本进程。所以**产物必须最后一步才落盘**，
//! 中途被杀不会留下半个文件。

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};


fn main() {
    if let Err(e) = run() {
        // 错误也走 stdout，和进度同一条流 —— 父进程只读一处
        println!("ERR {e:#}");
        let _ = std::io::stdout().flush();
        std::process::exit(1);
    }
}

fn say(line: &str) {
    println!("{line}");
    // 必须立刻冲刷：管道是块缓冲的，不冲的话进度会攒到进程结束才一起出来，
    // 而那时候进度条已经没有意义了。
    let _ = std::io::stdout().flush();
}

fn stage(s: &str) {
    say(&format!("STAGE {s}"));
}

fn progress(f: f32) {
    say(&format!("PROGRESS {:.4}", f.clamp(0.0, 1.0)));
}

struct Args {
    models: PathBuf,
    input: PathBuf,
    output: PathBuf,
    speaker: usize,
}

fn parse() -> Result<Args> {
    let a: Vec<String> = std::env::args().collect();
    let get = |k: &str| -> Option<String> {
        a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).cloned()
    };
    if a.iter().any(|x| x == "--selftest") {
        selftest()?;
        std::process::exit(0);
    }
    Ok(Args {
        models: get("--models").context("缺少 --models <模型目录>")?.into(),
        input: get("--input").context("缺少 --input <干声.wav>")?.into(),
        output: get("--output").context("缺少 --output <输出.wav>")?.into(),
        speaker: get("--speaker").and_then(|v| v.parse().ok()).unwrap_or(1),
    })
}

/// 不碰模型的自检 —— 父进程用它确认这个 exe 能跑（架构、依赖齐全）。
///
/// 下载完伴生程序之后先跑这一下，比等到用户点了转换才发现
/// "这个 exe 在这台机器上根本起不来"要好。
fn selftest() -> Result<()> {
    voice_neural::convert::selftest()?;
    say("OK selftest");
    Ok(())
}

fn run() -> Result<()> {
    let args = parse()?;
    voice_neural::session::init_pure_rust();
    let out = voice_neural::convert::run(
        &voice_neural::convert::Job {
            models: args.models,
            input: args.input,
            output: args.output,
            speaker: args.speaker,
        },
        |r| match r {
            voice_neural::convert::Report::Stage(s) => stage(s),
            voice_neural::convert::Report::Progress(p) => progress(p),
        },
    )?;
    say(&format!("OK {}", out.display()));
    Ok(())
}

/// 产物路径：`<原名>-cloned.wav`，与原录音放在一起。
///
/// 与离线校准的 `-corrected.wav` 并列，用户一眼能分清这是哪一步的产物。
pub fn output_path(input: &Path) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    input.with_file_name(format!("{stem}-cloned.wav"))
}
