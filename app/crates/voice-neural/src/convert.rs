//! 声线转换的完整流程 —— **库函数，不是可执行文件**。
//!
//! # 为什么抽成库
//!
//! 同一段逻辑有两个入口：
//!
//! - 主程序的 `--clone` 模式（产品用的那条）
//! - `wego-clone.exe`（开发排查用）
//!
//! 写两遍的话，改一处忘一处的表现是"命令行能跑，界面里不对"，
//! 而那种差异极难定位。所以逻辑只有这一份，入口只是壳。
//!
//! # 它仍然跑在单独的进程里
//!
//! 主程序把**自己**用 `--clone` 再拉起一个进程来干这件事。
//! 同一个二进制、两个进程，保住了两件要紧的事：
//!
//! - **推理没机会饿死音频线程。** 耳返每 3 ms 必须交货，余量 0.08 ms。
//!   子进程以 `BELOW_NORMAL_PRIORITY_CLASS` 起，调度器在系统层面
//!   站在音频那边 —— 这比代码里一道 if 硬。
//! - **模型坏了不会带走用户正在录的东西。** 360 MB 的文件是用户下载来的，
//!   损坏概率不低；解析坏模型可能直接 abort，而干声是这个产品里
//!   唯一不可再生的东西。
//!
//! 同一个二进制还消掉了**版本错配**：伴生程序单独分发的话，用户可能
//! 拿着旧的伴生程序配新的主程序，而那种 bug 只表现为"声音不对"。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::{assets, decoder, encoder, features, source, synth, wav};

/// 一次转换的输入。
pub struct Job {
    /// 模型目录。
    pub models: PathBuf,
    /// 干声。
    pub input: PathBuf,
    /// 产物。
    pub output: PathBuf,
    /// 声线编号，从 1 数。
    pub speaker: usize,
}

/// 过程汇报。调用方决定怎么呈现（打印协议行、更新进度条…）。
pub enum Report<'a> {
    Stage(&'a str),
    Progress(f32),
}

/// 跑一遍。
pub fn run(job: &Job, mut report: impl FnMut(Report)) -> Result<PathBuf> {
    run_inner(job, &mut report)
}

fn run_inner(job: &Job, report: &mut impl FnMut(Report)) -> Result<PathBuf> {
    let enc_path = job.models.join(assets::ASSETS[0].name);
    let dec_path = job.models.join(assets::ASSETS[1].name);
    for p in [&enc_path, &dec_path] {
        if !p.is_file() {
            bail!("模型文件不存在：{}", p.display());
        }
    }

    report(Report::Stage("载入解码器"));
    report(Report::Progress(0.02));
    let (net, cfg) = decoder::load_from_pth(&dec_path)?;

    report(Report::Stage("读取音频"));
    report(Report::Progress(0.05));
    let audio = wav::read(&job.input)?;
    if audio.samples.is_empty() {
        bail!("文件里没有音频数据");
    }
    let x = encoder::resample(&audio.samples, audio.sample_rate, cfg.sample_rate);
    if x.len() < cfg.block_size * 4 {
        bail!("音频太短：重采样后只有 {} 个样本，不够 4 帧", x.len());
    }

    report(Report::Stage("提取音高轨"));
    let track = voice_core::track_pitch(&x, cfg.sample_rate as f32, |p| {
        report(Report::Progress(0.08 + p * 0.17));
        true
    })
    .context("音高提取失败")?;
    if track.voiced_count() == 0 {
        bail!("整段音频里没有检测到人声 —— 确认录的是干声，不是伴奏");
    }

    report(Report::Stage("分析内容"));
    report(Report::Progress(0.28));
    let mut enc = encoder::ContentEncoder::open(&enc_path, 2)?;
    let content = enc.encode(&x, cfg.sample_rate)?;
    report(Report::Progress(0.72));

    let a = features::align(&track, &x, cfg.sample_rate, &content, cfg.block_size)?;
    if a.dim != cfg.n_unit {
        bail!(
            "内容特征是 {} 维，解码器要 {} 维 —— 编码器和解码器不配套",
            a.dim,
            cfg.n_unit
        );
    }
    if job.speaker == 0 || job.speaker > cfg.n_spk {
        bail!("声线编号 {} 越界（这个模型有 {} 个）", job.speaker, cfg.n_spk);
    }

    report(Report::Stage("重新合成"));
    report(Report::Progress(0.75));
    let exc = source::combtooth(&a.f0, cfg.sample_rate as f32, cfg.block_size);
    let ctrls = net.forward(
        &decoder::Inputs {
            units: &a.content,
            dim: a.dim,
            f0: &a.f0,
            phase: &exc.phase,
            volume: &a.volume,
        },
        job.speaker,
        &candle_core::Device::Cpu,
    )?;
    report(Report::Progress(0.90));

    let (hf, nf) = ctrls.filters();
    let y = synth::synthesize(&exc.combtooth, cfg.win_length, cfg.block_size, &hf, &nf, 1);

    check(&y, &a)?;
    report(Report::Progress(0.98));

    // ⚠️ **最后一步才落盘。** 取消就是父进程直接杀掉本进程，
    // 中途被杀不会留下半个文件 —— 一个"存在但内容是残的"产物，
    // 比没有产物糟得多。
    report(Report::Stage("写入文件"));
    wav::write(&job.output, &y, cfg.sample_rate)?;
    report(Report::Progress(1.0));
    Ok(job.output.clone())
}

/// 落盘之前的自检。
///
/// 这些是**机器能判**的全部：没有 NaN、不是静音、没有爆幅、长度对。
/// 像不像判不了，那是耳朵的事。
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

/// 不碰模型的自检 —— 确认这个二进制在这台机器上能跑（架构、依赖齐全）。
pub fn selftest() -> Result<()> {
    crate::session::init_pure_rust();
    let f0 = vec![220.0f32; 8];
    let s = source::combtooth(&f0, 44_100.0, 512);
    if s.combtooth.iter().any(|v| !v.is_finite()) {
        bail!("激励生成异常");
    }
    Ok(())
}

/// 产物路径：`<原名>-cloned.wav`，与原录音放在一起。
pub fn output_path(input: &Path) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    input.with_file_name(format!("{stem}-cloned.wav"))
}
