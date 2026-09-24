//! 神经件的检查工具。
//!
//! ```text
//! wego-neural inspect <模型.onnx>     打印输入/输出契约
//! wego-neural encode <模型.onnx> <干声.wav>   跑一遍内容编码器
//! ```
//!
//! # 为什么先做这个
//!
//! 「权重/图的形状跟代码里假设的对不上」是这类项目失败的大头，
//! 而症状是**声音怪**，不是报错 —— 几乎没法二分定位。
//!
//! 所以第一件事不是跑通推理，是把模型**到底长什么样**问出来，
//! 写进契约检查里。之后任何一次换模型、换来源、下错文件，
//! 都会在建会话那一刻以人话失败，而不是在听感上以玄学失败。

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // 后端二选一：tract（纯 Rust，静态链接）或 ONNX Runtime（外挂 DLL）。
    #[cfg(feature = "tract")]
    {
        voice_neural::session::init_pure_rust();
        eprintln!("[后端] tract（纯 Rust，无 DLL）");
    }
    #[cfg(not(feature = "tract"))]
    {
        let dll = std::env::var("WEGO_ORT_DLL")
            .map(PathBuf::from)
            .map_err(|_| anyhow::anyhow!("请用环境变量 WEGO_ORT_DLL 指向 onnxruntime.dll"))?;
        voice_neural::session::init(&dll)?;
        eprintln!("[后端] ONNX Runtime（{}）", dll.display());
    }

    match args.get(1).map(String::as_str) {
        Some("inspect") => {
            let p: PathBuf = args.get(2).context("用法：wego-neural inspect <模型.onnx>")?.into();
            inspect(&p)
        }
        Some("features") => {
            let m: PathBuf = args.get(2).context("用法：wego-neural features <模型.onnx> <干声.wav>")?.into();
            let w: PathBuf = args.get(3).context("缺少 WAV 路径")?.into();
            features(&m, &w)
        }
        Some("encode") => {
            let m: PathBuf = args.get(2).context("用法：wego-neural encode <模型.onnx> <干声.wav> [落盘.f32]")?.into();
            let w: PathBuf = args.get(3).context("缺少 WAV 路径")?.into();
            encode(&m, &w, args.get(4).map(PathBuf::from).as_deref())
        }
        _ => {
            println!("{}", include_str!("../../README.txt"));
            Ok(())
        }
    }
}

fn inspect(path: &Path) -> Result<()> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    println!("═══ {} ═══", path.display());
    println!("文件大小 {:.1} MB\n", size as f64 / 1048576.0);

    let (_session, contract) = voice_neural::session::open(path, 2)?;
    print!("{}", contract.describe());
    Ok(())
}

fn encode(model: &Path, wav: &Path, dump: Option<&Path>) -> Result<()> {
    let mut enc = voice_neural::encoder::ContentEncoder::open(model, 2)?;
    println!("═══ 内容编码器 ═══");
    print!("{}", enc.contract().describe());

    let audio = read_wav_mono(wav)?;
    println!(
        "\n输入 {} —— {:.2} 秒 @ {} Hz",
        wav.display(),
        audio.samples.len() as f32 / audio.sample_rate as f32,
        audio.sample_rate
    );

    let t0 = std::time::Instant::now();
    let feat = enc.encode(&audio.samples, audio.sample_rate)?;
    let dt = t0.elapsed().as_secs_f32();

    let secs = audio.samples.len() as f32 / audio.sample_rate as f32;
    println!(
        "\n特征 {} 帧 × {} 维（{:.1} fps）",
        feat.frames,
        feat.dim,
        feat.frames as f32 / secs
    );
    println!("耗时 {:.2} 秒（{:.1}× 实时）", dt, secs / dt.max(1e-6));

    // 特征本身没法"看对不对"，但**明显坏掉**是看得出来的：
    // 全零、全 NaN、方差为 0 —— 这几种都说明前面某处静默失败了。
    let (min, max, mean, nan) = stats(&feat.data);
    println!("\n取值 min {min:.3} / max {max:.3} / mean {mean:.3} / NaN {nan}");
    if nan > 0 {
        bail!("特征里有 {nan} 个 NaN —— 前面某处静默失败了");
    }
    if (max - min).abs() < 1e-6 {
        bail!("特征是常数（min==max）—— 多半喂进去的是静音，或者重采样坏了");
    }
    // 落盘是为了**逐元素**比对两个后端。统计量一样只能说明"没明显坏掉"，
    // 说明不了两条实现算的是同一个东西。
    if let Some(p) = dump {
        let mut bytes = Vec::with_capacity(feat.data.len() * 4);
        for v in &feat.data {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(p, &bytes)?;
        println!("已落盘 {} —— {} 个 f32", p.display(), feat.data.len());
    }

    println!("\n✅ 编码器跑通");
    Ok(())
}

fn stats(v: &[f32]) -> (f32, f32, f32, usize) {
    let nan = v.iter().filter(|x| !x.is_finite()).count();
    let fin: Vec<f32> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if fin.is_empty() {
        return (0.0, 0.0, 0.0, nan);
    }
    let min = fin.iter().copied().fold(f32::INFINITY, f32::min);
    let max = fin.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let mean = fin.iter().sum::<f32>() / fin.len() as f32;
    (min, max, mean, nan)
}

struct Mono {
    samples: Vec<f32>,
    sample_rate: u32,
}

/// 只读 32-bit float / PCM 单声道 WAV —— 够用就行。
///
/// 刻意不依赖 `voice-audio`：那个 crate 拖着 WASAPI 和整条实时链路，
/// 而这里只需要把一段波形读进来。
fn read_wav_mono(path: &Path) -> Result<Mono> {
    let b = std::fs::read(path).with_context(|| format!("读取失败：{}", path.display()))?;
    if b.len() < 44 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        bail!("不是 WAV 文件：{}", path.display());
    }
    let u16le = |at: usize| u16::from_le_bytes([b[at], b[at + 1]]);
    let u32le = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);

    let (mut fmt, mut data) = (None, None);
    let mut pos = 12usize;
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let size = u32le(pos + 4) as usize;
        let body = pos + 8;
        if id == b"fmt " && body + 16 <= b.len() {
            fmt = Some((u16le(body), u16le(body + 2), u32le(body + 4), u16le(body + 14)));
        } else if id == b"data" {
            data = Some((body, size.min(b.len().saturating_sub(body))));
        }
        if body + size > b.len() {
            break;
        }
        pos = body + size + (size & 1);
    }
    let (format, ch, rate, bits) = fmt.context("缺少 fmt chunk")?;
    let (off, len) = data.context("缺少 data chunk")?;
    let ch = ch.max(1) as usize;
    let bytes = (bits / 8).max(1) as usize;

    let frames = len / (bytes * ch);
    let mut out = Vec::with_capacity(frames);
    for f in 0..frames {
        let mut acc = 0.0f32;
        for c in 0..ch {
            let p = off + (f * ch + c) * bytes;
            acc += match (format, bits) {
                (1, 16) => i16::from_le_bytes([b[p], b[p + 1]]) as f32 / 32768.0,
                (3, 32) => f32::from_le_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]),
                _ => bail!("不支持的 WAV 格式：format={format} bits={bits}"),
            };
        }
        out.push(acc / ch as f32);
    }
    Ok(Mono { samples: out, sample_rate: rate })
}


/// 整条特征管线跑一遍：f0 + 响度 + 内容，三路对齐。
///
/// 单元测试验的是不变量（已知事件落在已知帧上、加 6 dB 响度涨 6 dB）。
/// 这条命令验的是**真模型 + 真音频**下三路确实等长、确实同步 ——
/// 单测里内容特征是假的，对不上真实帧数这种错它抓不到。
fn features(model: &Path, wav: &Path) -> Result<()> {
    let audio = read_wav_mono(wav)?;
    let secs = audio.samples.len() as f32 / audio.sample_rate as f32;
    println!("═══ 特征管线 ═══\n");
    println!("输入 {:.2} 秒 @ {} Hz", secs, audio.sample_rate);

    let t0 = std::time::Instant::now();
    let track = voice_core::track_pitch(&audio.samples, audio.sample_rate as f32, |_| true)
        .context("音高提取被取消")?;
    let t_f0 = t0.elapsed().as_secs_f32();

    let t1 = std::time::Instant::now();
    let mut enc = voice_neural::encoder::ContentEncoder::open(model, 2)?;
    let content = enc.encode(&audio.samples, audio.sample_rate)?;
    let t_enc = t1.elapsed().as_secs_f32();

    let a = voice_neural::align(&track, &audio.samples, audio.sample_rate, &content)?;

    println!("\n音高轨   {} 帧 @ {} 样本步进（{:.1} fps）",
             track.frames.len(), track.hop,
             audio.sample_rate as f32 / track.hop as f32);
    println!("内容特征 {} 帧 × {} 维（{:.1} fps）",
             content.frames, content.dim, content.frames as f32 / secs);
    println!("\n对齐后   {:?}", a);

    // 三路等长是这个模块唯一的承诺 —— 在真数据上再确认一次
    anyhow::ensure!(
        a.f0.len() == a.frames && a.voiced.len() == a.frames && a.loudness_db.len() == a.frames,
        "三路长度不一致：f0={} voiced={} loud={} frames={}",
        a.f0.len(), a.voiced.len(), a.loudness_db.len(), a.frames
    );
    anyhow::ensure!(
        a.content.len() == a.frames * a.dim,
        "内容特征长度 {} ≠ {}×{}", a.content.len(), a.frames, a.dim
    );

    let voiced_n = a.voiced.iter().filter(|v| **v).count();
    let f0s: Vec<f32> = a.f0.iter().copied().filter(|v| *v > 0.0).collect();
    let lo = a.loudness_db.iter().copied().fold(f32::INFINITY, f32::min);
    let hi = a.loudness_db.iter().copied().fold(f32::NEG_INFINITY, f32::max);

    println!("\n有声   {voiced_n} / {} 帧（{:.0}%）", a.frames, voiced_n as f32 / a.frames as f32 * 100.0);
    if !f0s.is_empty() {
        let mut v = f0s.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("f0     中位 {:.1} Hz，范围 {:.1}~{:.1} Hz", v[v.len()/2], v[0], v[v.len()-1]);
    }
    println!("响度   {lo:.1} ~ {hi:.1} dBFS");
    println!("\n耗时   音高 {t_f0:.2}s / 内容 {t_enc:.2}s（含载入模型）");

    anyhow::ensure!(a.f0.iter().all(|v| v.is_finite()), "f0 里有非有限值");
    anyhow::ensure!(a.loudness_db.iter().all(|v| v.is_finite()), "响度里有非有限值");
    anyhow::ensure!(a.content.iter().all(|v| v.is_finite()), "内容特征里有非有限值");
    println!("\n✅ 三路等长、无非有限值");
    Ok(())
}
