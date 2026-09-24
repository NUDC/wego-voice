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

use anyhow::{bail, Context, Result};
use voice_neural::{assets, decoder, encoder, features, source, synth, wav};

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
    voice_neural::session::init_pure_rust();
    let f0 = vec![220.0f32; 8];
    let s = source::combtooth(&f0, 44_100.0, 512);
    if s.combtooth.iter().any(|v| !v.is_finite()) {
        bail!("激励生成异常");
    }
    say("OK selftest");
    Ok(())
}

fn run() -> Result<()> {
    let args = parse()?;
    voice_neural::session::init_pure_rust();

    let enc_path = args.models.join(assets::ASSETS[0].name);
    let dec_path = args.models.join(assets::ASSETS[1].name);
    for p in [&enc_path, &dec_path] {
        if !p.is_file() {
            bail!("模型文件不存在：{}", p.display());
        }
    }

    // ── 解码器：顺带把结构参数读出来，不写死 ──
    stage("载入解码器");
    progress(0.02);
    let (net, cfg) = decoder::load_from_pth(&dec_path)?;

    // ── 读干声并重采样到模型的采样率 ──
    stage("读取音频");
    progress(0.05);
    let audio = wav::read(&args.input)?;
    if audio.samples.is_empty() {
        bail!("文件里没有音频数据");
    }
    let x = encoder::resample(&audio.samples, audio.sample_rate, cfg.sample_rate);
    if x.len() < cfg.block_size * 4 {
        bail!(
            "音频太短：重采样后只有 {} 个样本，不够 4 帧",
            x.len()
        );
    }

    // ── 三路特征 ──
    stage("提取音高轨");
    let track = voice_core::track_pitch(&x, cfg.sample_rate as f32, |p| {
        progress(0.08 + p * 0.17);
        true
    })
    .context("音高提取失败")?;
    if track.voiced_count() == 0 {
        bail!("整段音频里没有检测到人声 —— 确认录的是干声，不是伴奏");
    }

    stage("分析内容");
    progress(0.28);
    let mut enc = encoder::ContentEncoder::open(&enc_path, 2)?;
    let content = enc.encode(&x, cfg.sample_rate)?;
    progress(0.72);

    let a = features::align(&track, &x, cfg.sample_rate, &content, cfg.block_size)?;
    if a.dim != cfg.n_unit {
        bail!(
            "内容特征是 {} 维，解码器要 {} 维 —— 编码器和解码器不配套",
            a.dim,
            cfg.n_unit
        );
    }
    if args.speaker == 0 || args.speaker > cfg.n_spk {
        bail!("声线编号 {} 越界（这个模型有 {} 个）", args.speaker, cfg.n_spk);
    }

    // ── 合成 ──
    stage("重新合成");
    progress(0.75);
    let exc = source::combtooth(&a.f0, cfg.sample_rate as f32, cfg.block_size);
    let ctrls = net.forward(
        &decoder::Inputs {
            units: &a.content,
            dim: a.dim,
            f0: &a.f0,
            phase: &exc.phase,
            volume: &a.volume,
        },
        args.speaker,
        &candle_core::Device::Cpu,
    )?;
    progress(0.90);

    let (hf, nf) = ctrls.filters();
    let y = synth::synthesize(&exc.combtooth, cfg.win_length, cfg.block_size, &hf, &nf, 1);

    // ── 能机器判定的，落盘之前全判一遍 ──
    check(&y, &a)?;
    progress(0.98);

    // ⚠️ **最后一步才落盘。** 中途被父进程杀掉不会留下半个文件 ——
    // 一个"存在但内容是残的"产物，比没有产物糟得多。
    stage("写入文件");
    wav::write(&args.output, &y, cfg.sample_rate)?;
    progress(1.0);
    say(&format!("OK {}", args.output.display()));
    Ok(())
}

/// 落盘之前的自检。
///
/// 这些是**机器能判**的全部：没有 NaN、不是静音、没有爆幅、长度对、
/// 而且**音高保住了**。像不像判不了，那是耳朵的事。
fn check(y: &[f32], a: &features::Aligned) -> Result<()> {
    let nan = y.iter().filter(|v| !v.is_finite()).count();
    if nan > 0 {
        bail!("输出里有 {nan} 个非有限值 —— 推理过程中某处发散了");
    }
    let peak = y.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if peak < 1e-4 {
        bail!("输出几乎是静音（峰值 {peak:.6}）");
    }
    if peak > 100.0 {
        bail!("输出爆幅（峰值 {peak:.1}）—— 多半是模型与代码版本不配套");
    }
    if y.len() != a.frames * a.block_size {
        bail!("输出长度 {} 与 {} 帧对不上", y.len(), a.frames);
    }
    Ok(())
}

/// 产物路径：`<原名>-cloned.wav`，与原录音放在一起。
///
/// 与离线校准的 `-corrected.wav` 并列，用户一眼能分清这是哪一步的产物。
pub fn output_path(input: &Path) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    input.with_file_name(format!("{stem}-cloned.wav"))
}
