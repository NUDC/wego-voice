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
        #[cfg(all(feature = "candle", feature = "onnx"))]
        Some("convert") => {
            let enc: PathBuf = args
                .get(2)
                .context("用法：wego-neural convert <编码器.onnx> <解码器.pt> <干声.wav> <输出.wav>")?
                .into();
            let dec: PathBuf = args.get(3).context("缺少解码器检查点")?.into();
            let src: PathBuf = args.get(4).context("缺少干声 WAV")?.into();
            let out: PathBuf = args.get(5).context("缺少输出路径")?.into();
            convert(&enc, &dec, &src, &out)
        }
        #[cfg(feature = "candle")]
        Some("check") => {
            let p: PathBuf = args.get(2).context("用法：wego-neural check <检查点.pt>")?.into();
            check(&p)
        }
        #[cfg(feature = "candle")]
        Some("keys") => {
            let p: PathBuf = args.get(2).context("用法：wego-neural keys <检查点.pt>")?.into();
            keys(&p)
        }
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

    let a = voice_neural::align(
        &track,
        &audio.samples,
        audio.sample_rate,
        &content,
        voice_neural::BLOCK_SIZE,
    )?;

    println!("\n音高轨   {} 帧 @ {} 样本步进（{:.1} fps）",
             track.frames.len(), track.hop,
             audio.sample_rate as f32 / track.hop as f32);
    println!("内容特征 {} 帧 × {} 维（{:.1} fps）",
             content.frames, content.dim, content.frames as f32 / secs);
    println!("\n对齐后   {:?}", a);

    // 三路等长是这个模块唯一的承诺 —— 在真数据上再确认一次
    anyhow::ensure!(
        a.f0.len() == a.frames && a.voiced.len() == a.frames && a.volume.len() == a.frames,
        "各路长度不一致：f0={} voiced={} vol={} frames={}",
        a.f0.len(), a.voiced.len(), a.volume.len(), a.frames
    );
    anyhow::ensure!(
        a.content.len() == a.frames * a.dim,
        "内容特征长度 {} ≠ {}×{}", a.content.len(), a.frames, a.dim
    );

    let voiced_n = a.voiced.iter().filter(|v| **v).count();
    let f0s: Vec<f32> = a.f0.iter().copied().filter(|v| *v > 0.0).collect();
    let lo = a.volume.iter().copied().fold(f32::INFINITY, f32::min);
    let hi = a.volume.iter().copied().fold(f32::NEG_INFINITY, f32::max);

    println!("\n有声   {voiced_n} / {} 帧（{:.0}%）", a.frames, voiced_n as f32 / a.frames as f32 * 100.0);
    if !f0s.is_empty() {
        let mut v = f0s.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("f0     中位 {:.1} Hz，范围 {:.1}~{:.1} Hz", v[v.len()/2], v[0], v[v.len()-1]);
    }
    println!("音量   {lo:.5} ~ {hi:.5}（线性 RMS）");
    println!("\n耗时   音高 {t_f0:.2}s / 内容 {t_enc:.2}s（含载入模型）");

    anyhow::ensure!(a.f0.iter().all(|v| v.is_finite()), "f0 里有非有限值");
    anyhow::ensure!(a.volume.iter().all(|v| v.is_finite()), "音量里有非有限值");
    anyhow::ensure!(a.content.iter().all(|v| v.is_finite()), "内容特征里有非有限值");
    println!("\n✅ 三路等长、无非有限值");
    Ok(())
}


/// 列出检查点里的参数名与形状。
///
/// # 为什么这是第一步
///
/// 「权重名跟代码里假设的对不上」是这类移植失败的大头，而症状是
/// **能跑但声音怪**，不是报错。先把真实的名字打出来，
/// 比对着写，比写完再调省十倍力气。
#[cfg(feature = "candle")]
fn keys(path: &Path) -> Result<()> {
    let ks = voice_neural::decoder::dump_keys(path)?;
    println!("═══ {} ═══", path.display());
    println!("共 {} 个张量\n", ks.len());
    let mut total = 0usize;
    for (name, shape) in &ks {
        let n: usize = shape.iter().product();
        total += n;
        println!("  {name:<52} {shape:?}");
    }
    println!("\n参数量 {:.2} M", total as f64 / 1e6);
    Ok(())
}


/// 拿检查点跟本实现的期望对一遍。
#[cfg(feature = "candle")]
fn check(path: &Path) -> Result<()> {
    use voice_neural::decoder;
    let cfg = decoder::read_config(path)?;
    println!("═══ {} ═══\n", path.display());
    println!("从检查点读出来的结构：");
    println!("  采样率      {} Hz", cfg.sample_rate);
    println!("  block_size  {}（{:.2} fps）", cfg.block_size,
             cfg.sample_rate as f32 / cfg.block_size as f32);
    println!("  win_length  {}", cfg.win_length);
    println!("  频点数      {}（win/2+1）", cfg.bins);
    println!("  内容特征维  {}", cfg.n_unit);
    println!("  说话人数    {}", cfg.n_spk);
    println!();
    match decoder::check_against(path, cfg.n_unit, cfg.n_spk, cfg.bins) {
        Ok(r) => {
            println!("{r}");
            Ok(())
        }
        Err(e) => {
            println!("{e}");
            anyhow::bail!("权重名/形状对不上")
        }
    }
}


/// 整条链路跑一遍：干声 → 声线转换 → WAV。
///
/// # 这一步能验什么、不能验什么
///
/// **能验**：跑得通、长度对、没有 NaN、没有爆幅、可复现。
/// **不能验**：像不像。那是耳朵的事，我没有。
///
/// 所以这里把所有能机器判定的都判一遍，剩下的交给人听。
#[cfg(all(feature = "candle", feature = "onnx"))]
fn convert(enc_path: &Path, dec_path: &Path, wav: &Path, out: &Path) -> Result<()> {
    use voice_neural::{decoder, encoder, features, source, synth};

    println!("═══ 声线转换 ═══\n");

    // ── 1. 解码器（连带把结构参数读出来）──
    let (net, cfg) = decoder::load_from_pth(dec_path)?;
    println!(
        "解码器  {} Hz / block {} / win {} / {} 说话人",
        cfg.sample_rate, cfg.block_size, cfg.win_length, cfg.n_spk
    );

    // ── 2. 读干声并重采样到模型的采样率 ──
    let audio = read_wav_mono(wav)?;
    let x = encoder::resample(&audio.samples, audio.sample_rate, cfg.sample_rate);
    let secs = x.len() as f32 / cfg.sample_rate as f32;
    println!(
        "输入    {:.2} 秒（{} Hz → {} Hz）",
        secs, audio.sample_rate, cfg.sample_rate
    );

    // ── 3. 三路特征 ──
    let t0 = std::time::Instant::now();
    let track = voice_core::track_pitch(&x, cfg.sample_rate as f32, |_| true)
        .context("音高提取被取消")?;
    let mut enc = encoder::ContentEncoder::open(enc_path, 2)?;
    let content = enc.encode(&x, cfg.sample_rate)?;
    let a = features::align(&track, &x, cfg.sample_rate, &content, cfg.block_size)?;
    println!("特征    {a:?}（{:.1} 秒）", t0.elapsed().as_secs_f32());

    anyhow::ensure!(
        a.dim == cfg.n_unit,
        "内容特征是 {} 维，解码器要 {} 维 —— 编码器和解码器不配套",
        a.dim,
        cfg.n_unit
    );

    // ── 4. 激励 ──
    let exc = source::combtooth(&a.f0, cfg.sample_rate as f32, cfg.block_size);

    // ── 5. 网络 ──
    let t1 = std::time::Instant::now();
    let ctrls = net.forward(
        &decoder::Inputs {
            units: &a.content,
            dim: a.dim,
            f0: &a.f0,
            phase: &exc.phase,
            volume: &a.volume,
        },
        1,
        &candle_core::Device::Cpu,
    )?;
    println!("网络    {ctrls:?}（{:.1} 秒）", t1.elapsed().as_secs_f32());

    // ── 6. 合成 ──
    let (hf, nf) = ctrls.filters();
    let y = synth::synthesize(&exc.combtooth, cfg.win_length, cfg.block_size, &hf, &nf, 1);

    // ── 能机器判定的，全判一遍 ──
    let peak = y.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    let rms = (y.iter().map(|v| v * v).sum::<f32>() / y.len().max(1) as f32).sqrt();
    let nan = y.iter().filter(|v| !v.is_finite()).count();
    println!(
        "\n输出    {} 样本（{:.2} 秒）  峰值 {peak:.3}  RMS {rms:.4}  NaN {nan}",
        y.len(),
        y.len() as f32 / cfg.sample_rate as f32
    );
    anyhow::ensure!(nan == 0, "输出里有 {nan} 个 NaN");
    anyhow::ensure!(peak > 1e-4, "输出几乎是静音（峰值 {peak:.6}）");
    anyhow::ensure!(peak < 100.0, "输出爆幅（峰值 {peak:.1}）—— 多半是滤波器尺度错了");
    anyhow::ensure!(
        y.len() == a.frames * cfg.block_size,
        "输出长度 {} ≠ {} 帧 × {}",
        y.len(),
        a.frames,
        cfg.block_size
    );

    // ⚠️ 一条**机器能判**的关键性质：音高必须原样保住。
    //
    // 这整条路的承诺是"音高走你的，音色走他的"。音色像不像我听不出来，
    // 但音高有没有被改动是能量的 —— 而且它最容易出错：
    // 相位接力断了、f0 条件接错了、激励和滤波器错位，都会表现为音高跑掉。
    let back = voice_core::track_pitch(&y, cfg.sample_rate as f32, |_| true)
        .context("产物音高提取失败")?;
    let mut drift: Vec<f32> = Vec::new();
    for (t, f) in back.frames.iter().enumerate() {
        let pos = t * back.hop;
        let frame = pos / cfg.block_size;
        if !f.voiced || frame >= a.frames || !a.voiced[frame] || a.f0[frame] <= 0.0 {
            continue;
        }
        drift.push(1200.0 * (f.f0 / a.f0[frame]).log2());
    }
    if drift.is_empty() {
        println!("
⚠️ 产物里测不到浊音 —— 没法验证音高是否保住");
    } else {
        drift.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let med = drift[drift.len() / 2];
        let p90 = drift[drift.len() * 9 / 10].abs().max(drift[drift.len() / 10].abs());
        println!(
            "
音高保真  中位偏差 {med:+.1} 音分 / P90 |{p90:.1}| 音分（{} 帧）",
            drift.len()
        );
        anyhow::ensure!(
            med.abs() < 50.0,
            "输出音高整体偏了 {med:.0} 音分 —— 音高没保住，链路某处接错了"
        );
    }

    write_wav(out, &y, cfg.sample_rate)?;
    println!("已写入  {}", out.display());
    println!("\n⚠️ 机器只能验到这里。**像不像要靠耳朵** —— 我没有。");
    Ok(())
}

/// 写 32-bit float 单声道 WAV。
#[cfg(all(feature = "candle", feature = "onnx"))]
fn write_wav(path: &Path, x: &[f32], rate: u32) -> Result<()> {
    let bytes = (x.len() * 4) as u32;
    let mut v = Vec::with_capacity(44 + bytes as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + bytes).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&32u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&bytes.to_le_bytes());
    for s in x {
        v.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, &v).with_context(|| format!("写入失败：{}", path.display()))
}
