// Release 构建不弹控制台窗口 —— 但 --bench 模式需要控制台输出，
// 所以只在没有 --bench 时隐藏（见下方 main）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::time::{Duration, Instant};

use clap::Parser;
use voice_audio::{thresholds, AudioEngine, EngineConfig};

#[derive(Parser, Debug)]
#[command(
    name = "wego-voice",
    about = "桌面实时修音与音色转换工具",
    long_about = "不带参数 = 打开诊断页 GUI。\n\n\
                  另有三种无界面模式，都跑在同一个可执行文件里：\n\
                  --bench     压测引擎（跳过 WebView，可与 wego-bench 做同源对比）\n\
                  --selftest  自检：走一遍 UI 用的那条路并断言\n\
                  --autostart 开窗即启动引擎",
    version
)]
struct Cli {
    /// 压测引擎，不创建 WebView
    ///
    /// 与独立的 `wego-bench` 跑同一套引擎代码 ——
    /// 两者数字相减即 Tauri 外壳的净开销，属于同源对比。
    #[arg(long, conflicts_with_all = ["selftest", "autostart"])]
    bench: bool,

    /// 自检：走一遍 UI 实际用的那条路（start → tick → stop）并断言
    ///
    /// UI 显示得对不对，取决于这条路给出的数据对不对。
    /// 用合成鼠标点击验证既脆又慢；这个模式几秒钟跑完，
    /// 还能在没有图形界面的环境里跑。
    #[arg(long, conflicts_with_all = ["bench", "autostart"])]
    selftest: bool,

    /// 开窗即启动引擎（自动静音耳返，避免还没戴耳机就出声）
    #[arg(long)]
    autostart: bool,

    /// 压测时长（秒），仅 --bench 有效
    #[arg(short, long, default_value_t = 30.0)]
    duration: f32,

    /// PSOLA 基频下限（Hz），决定 DSP 算法延迟
    #[arg(long, default_value_t = 130.0)]
    f0_floor: f32,

    /// 静音耳返（避免啸叫）
    #[arg(long)]
    mute: bool,
}

fn main() {
    // GUI 模式下（windows_subsystem="windows"）没有控制台，日志无处可去；
    // 但 headless 模式必须能看到警告。统一初始化，代价可忽略。
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

    if cli.bench {
        if let Err(e) = run_bench(&cli) {
            log::error!("压测失败：{e}");
            std::process::exit(1);
        }
        return;
    }

    if cli.selftest {
        if let Err(e) = run_selftest(&cli) {
            log::error!("自检失败：{e}");
            std::process::exit(1);
        }
        return;
    }

    wego_voice_lib::run(cli.autostart, cli.f0_floor);
}

fn run_selftest(cli: &Cli) -> anyhow::Result<()> {
    use wego_voice_lib::state::AppState;

    println!("═══ 自检：AppState 全路径（不创建 WebView）═══\n");

    let state = AppState::default();

    // 1. 设备枚举 —— UI 首屏就靠它填下拉框
    let devices = voice_audio::list_devices()?;
    println!("设备枚举    输入 {} 个 / 输出 {} 个", devices.inputs.len(), devices.outputs.len());
    anyhow::ensure!(!devices.inputs.is_empty(), "没有枚举到输入设备");
    anyhow::ensure!(!devices.outputs.is_empty(), "没有枚举到输出设备");

    // 2. 未启动时的 tick 必须是安全的空值，不能 panic 或给出假数据
    let idle = state.tick();
    anyhow::ensure!(!idle.running, "未启动时 running 应为 false");
    anyhow::ensure!(idle.latency_ms == 0.0, "未启动时不该报延迟");
    println!("空闲 tick    running=false ✓");

    // 3. 启动
    let cfg = EngineConfig {
        f0_floor: cli.f0_floor,
        ..Default::default()
    };
    let info = state.start(cfg)?;
    println!(
        "启动         {} / 输入 {} 帧 / 输出 {} 帧 / {:.2} ms",
        info.backend,
        info.input_block_frames,
        info.output_block_frames,
        info.theoretical_latency_ms()
    );
    if let Some(p) = state.params() {
        p.monitor_muted
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // 4. 跑几秒，确认 tick 里的数字真的在动
    std::thread::sleep(Duration::from_secs(3));
    let t = state.tick();
    anyhow::ensure!(t.running, "启动后 running 应为 true");
    anyhow::ensure!(
        t.metrics.output_frames > 0,
        "输出帧数为 0 —— 音频没有真正流过"
    );
    anyhow::ensure!(
        t.metrics.input_frames > 0,
        "输入帧数为 0 —— 采集侧没跑起来"
    );
    anyhow::ensure!(t.latency_ms > 0.0, "延迟应有值");
    anyhow::ensure!(!t.latency_verdict.is_empty(), "延迟判语不应为空");
    anyhow::ensure!(t.target_fill > 0, "目标水位应有值");

    println!(
        "运行 tick    输入 {} 帧 / 输出 {} 帧 / 水位 {} / 目标 {}",
        t.metrics.input_frames, t.metrics.output_frames, t.metrics.ring_fill, t.target_fill
    );
    println!("             延迟 {:.2} ms —— {}", t.latency_ms, t.latency_verdict);
    println!("             xrun {} / 漂移 {:+.1} ppm", t.metrics.xruns, t.drift_ppm);

    // 5. tick 必须能序列化成前端要的形状 —— 字段名拼错在这里就会暴露
    let json = serde_json::to_string(&t)?;
    for field in [
        "running",
        "latencyMs",
        "latencyVerdict",
        "driftPpm",
        "targetFill",
        "metrics",
        "ringFill",
        "callbackAvgUs",
        "captureStalls",
        "rtPromotions",
    ] {
        anyhow::ensure!(json.contains(field), "序列化结果缺少字段 `{field}`");
    }
    println!("序列化       {} 字节，关键字段齐全 ✓", json.len());

    // 6. 停止后必须干净退出
    state.stop();
    let after = state.tick();
    anyhow::ensure!(!after.running, "停止后 running 应为 false");
    println!("停止         running=false ✓");

    println!("\n✅ 自检通过");
    Ok(())
}

fn run_bench(cli: &Cli) -> anyhow::Result<()> {
    let duration = cli.duration;
    let cfg = EngineConfig {
        f0_floor: cli.f0_floor,
        ..Default::default()
    };

    println!("═══ headless 基线（Tauri 可执行文件，未创建 WebView）═══\n");
    if !cli.mute {
        log::warn!("耳返未静音 —— 请戴耳机，否则会形成啸叫回路（加 --mute 可静音）");
    }

    let mut engine = AudioEngine::start(&cfg)?;
    engine
        .params
        .monitor_muted
        .store(cli.mute, std::sync::atomic::Ordering::Relaxed);

    std::thread::sleep(Duration::from_millis(400));
    engine.refresh();
    let info = engine.info().clone();

    println!("后端        {}{}", info.backend, if info.exclusive { "（独占）" } else { "" });
    println!("输入        {} / {}", info.input_device, info.input_format);
    println!("输出        {} / {}", info.output_device, info.output_format);
    println!(
        "块大小      输入 {} 帧 / 输出 {} 帧 @ {} Hz",
        info.input_block_frames, info.output_block_frames, info.sample_rate
    );
    let lat = info.theoretical_latency_ms();
    println!("理论延迟    {lat:.2} ms —— {}\n", thresholds::verdict(lat));

    // 等启动瞬态过去再统计，否则收敛过程会被当成漂移
    println!("稳定中（8s）…");
    std::thread::sleep(Duration::from_secs(8));
    engine.metrics.reset();

    println!("压测 {duration:.0} 秒…\n");
    let start = Instant::now();
    while start.elapsed().as_secs_f32() < duration {
        std::thread::sleep(Duration::from_millis(500));
    }

    let m = engine.metrics.snapshot();
    let sr = info.sample_rate as f32;
    println!("吞吐        输入 {} 帧 / 输出 {} 帧", m.input_frames, m.output_frames);
    println!("xrun        {}", m.xruns);
    println!(
        "回调耗时    平均 {} / 峰值 {} µs（预算 {} µs）",
        m.callback_avg_us, m.callback_max_us, m.budget_us
    );
    println!(
        "线程停顿    采集 {} 次（最大 {} µs）/ 渲染 {} 次（最大 {} µs）",
        m.capture_stalls, m.capture_gap_max_us, m.render_stalls, m.render_gap_max_us
    );
    println!("时钟失配    {:+.1} ppm", m.drift_ppm(sr));

    // 吞吐量必须先查：死掉的引擎在其余指标上看起来比健康的还完美
    let expected = (sr * duration * 0.5) as u64;
    if m.input_frames < expected || m.output_frames < expected {
        println!("\n❌ 音频没有真正流过，以上指标无效。");
        return Ok(());
    }
    println!(
        "\n{}",
        if m.xruns == 0 {
            "✅ 无 xrun"
        } else {
            "❌ 有 xrun —— 看线程停顿那行定位是哪一侧被饿着"
        }
    );
    Ok(())
}
