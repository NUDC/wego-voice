//! `wego-bench` —— Phase 0 线 A 的 headless 压测入口。
//!
//! 它只回答一个问题：**端到端耳返延迟能不能稳定压到 30ms 以内？**
//!
//! ```text
//! wego-bench devices                       列出音频设备
//! wego-bench                               默认 30 秒压测（等价于 soak）
//! wego-bench soak -d 600 --mute            10 分钟连测，查时钟漂移
//! wego-bench soak --backend cpal           用共享模式做对照
//! wego-bench latency --rounds 50 --mute    脉冲实测往返延迟
//! wego-bench sweep                         扫描缓冲大小，找不爆音的最小值
//! wego-bench soak --no-rt                  关掉实时优先级，做对照实验
//! ```
//!
//! # 日志与输出是两回事
//!
//! 报告表格走 `println!` —— 那是**程序的输出**，用户要看的结果。
//! 警告与错误走 `log::warn!` / `log::error!` —— 那是**诊断信息**。
//!
//! 把两者混在一起是常见错误：结果会被日志级别过滤掉，
//! 或者日志混进了要被 `grep` 解析的输出里。
//!
//! 调日志级别用环境变量：`RUST_LOG=debug wego-bench soak`

use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Args as ClapArgs, Parser, Subcommand};
use voice_audio::{
    list_devices, parse_key, thresholds, AudioEngine, BackendKind, EngineConfig, LatencyStats,
};

#[derive(Parser, Debug)]
#[command(
    name = "wego-bench",
    about = "wego-voice 实时音频链路压测",
    long_about = "Phase 0 线 A 的 headless 压测入口。\n\
                  只回答一个问题：端到端耳返延迟能不能稳定压到 30ms 以内？\n\n\
                  ⚠️ 戴上耳机再跑。外放会让麦克风拾到自己的输出，形成啸叫回路。\n\
                  不想出声就加 --mute：DSP 照常跑，只是不送耳返。",
    version
)]
struct Cli {
    /// 不给子命令时默认执行 soak
    #[command(subcommand)]
    cmd: Option<Cmd>,

    #[command(flatten)]
    audio: AudioOpts,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 列出音频设备后退出
    Devices,

    /// 连续压测并给出 Phase 0 判定（默认命令）
    Soak {
        /// 压测时长（秒）。
        ///
        /// ⚠️ 漂移与偶发 xrun 具有聚集性，180 秒窗口测不出来 ——
        /// 正式结论必须跑满 600 秒。
        #[arg(short, long, default_value_t = 30.0)]
        duration: f32,
    },

    /// 脉冲实测往返延迟（需回环装置）
    Latency {
        /// 测量轮数。看中位数，不看单次。
        #[arg(long, default_value_t = 25)]
        rounds: usize,

        /// 测完之后继续压测多久（秒）
        #[arg(short, long, default_value_t = 10.0)]
        duration: f32,
    },

    /// 扫描缓冲大小，找不产生 xrun 的最小值
    ///
    /// ⚠️ WASAPI 共享模式下无效（设备无视请求值）；独占模式下才有意义。
    Sweep {
        /// 每档跑多久（秒）
        #[arg(short, long, default_value_t = 15.0)]
        duration: f32,
    },

    /// 离线量 DSP 成本，不碰声卡
    ///
    /// # 为什么需要这个
    ///
    /// `soak` 用的是真实麦克风输入。没人对着麦克风唱的时候信号判为清音，
    /// PSOLA 走透传、`overlap_add` 一次都不执行 —— 于是 soak 测出来的
    /// "CPU 占用"跟 DSP 的真实成本**毫无关系**。
    ///
    /// 这个子命令喂合成浊音，逼着走完整的分析+合成路径，
    /// 是唯一能把"某个参数贵不贵"量准的地方。
    /// 离线重新校准一个 WAV
    ///
    /// 实时链路被 30ms 预算捆着：不能回看、f0 只能因果、PSOLA 窗口被
    /// f0_floor 截断。离线这三条限制都没有，所以质量应当明显更好。
    ///
    /// 产出的音高轨同时是 DDSP-SVC 那条路的必需输入。
    Recorrect {
        /// 输入干声 WAV
        #[arg(long)]
        input: String,

        /// 输出 WAV
        #[arg(long)]
        output: String,
    },

    /// 比较两段音频的声线，输出角色参数建议
    ///
    /// 这条路径的 UI 入口要靠拖拽文件，没法自动化验证；
    /// 命令行入口能把「读 WAV → 分析 → 匹配」整条链路跑通。
    Timbre {
        /// 参考音频（想模拟的那个声线）
        #[arg(long)]
        reference: String,

        /// 你自己的干声（起点）
        #[arg(long)]
        source: String,
    },

    Dsp {
        /// 模拟多少秒的音频
        #[arg(short, long, default_value_t = 30.0)]
        duration: f32,

        /// 每块多少帧。用设备实际块大小才有可比性。
        #[arg(long, default_value_t = 144)]
        block: usize,
    },
}

/// 所有模式共用的音频参数。
#[derive(ClapArgs, Debug, Clone)]
struct AudioOpts {
    /// 音频后端：auto / cpal / wasapi
    ///
    /// auto   = 优先独占，失败退回共享
    /// cpal   = 共享模式，兼容性好但延迟高（实测 55ms）
    /// wasapi = 独占模式，低延迟（实测 29.92ms）但独占声卡
    #[arg(long, global = true, default_value = "auto", value_parser = parse_backend)]
    backend: BackendKind,

    /// 输入设备（子串匹配，缺省用系统默认）
    #[arg(long, global = true)]
    input: Option<String>,

    /// 输出设备（子串匹配，缺省用系统默认）
    #[arg(long, global = true)]
    output: Option<String>,

    /// 请求的缓冲大小（帧）。共享模式下会被设备忽略。
    #[arg(short, long, global = true, default_value_t = 256)]
    buffer: u32,

    /// 采样率
    #[arg(long, global = true, default_value_t = 48_000)]
    rate: u32,

    /// PSOLA 基频下限（Hz），决定 DSP 算法延迟。
    ///
    /// 100→20ms  130→15.4ms  160→12.5ms。
    /// 调高则延迟降低，但低于该频率的男声音质下降。
    #[arg(long, global = true, default_value_t = 130.0, value_parser = parse_f0_floor)]
    f0_floor: f32,

    /// 环形缓冲目标水位（以输出块为单位）。
    ///
    /// 实测 2.5 是不产生 xrun 的最小值；1.5 会被抽干。
    #[arg(long, global = true, default_value_t = 2.5, value_parser = parse_target_fill)]
    target_fill: f32,

    /// 关闭实时线程提权（对照实验用）
    #[arg(long, global = true)]
    no_rt: bool,

    /// 静音耳返：DSP 照常跑，只是不送声音（避免啸叫）
    #[arg(long, global = true)]
    mute: bool,

    /// 调名，如 C、Am、F#m。缺省为半音阶（只吸附到最近半音）。
    #[arg(long, global = true)]
    key: Option<String>,

    /// 修正速度（毫秒）。0 = 电音档。
    #[arg(long, global = true)]
    retune: Option<f32>,

    /// 角色：整体移调（半音）。
    #[arg(long, global = true)]
    pitch_shift: Option<f32>,

    /// 压测时同时录干声到指定文件（WAV，32-bit float 单声道）。
    ///
    /// 这条路径的单元测试只覆盖了 WAV 写入器本身；
    /// **真实采集线程有没有喂进来，只能这样实跑验证**。
    #[arg(long, global = true)]
    record: Option<String>,

    /// 角色：频谱倾斜（dB/八度）。正 = 更亮。
    #[arg(long, global = true)]
    tilt: Option<f32>,

    /// 噪声门余量（dB）。人声要高出实测本底这么多才进入音高检测。
    /// 0 = 关掉门。默认 12。
    #[arg(long, global = true)]
    noise_gate_db: Option<f32>,

    /// 角色：共振峰平移（半音）。
    ///
    /// ⚠️ 非 0 会让 PSOLA 走**逐样本插值**路径 —— 这是声线功能真正的
    /// CPU 代价所在，量 CPU 时务必带上这个参数，否则测的是零成本的快路径。
    #[arg(long, global = true)]
    formant_shift: Option<f32>,
}

fn parse_backend(s: &str) -> Result<BackendKind, String> {
    BackendKind::parse(s).ok_or_else(|| format!("未知后端：{s}（可选 auto / cpal / wasapi）"))
}

fn parse_f0_floor(s: &str) -> Result<f32, String> {
    let v: f32 = s.parse().map_err(|_| format!("不是数字：{s}"))?;
    // 60Hz 以下低于人声基频范围；260Hz 以上会连女声都截断
    if (60.0..=260.0).contains(&v) {
        Ok(v)
    } else {
        Err(format!("f0-floor 应在 60~260 Hz 之间，给的是 {v}"))
    }
}

fn parse_target_fill(s: &str) -> Result<f32, String> {
    let v: f32 = s.parse().map_err(|_| format!("不是数字：{s}"))?;
    // 低于 1.0 连一个输出块都装不下，必然欠载；高于 6.0 延迟已完全失控
    if (1.0..=6.0).contains(&v) {
        Ok(v)
    } else {
        Err(format!("target-fill 应在 1.0~6.0 之间，给的是 {v}"))
    }
}

/// 把 clap 的参数摊平成后续函数用的形状。
///
/// 保留这一层是因为报告与压测函数需要「时长」「轮数」这些跨命令的值，
/// 而它们分散在不同子命令里。
struct Args {
    duration: f32,
    buffer: u32,
    sample_rate: Option<u32>,
    realtime: bool,
    latency_rounds: usize,
    input: Option<String>,
    output: Option<String>,
    key: Option<String>,
    retune_ms: Option<f32>,
    pitch_shift: Option<f32>,
    formant_shift: Option<f32>,
    noise_gate_db: Option<f32>,
    tilt: Option<f32>,
    record: Option<String>,
    muted: bool,
    f0_floor: f32,
    backend: BackendKind,
    target_fill: f32,
}

impl Args {
    fn new(audio: &AudioOpts, duration: f32, latency_rounds: usize) -> Self {
        Self {
            duration,
            buffer: audio.buffer,
            sample_rate: Some(audio.rate),
            realtime: !audio.no_rt,
            latency_rounds,
            input: audio.input.clone(),
            output: audio.output.clone(),
            key: audio.key.clone(),
            retune_ms: audio.retune,
            pitch_shift: audio.pitch_shift,
            formant_shift: audio.formant_shift,
            noise_gate_db: audio.noise_gate_db,
            tilt: audio.tilt,
            record: audio.record.clone(),
            muted: audio.mute,
            f0_floor: audio.f0_floor,
            backend: audio.backend,
            target_fill: audio.target_fill,
        }
    }
}

fn main() -> Result<()> {
    // 默认 info：警告要能被看见，不必先设 RUST_LOG。
    // 需要更细就 `RUST_LOG=debug`。
    // 默认 info，但把第三方库压到 warn。
    //
    // `audio_thread_priority` 每次提权都会打一条 INFO，而且是**从音频线程打的**
    // （我们控制不了它，只能过滤）。不压下去的话，真正要看的警告会被淹掉。
    env_logger::Builder::from_env(
        env_logger::Env::default()
            .default_filter_or("warn,voice_audio=info,voice_core=info,wego_bench=info,wego_voice_lib=info,wego_voice_app=info"),
    )
    .format_timestamp(None)
    .format_target(false)
    .init();

    let cli = Cli::parse();

    // 不给子命令 = soak，保持「直接跑」的便利
    let cmd = cli.cmd.unwrap_or(Cmd::Soak { duration: 30.0 });

    match cmd {
        Cmd::Devices => {
            let d = list_devices()?;
            println!("音频后端：{}", d.host);
            println!("\n输入设备：");
            for n in &d.inputs {
                let mark = if Some(n) == d.default_input.as_ref() { " (默认)" } else { "" };
                println!("  - {n}{mark}");
            }
            println!("\n输出设备：");
            for n in &d.outputs {
                let mark = if Some(n) == d.default_output.as_ref() { " (默认)" } else { "" };
                println!("  - {n}{mark}");
            }
            Ok(())
        }

        Cmd::Sweep { duration } => {
            let args = Args::new(&cli.audio, duration, 0);
            buffer_sweep(&args)
        }

        Cmd::Soak { duration } => run(&Args::new(&cli.audio, duration, 0), false),

        Cmd::Latency { rounds, duration } => {
            run(&Args::new(&cli.audio, duration, rounds), true)
        }

        Cmd::Dsp { duration, block } => dsp_cost(&cli.audio, duration, block),

        Cmd::Timbre { reference, source } => timbre_report(&reference, &source),

        Cmd::Recorrect { input, output } => recorrect_file(&cli.audio, &input, &output),
    }
}

/// 离线重新校准：读 WAV → 提音高轨 → 重新校准 → 写 WAV。
fn recorrect_file(audio: &AudioOpts, input: &str, output: &str) -> Result<()> {
    use std::io::Write;
    use voice_audio::wav;

    let src = wav::read(input)?;
    let sr = src.sample_rate as f32;
    println!(
        "【输入】{}  {:.1}s  {}ch @{}Hz",
        input,
        src.duration_secs(),
        src.channels,
        src.sample_rate
    );

    // 取 String 而不是 &str：闭包要 move 进去，借用活不过返回
    let bar = |label: String| {
        let mut last = -1i32;
        move |p: f32| -> bool {
            let pct = (p * 100.0) as i32 / 5 * 5;
            if pct != last {
                last = pct;
                print!("\r  {label} {pct:3}%");
                let _ = std::io::stdout().flush();
            }
            true
        }
    };

    let t0 = std::time::Instant::now();
    let track = voice_core::track_pitch(&src.samples, sr, bar("提取音高".into())).unwrap();
    println!();

    println!("【音高轨】");
    println!(
        "  {} 帧（步进 {} = {:.1}ms），浊音 {} 帧（{:.0}%）",
        track.frames.len(),
        track.hop,
        track.hop as f32 * 1000.0 / sr,
        track.voiced_count(),
        track.voiced_count() as f32 / track.frames.len().max(1) as f32 * 100.0
    );
    println!("  中位基频    {:.1} Hz", track.median_f0());
    println!(
        "  八度纠错    {} 帧   ← 实时链路看不到邻域，这些修不了",
        track.octave_fixes
    );
    println!("  补洞        {} 帧", track.gap_fills);

    let key = audio
        .key
        .as_deref()
        .and_then(parse_key)
        .unwrap_or_default();
    let cfg = voice_core::RecorrectConfig {
        key,
        retune_ms: audio.retune.unwrap_or(40.0),
        intent_ms: 150.0,
        pitch_shift: audio.pitch_shift.unwrap_or(0.0),
        formant_shift: audio.formant_shift.unwrap_or(0.0),
    };

    let out = voice_core::recorrect(&src.samples, sr, &track, cfg, bar("重新校准".into())).unwrap();
    println!();

    wav::write(output, &out, src.sample_rate)?;
    let secs = t0.elapsed().as_secs_f32();
    println!("【输出】{output}");
    println!(
        "  {:.1}s 音频耗时 {:.1}s（{:.1}× 实时）",
        src.duration_secs(),
        secs,
        src.duration_secs() / secs.max(1e-6)
    );
    Ok(())
}

/// 声线比较。读两个 WAV，输出角色参数建议。
fn timbre_report(reference: &str, source: &str) -> Result<()> {
    use voice_audio::wav;

    // 与 `suggest_character` 保持一致：参考素材截到两分钟。
    // 两边算法一样但读入不一样的话，bench 的结论就不能用来解释应用的行为。
    let r = wav::read_capped(reference, wav::MAX_SECS)?;
    let s = wav::read_capped(source, wav::MAX_SECS)?;

    let a = voice_core::analyze_timbre(&s.samples, s.sample_rate as f32);
    let b = voice_core::analyze_timbre(&r.samples, r.sample_rate as f32);
    let m = voice_core::match_to(&a, &b);

    let line = |tag: &str, p: &voice_core::TimbreProfile, au: &wav::Audio| {
        println!(
            "  {tag:<8} {:.1}s（浊音 {:.1}s） {}ch @{}Hz  f0 中位 {:.0}Hz（{:.0}~{:.0}）{}",
            au.duration_secs(),
            p.voiced_secs,
            au.channels,
            au.sample_rate,
            p.median_f0,
            p.f0_low,
            p.f0_high,
            if p.is_usable() { "" } else { "  ⚠️ 素材不合格" }
        );
    };

    println!("【素材】");
    line("我的", &a, &s);
    line("参考", &b, &r);

    println!("
【建议】");
    println!(
        "  共振峰平移   {:+.1} 半音   ← 这是声线的主维度，也是唯一会被采纳的值",
        m.formant_shift
    );
    println!("  把握         {:.0}%", m.confidence * 100.0);

    println!("
【实测但不采纳】");
    println!(
        "  音高差       {:+.1} 半音   参考音源{}，但改音高就不是这首歌了",
        m.pitch_delta,
        if m.pitch_delta > 0.0 { "更高" } else { "更低" }
    );
    println!(
        "  频谱倾斜差   {:+.1} dB/八度  参考{}，当前引擎没有 EQ 环节，补不了",
        m.tilt_delta,
        if m.tilt_delta > 0.0 { "更亮" } else { "更暗" }
    );

    if !a.is_usable() || !b.is_usable() {
        println!(
            "
⚠️ 至少要 {:.1} 秒浊音才有意义 —— 上面的数字不要用。",
            voice_core::timbre::MIN_VOICED_SECS
        );
    } else if m.confidence < 0.4 {
        println!("
⚠️ 把握不足 40%：两段谱包络差异不明显，换更长更干净的素材再试。");
    }
    Ok(())
}

/// 离线 DSP 成本测量。不碰声卡，喂合成浊音。
fn dsp_cost(audio: &AudioOpts, duration: f32, block: usize) -> Result<()> {
    use std::time::Instant;
    use voice_core::{Corrector, CorrectorConfig};

    let sr = audio.rate as f32;
    let mut cfg = CorrectorConfig::for_sample_rate(sr);
    cfg.psola.latency_f0_floor = audio.f0_floor;
    if let Some(k) = &audio.key {
        if let Some(key) = parse_key(k) {
            cfg.key = key;
        }
    }

    let mut c = Corrector::new(cfg);
    if let Some(v) = audio.retune {
        c.set_retune_ms(v);
    }
    c.set_pitch_shift(audio.pitch_shift.unwrap_or(0.0));
    c.set_formant_shift(audio.formant_shift.unwrap_or(0.0));
    c.set_tilt_db_per_oct(audio.tilt.unwrap_or(0.0));

    // 带共振峰的合成浊音：冲激串过二阶谐振器。
    // 纯正弦也能跑通，但它没有频谱包络，测不出共振峰平移的真实开销。
    let total = (duration * sr) as usize;
    let f0 = 180.0f32;
    let period = (sr / f0).round().max(2.0) as usize;
    let (r, theta) = (0.96f32, std::f32::consts::TAU * 900.0 / sr);
    let (a1, a2) = (2.0 * r * theta.cos(), -r * r);
    let (mut y1, mut y2) = (0.0f32, 0.0f32);
    let mut input = Vec::with_capacity(total);
    for i in 0..total {
        let x = if i % period == 0 { 1.0 } else { 0.0 };
        let y = x + a1 * y1 + a2 * y2;
        input.push(y * 0.3);
        y2 = y1;
        y1 = y;
    }

    let mut out = vec![0.0f32; block];
    let mut times = Vec::with_capacity(total / block + 1);
    for chunk in input.chunks(block) {
        let n = chunk.len();
        let t0 = Instant::now();
        c.process(chunk, &mut out[..n]);
        times.push(t0.elapsed().as_nanos() as u64);
    }

    times.sort_unstable();
    let budget_us = block as f32 * 1e6 / sr;
    let ns = |p: f32| times[((times.len() - 1) as f32 * p) as usize] as f32 / 1000.0;
    let avg = times.iter().sum::<u64>() as f32 / times.len() as f32 / 1000.0;

    println!("【离线 DSP 成本】");
    println!("  采样率 {sr:.0} Hz  块 {block} 帧  f0_floor {} Hz", audio.f0_floor);
    println!(
        "  移调 {:+.1} 半音  共振峰 {:+.1} 半音{}",
        audio.pitch_shift.unwrap_or(0.0),
        audio.formant_shift.unwrap_or(0.0),
        if audio.formant_shift.unwrap_or(0.0).abs() < 1e-4 {
            "（整数快路径）"
        } else {
            "（逐样本插值）"
        }
    );
    println!("  {} 块 / {:.0} 秒音频", times.len(), duration);
    println!(
        "  每块耗时 平均 {avg:.1} / 中位 {:.1} / P99 {:.1} / 最大 {:.1} µs",
        ns(0.5),
        ns(0.99),
        ns(1.0)
    );
    println!(
        "  占回调预算（{budget_us:.0} µs） 平均 {:.1}% / P99 {:.1}%",
        avg / budget_us * 100.0,
        ns(0.99) / budget_us * 100.0
    );
    println!("  DSP 欠载 {}", c.underruns());
    Ok(())
}

fn run(args: &Args, measure: bool) -> Result<()> {
    let cfg = EngineConfig {
        buffer_frames: args.buffer,
        sample_rate: args.sample_rate,
        target_fill_blocks: args.target_fill,
        input_device: args.input.clone(),
        output_device: args.output.clone(),
        realtime_priority: args.realtime,
        f0_floor: args.f0_floor,
        backend: args.backend,
    };

    if !args.muted {
        log::warn!("耳返未静音 —— 请戴耳机，否则会形成啸叫回路（加 --mute 可静音）");
    }

    let mut engine = AudioEngine::start(&cfg)?;
    apply_params(&engine, args);
    print_engine_info(&mut engine);

    if measure {
        measure_latency(&engine, args.latency_rounds)?;
    }

    if let Some(path) = &args.record {
        engine.start_recording(path)?;
        println!("● 开始录干声 → {path}");
    }

    run_soak(&engine, args.duration)?;

    if args.record.is_some() {
        let (_, secs, dropped, path) = engine.recording_status();
        engine.stop_recording()?;
        println!("■ 录音结束：{secs:.2} 秒，丢弃 {dropped} 样本");
        if let Some(p) = path {
            match std::fs::metadata(&p) {
                Ok(md) => println!("   {} （{} 字节）", p.display(), md.len()),
                Err(e) => println!("   ⚠️ 文件不可读：{e}"),
            }
        }
        if dropped > 0 {
            println!("   ⚠️ 有丢帧，文件里会有细微断裂");
        }
    }

    print_report(&engine, args);
    Ok(())
}

fn apply_params(engine: &AudioEngine, args: &Args) {
    if let Some(k) = &args.key {
        match parse_key(k) {
            Some(key) => engine.params.set_key(key),
            None => eprintln!("[警告] 无法解析调名 {k:?}，沿用默认"),
        }
    }
    if let Some(ms) = args.retune_ms {
        engine.params.set_retune_ms(ms);
    }
    if let Some(v) = args.pitch_shift {
        engine.params.set_pitch_shift(v);
    }
    if let Some(v) = args.formant_shift {
        engine.params.set_formant_shift(v);
    }
    if let Some(v) = args.noise_gate_db {
        engine.params.set_noise_gate_db(v);
    }
    if let Some(v) = args.tilt {
        engine.params.set_tilt_db_per_oct(v);
    }
    engine
        .params
        .monitor_muted
        .store(args.muted, std::sync::atomic::Ordering::Relaxed);
}

fn print_engine_info(engine: &mut AudioEngine) {
    // 共享模式下实际块大小要跑几个回调才知道，先等一会儿再回填
    thread::sleep(Duration::from_millis(400));
    engine.refresh();

    let i = engine.info();
    let ms_per_block = |f: u32| f as f32 / i.sample_rate as f32 * 1000.0;

    println!("后端        {}{}", i.backend, if i.exclusive { "  ⚠️ 独占设备" } else { "" });
    println!("输入设备    {} （{} 声道，{}）", i.input_device, i.input_channels, i.input_format);
    println!("输出设备    {} （{} 声道，{}）", i.output_device, i.output_channels, i.output_format);
    println!("采样率      {} Hz", i.sample_rate);

    println!(
        "请求缓冲    {} 帧（{:.2} ms）",
        i.requested_buffer_frames,
        ms_per_block(i.requested_buffer_frames)
    );
    if i.output_block_frames == 0 {
        println!("实际缓冲    <尚未跑起来>");
    } else if i.buffer_size_honored() {
        println!(
            "实际缓冲    输入 {} 帧 / 输出 {} 帧 ✅ 设备接受了请求值",
            i.input_block_frames, i.output_block_frames
        );
    } else {
        println!(
            "实际缓冲    输入 {} 帧（{:.2} ms）/ 输出 {} 帧（{:.2} ms）",
            i.input_block_frames,
            ms_per_block(i.input_block_frames),
            i.output_block_frames,
            ms_per_block(i.output_block_frames)
        );
        if !i.exclusive {
            println!("            ⚠️ 共享模式按设备自身周期给块，请求值无效。");
            println!("              低延迟请用 --backend wasapi（独占模式）。");
        }
    }
    println!("实时优先级  {}", if i.realtime_priority { "已请求" } else { "关闭" });
    println!(
        "DSP 算法延迟 {:.2} ms（PSOLA，由 --f0-floor 决定）",
        i.algorithmic_ms
    );
    let theo = i.theoretical_latency_ms();
    println!("理论延迟    {:.2} ms —— {}", theo, thresholds::verdict(theo));
    println!("            （不含驱动与硬件固有延迟，是乐观下界）\n");
}

/// 脉冲往返延迟测量。
fn measure_latency(engine: &AudioEngine, rounds: usize) -> Result<()> {
    println!("── 往返延迟实测 ──");
    println!("三种接法，精度递减：");
    println!("  ① 回环线（输出口→输入口）—— 最准，纯电气链路");
    println!("  ② 耳机贴住麦克风 —— 次之，多几厘米声程");
    println!("  ③ 外放 + 内置麦克风 —— 可用，但含扬声器到麦克风的声程");
    println!("     （约 3ms/米，最后要从结果里扣掉）");
    println!("建议配合 --mute：耳返静音、脉冲照发，避免声学啸叫回路。");
    println!("\n测量中，共 {rounds} 轮…\n");

    let mut samples = Vec::with_capacity(rounds);
    let mut last_seen = engine.probe.completed();

    for r in 0..rounds {
        engine.probe.arm();
        let deadline = Instant::now() + Duration::from_millis(800);
        loop {
            thread::sleep(Duration::from_millis(5));
            let done = engine.probe.completed();
            if done > last_seen {
                last_seen = done;
                samples.push(engine.probe.last_us());
                break;
            }
            if Instant::now() > deadline {
                eprintln!("  第 {} 轮超时（没检测到回声）", r + 1);
                break;
            }
        }
        thread::sleep(Duration::from_millis(60));
    }

    if samples.is_empty() {
        let peak = engine.probe.peak_seen();
        let floor = engine.probe.noise_floor();
        let thr = engine.probe.threshold();
        println!("❌ 一次也没测到。");
        println!("   输入峰值 {peak:.4} ／ 本底 {floor:.4} ／ 阈值 {thr:.4}");
        if peak < 0.005 {
            println!("   → 输入几乎没有信号。检查：麦克风是否静音、输入设备是否选对（--input）");
        } else if peak < thr {
            println!("   → 有信号但没过阈值。检查：扬声器音量是否太低、耳机是否贴紧麦克风");
        } else {
            println!("   → 电平够但没触发，可能是脉冲被设备的降噪/回声消除吃掉了");
        }
        println!();
        return Ok(());
    }

    // 回填到指标，供报告段使用
    let stats = LatencyStats::from_micros(samples);
    engine
        .metrics
        .measured_rt_us
        .store((stats.median_ms * 1000.0) as u64, std::sync::atomic::Ordering::Relaxed);
    println!("测得样本    {} / {rounds}", stats.samples);
    println!("最小值      {:.2} ms", stats.min_ms);
    println!("中位数      {:.2} ms  ← 以这个为准", stats.median_ms);
    println!("P90         {:.2} ms", stats.p90_ms);
    println!("最大值      {:.2} ms", stats.max_ms);
    println!("离散度      {:.2} ms", stats.spread_ms);
    println!("\n判定：{}", thresholds::verdict(stats.median_ms));
    if stats.spread_ms > 5.0 {
        println!("⚠️  离散度偏大，说明系统调度不稳定，本身就值得追查。");
    }
    println!();
    Ok(())
}

/// 连续运行并观察实时健康度。
fn run_soak(engine: &AudioEngine, duration: f32) -> Result<()> {
    // 先空转一段：等启动缓冲、实时提权、缓存预热都过去。
    // 不这样做的话，启动瞬间的一次性开销会被算进稳态指标 ——
    // 尤其是漂移统计，会把启动收敛误报成时钟失配。
    // 至少要盖过水位平滑的时间常数（约 2.5s）的三倍，
    // 否则 EMA 自身还没收敛，统计到的"漂移"里混着它的建立过程。
    const SETTLE_S: f32 = 8.0;
    println!("稳定中（{SETTLE_S:.0}s）…");
    thread::sleep(Duration::from_secs_f32(SETTLE_S));

    println!("── 连续压测 {duration:.0} 秒 ──");
    println!("（对着麦克风唱几句，让 DSP 走到浊音分支）\n");
    engine.metrics.reset();

    let start = Instant::now();
    let mut next_tick = Duration::from_secs(1);
    while start.elapsed().as_secs_f32() < duration {
        thread::sleep(Duration::from_millis(100));
        if start.elapsed() >= next_tick {
            next_tick += Duration::from_secs(5);
            let m = engine.metrics.snapshot();
            let note = if m.voiced {
                format!(
                    "{:.1} Hz  {:+.0} cents",
                    m.f0_hz, m.cents_off
                )
            } else {
                "—".to_string()
            };
            println!(
                "  {:>4.0}s  xrun {:<4} 水位 {:<5} 跨度 {:<5} 回调峰值 {:>5}µs  {}",
                start.elapsed().as_secs_f32(),
                m.xruns,
                m.ring_fill,
                m.ring_fill_span(),
                m.callback_max_us,
                note
            );
        }
    }
    println!();
    Ok(())
}

fn print_report(engine: &AudioEngine, args: &Args) {
    let m = engine.metrics.snapshot();
    let info = engine.info();
    let sr = info.sample_rate as f32;
    let budget_us = m.budget_us.max(1) as f32;

    println!("═══ 报告 ═══\n");
    println!("【吞吐】");
    println!(
        "  输入 {} 帧 / {} 次回调（块 {}）",
        m.input_frames, m.input_callbacks, m.actual_input_block
    );
    println!(
        "  输出 {} 帧 / {} 次回调（块 {}）",
        m.output_frames, m.output_callbacks, m.actual_output_block
    );

    println!("\n【实时健康度】");
    println!("  xrun（输出欠载）      {}", m.xruns);
    println!("  overflow（输入溢出）  {}", m.overflows);
    println!("  DSP 内部欠载          {}", m.dsp_underruns);
    let rt = if m.rt_failures > 0 {
        format!("⚠️ {} 成功 / {} 失败 —— 会导致偶发爆音", m.rt_promotions, m.rt_failures)
    } else if m.rt_promotions >= 2 {
        format!("✅ {} 个音频线程已提权", m.rt_promotions)
    } else {
        format!("{} 个（预期 2 个：采集 + 渲染）", m.rt_promotions)
    };
    println!("  实时优先级            {rt}");
    println!(
        "  回调耗时 平均/峰值    {} / {} µs   预算 {:.0} µs",
        m.callback_avg_us, m.callback_max_us, budget_us
    );
    println!(
        "  CPU 余量              平均占 {:.0}%，峰值占 {:.0}%",
        m.callback_avg_us as f32 / budget_us * 100.0,
        m.callback_max_us as f32 / budget_us * 100.0
    );
    println!("  超过半预算的回调      {} 次", m.callback_over_half_budget);

    // 线程间隔：区分「我们算得慢」和「我们被调度器饿着了」。
    // 回调耗时正常但仍有 xrun 时，答案一定在这里。
    let nominal_cap = if m.actual_input_block > 0 {
        m.actual_input_block as f32 / sr * 1e6
    } else {
        0.0
    };
    let nominal_rnd = if m.actual_output_block > 0 {
        m.actual_output_block as f32 / sr * 1e6
    } else {
        0.0
    };
    println!(
        "  采集间隔 最大/正常    {} / {:.0} µs   停顿 {} 次",
        m.capture_gap_max_us, nominal_cap, m.capture_stalls
    );
    println!(
        "  渲染间隔 最大/正常    {} / {:.0} µs   停顿 {} 次",
        m.render_gap_max_us, nominal_rnd, m.render_stalls
    );
    if m.xruns > 0 {
        let blame = if m.capture_stalls > m.render_stalls {
            "采集线程被饿着（间隔远超正常值）"
        } else if m.render_stalls > 0 {
            "渲染线程被饿着"
        } else {
            "两侧间隔都正常 —— xrun 另有原因，查环形缓冲水位与漂移补偿"
        };
        println!("  ↑ xrun 归因：{blame}");
    }

    // 输入噪声本底。
    //
    // 放进常规报告是有目的的：WASAPI 独占模式会绕过 Windows 的音频引擎，
    // 系统与厂商的 APO 降噪（例如「英特尔智音技术」）在独占下**不生效**。
    // 也就是说我们为了压延迟，可能顺手关掉了用户本来有的降噪。
    //
    // 这个数字让那件事可测：同一台机器上跑
    //     wego-bench soak --backend wasapi --mute
    //     wego-bench soak --backend cpal   --mute
    // 两轮的本底一比就知道差多少 dB。
    println!("\n【输入噪声】");
    let floor = m.noise_floor;
    println!(
        "  本底估计      {:.5}（{:.1} dBFS）",
        floor,
        voice_core::to_dbfs(floor)
    );
    println!(
        "  噪声门余量    {:.0} dB → 门限 {:.1} dBFS",
        args.noise_gate_db.unwrap_or(12.0),
        voice_core::to_dbfs(floor) + args.noise_gate_db.unwrap_or(12.0)
    );
    if floor > 0.02 {
        println!("  ⚠️ 本底过高，噪声门已自动停用（估出来的不像是噪声）");
    } else if floor > 0.005 {
        println!("  ⚠️ 环境偏吵，弱起音可能被门挡掉；考虑换设备或降低余量");
    }

    println!("\n【时钟漂移】");
    println!(
        "  瞬时水位 {} （{}~{}，跨度 {} ≈ 相位抖动，非漂移）",
        m.ring_fill,
        m.ring_fill_min,
        m.ring_fill_max,
        m.ring_fill_span()
    );
    println!("  平滑水位 {}", m.smoothed_fill);
    println!("  补偿 丢帧 {} / 插帧 {} / 净 {:+}", m.drift_drops, m.drift_inserts, m.net_drift);
    let ppm = m.drift_ppm(sr);
    println!("  实测时钟失配 {ppm:+.1} ppm");
    println!(
        "  若不补偿：10 分钟累积 {:.1} ms",
        m.uncompensated_drift_ms(sr, 10.0).abs()
    );

    println!("\n【延迟】");
    let theo = info.theoretical_latency_ms();
    let io_ms = theo - info.algorithmic_ms;
    println!(
        "  设备 I/O 缓冲 {io_ms:.2} ms  ← 输入 {} 帧 + 环形 ×{:.1} + 输出 {} 帧",
        info.input_block_frames, info.target_fill_blocks, info.output_block_frames
    );
    println!("  DSP 算法延迟  {:.2} ms", info.algorithmic_ms);
    println!("  理论下界      {theo:.2} ms");
    if m.measured_rt_us > 0 {
        println!("  实测往返      {:.2} ms", m.measured_rt_us as f32 / 1000.0);
    } else {
        println!("  实测往返      未测（加 --latency）");
    }

    // ---- 判定 ----
    println!("\n═══ Phase 0 判定 ═══\n");
    let mut pass = true;

    // ⚠️ 吞吐量必须**第一个**查。
    //
    // 踩过的坑：采集线程读成 0 帧、整整 10 分钟一个样本都没进来，
    // 而所有健康度指标（xrun / 漂移 / CPU）全是 0，于是报告显示"本轮通过"。
    // 死掉的引擎在这些指标上看起来比健康的还完美 ——
    // 必须先证明音频真的流过，其余指标才有意义。
    let expected = (sr * args.duration * 0.5) as u64; // 一半时长作为下限，留足余量
    let flowed = m.input_frames > expected && m.output_frames > expected;
    println!(
        "  {} 吞吐量：输入 {} 帧 / 输出 {} 帧（期望各 > {}）",
        mark(flowed),
        m.input_frames,
        m.output_frames,
        expected
    );
    if !flowed {
        println!("      ↑ 音频没有真正流过，下面的指标全部无效。");
        if m.input_frames <= expected {
            println!("        采集侧为 0 —— 查采集线程是否初始化失败或读取方式不对");
        }
        if m.output_frames <= expected {
            println!("        渲染侧为 0 —— 查渲染线程是否初始化失败");
        }
    }
    pass &= flowed;

    let health_ok = m.is_healthy();
    println!(
        "  {} 实时健康度：xrun {} / overflow {} / DSP 欠载 {}",
        mark(health_ok),
        m.xruns,
        m.overflows,
        m.dsp_underruns
    );
    pass &= health_ok;

    // 漂移判据看的是"补偿之后还剩多少残余偏差"，而不是失配率本身。
    // 失配是硬件事实，无法消除；能不能补偿掉才是我们要回答的问题。
    let residual_ms = (m.smoothed_fill as f32 - m.actual_output_block as f32 * 1.5).abs()
        / sr
        * 1000.0;
    let drift_ok = residual_ms <= thresholds::DRIFT_NO_GO_MS;
    println!(
        "  {} 漂移补偿后残余：{residual_ms:.2} ms（上限 {:.0} ms），失配 {:+.1} ppm",
        mark(drift_ok),
        thresholds::DRIFT_NO_GO_MS,
        m.drift_ppm(sr)
    );
    pass &= drift_ok;
    if args.duration < 600.0 {
        println!("      ↑ 仅 {:.0} 秒。漂移结论必须跑满 10 分钟：--duration 600", args.duration);
    }

    let lat = if m.measured_rt_us > 0 {
        m.measured_rt_us as f32 / 1000.0
    } else {
        theo
    };
    let lat_ok = lat <= thresholds::LATENCY_NO_GO_MS;
    println!(
        "  {} 延迟：{lat:.2} ms —— {}",
        mark(lat_ok),
        thresholds::verdict(lat)
    );
    pass &= lat_ok;
    if m.measured_rt_us == 0 {
        println!("      ↑ 用的是理论下界。真实数字必须实测：--latency");
    }

    println!(
        "\n  {}",
        if pass {
            "本轮通过。仍需补齐：10 分钟连测 + 脉冲实测 + 三档协议 + 音痴盲测。"
        } else {
            "本轮未通过。先看上面哪一项亮红。"
        }
    );
}

fn mark(ok: bool) -> &'static str {
    if ok {
        "✅"
    } else {
        "❌"
    }
}

/// 扫描缓冲大小，找出不产生 xrun 的最小值。
///
/// 这是 Phase 0 最有价值的一次自动化实验：缓冲大小直接决定延迟预算里
/// 最大的一块，而它的下限完全取决于机器，只能实测。
fn buffer_sweep(args: &Args) -> Result<()> {
    println!("── 缓冲大小扫描 ──");
    println!("对每个缓冲大小跑 {:.0} 秒，找不产生 xrun 的最小值。\n", args.duration);
    println!("⚠️  请戴上耳机。\n");
    println!(
        "  {:<8} {:<8} {:<12} {:<8} {:<12} {}",
        "请求", "实际", "理论延迟", "xrun", "回调均/峰", "结论"
    );
    println!("  {}", "─".repeat(70));

    let mut best: Option<u32> = None;
    let mut actual_sizes = Vec::new();

    for &frames in &[64u32, 96, 128, 192, 256, 384, 512] {
        let cfg = EngineConfig {
            buffer_frames: frames,
            sample_rate: args.sample_rate,
            target_fill_blocks: args.target_fill,
            input_device: args.input.clone(),
            output_device: args.output.clone(),
            realtime_priority: args.realtime,
            f0_floor: args.f0_floor,
            backend: args.backend,
        };
        let mut engine = match AudioEngine::start(&cfg) {
            Ok(e) => e,
            Err(e) => {
                println!("  {frames:<8} 启动失败：{e}");
                continue;
            }
        };
        engine
            .params
            .monitor_muted
            .store(args.muted, std::sync::atomic::Ordering::Relaxed);
        // 等启动缓冲期过去再重置，否则启动瞬间的 xrun 会污染结论
        thread::sleep(Duration::from_millis(600));
        engine.metrics.reset();
        thread::sleep(Duration::from_secs_f32(args.duration.min(15.0)));

        engine.refresh();
        let m = engine.metrics.snapshot();
        let ok = m.is_healthy();
        let actual = engine.actual_block_frames();
        actual_sizes.push(actual);
        if ok && best.is_none() {
            best = Some(frames);
        }
        println!(
            "  {:<8} {:<8} {:<12} {:<8} {:<12} {}",
            frames,
            actual,
            format!("{:.2} ms", engine.theoretical_latency_ms()),
            m.xruns,
            format!("{}/{} µs", m.callback_avg_us, m.callback_max_us),
            if ok { "✅ 稳定" } else { "❌ 有欠载" }
        );
        drop(engine);
        thread::sleep(Duration::from_millis(300));
    }

    println!();

    // 如果实际块大小根本不随请求变化，扫描本身就是无意义的 ——
    // 必须说出来，否则会误以为"已经找到最优值"。
    let unique: std::collections::BTreeSet<_> = actual_sizes.iter().copied().collect();
    if unique.len() <= 1 {
        let n = unique.into_iter().next().unwrap_or(0);
        println!("  ⚠️  实际块大小恒为 {n} 帧，完全不随请求变化。");
        println!("      本次扫描无效 —— 当前音频后端不接受缓冲大小设置。");
        println!("      这本身就是一条 Phase 0 结论：延迟下界由后端锁死，");
        println!("      要突破得换 WASAPI 独占模式 / IAudioClient3 / miniaudio。");
    } else {
        match best {
            Some(f) => println!("  推荐缓冲大小：{f} 帧（最小的无 xrun 值）"),
            None => println!("  ❌ 所有缓冲大小都有欠载。先查实时优先级是否提权成功。"),
        }
    }
    Ok(())
}
