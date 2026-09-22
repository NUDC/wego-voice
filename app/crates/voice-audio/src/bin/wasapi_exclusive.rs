//! `wasapi-exclusive` —— 独占模式可行性验证。
//!
//! # 和 `wasapi-probe` 的分工
//!
//! `wasapi-probe` **查**硬件声称的能力（GetDevicePeriod 等），算出理论延迟。
//! 本程序**真的去打开**独占流，验证那些数字能不能兑现 ——
//! 声称支持和实际能开成，在 Windows 音频上是两回事。
//!
//! 要回答的问题：
//!
//! 1. 独占模式能不能开成？（驱动、设备占用、权限都可能挡住）
//! 2. 开成之后实际拿到的周期是多少帧？
//! 3. 采样格式要怎么退让？（探针显示两侧都不吃 f32）
//! 4. 最终端到端延迟是多少？
//!
//! ```text
//! cargo run -p voice-audio --release --bin wasapi-exclusive
//! ```
//!
//! ⚠️ 独占模式会独占设备。跑之前关掉其他在放音的程序，
//! 否则会看到 `AUDCLNT_E_DEVICE_IN_USE`。

#[cfg(not(windows))]
fn main() {
    eprintln!("本程序只在 Windows 上有意义。");
}

#[cfg(windows)]
fn main() {
    windows_impl::run();
}

#[cfg(windows)]
mod windows_impl {
    use wasapi::{
        calculate_period_100ns, initialize_mta, Direction, SampleType, StreamMode, WaveFormat,
    };

    /// PSOLA 默认算法延迟（f0_floor = 100Hz）。
    const DSP_MS: f64 = 20.0;
    const NO_GO_MS: f64 = 30.0;

    /// 一侧设备的验证结果。
    struct Outcome {
        label: &'static str,
        name: String,
        /// 独占模式实际拿到的周期（帧）。None = 开不成。
        frames: Option<u32>,
        sample_rate: usize,
        /// 最终协商下来的格式描述。
        format: String,
        /// 共享模式作为对照。
        shared_frames: u32,
    }

    pub fn run() {
        let hr = initialize_mta();
        if hr.is_err() {
            eprintln!("COM 初始化失败：{hr:?}");
            return;
        }

        println!("═══ WASAPI 独占模式实开验证 ═══\n");
        println!("目的：验证 wasapi-probe 算出的 9.5ms I/O 能否真正兑现。");
        println!("⚠️ 独占模式会独占设备；若报 DEVICE_IN_USE，先关掉其他在放音的程序。\n");

        let inp = try_direction("输入（采集）", Direction::Capture);
        println!();
        let out = try_direction("输出（渲染）", Direction::Render);
        println!();

        summarize(&inp, &out);
    }

    fn try_direction(label: &'static str, direction: Direction) -> Outcome {
        println!("─── {label} ───");

        let mut outcome = Outcome {
            label,
            name: "<未知>".into(),
            frames: None,
            sample_rate: 48_000,
            format: "—".into(),
            shared_frames: 0,
        };

        let enumerator = match wasapi::DeviceEnumerator::new() {
            Ok(e) => e,
            Err(e) => {
                println!("  枚举器创建失败：{e}");
                return outcome;
            }
        };
        let device = match enumerator.get_default_device(&direction) {
            Ok(d) => d,
            Err(e) => {
                println!("  取默认设备失败：{e}");
                return outcome;
            }
        };
        outcome.name = device.get_friendlyname().unwrap_or_else(|_| "<未知>".into());
        println!("  设备：{}", outcome.name);

        let mut client = match device.get_iaudioclient() {
            Ok(c) => c,
            Err(e) => {
                println!("  取 AudioClient 失败：{e}");
                return outcome;
            }
        };

        // ---- 基准：共享模式的周期 ----
        let (default_hns, min_hns) = match client.get_device_period() {
            Ok(p) => p,
            Err(e) => {
                println!("  查周期失败：{e}");
                return outcome;
            }
        };
        let mix = match client.get_mixformat() {
            Ok(f) => f,
            Err(e) => {
                println!("  查混音格式失败：{e}");
                return outcome;
            }
        };
        let sr = mix.get_samplespersec() as usize;
        let channels = mix.get_nchannels() as usize;
        outcome.sample_rate = sr;
        let hns_to_frames = |hns: i64| (hns as f64 * sr as f64 / 1e7).round() as u32;
        outcome.shared_frames = hns_to_frames(default_hns);

        println!(
            "  混音格式：{sr} Hz，{channels} 声道；共享周期 {} 帧（{:.2} ms），独占最小 {} 帧（{:.2} ms）",
            outcome.shared_frames,
            default_hns as f64 / 1e4,
            hns_to_frames(min_hns),
            min_hns as f64 / 1e4,
        );

        // ---- 找一个独占模式能接受的格式 ----
        //
        // wasapi-probe 已经确认两侧都不吃 f32 混音格式，
        // 所以这里从 f32 开始逐级退让到整数格式。
        // `is_supported_exclusive_with_quirks` 还会自动试不同的声道掩码 ——
        // 这正是独占模式最容易卡住的地方。
        let candidates = [
            (32, 32, SampleType::Float, "f32"),
            (32, 24, SampleType::Int, "int24-in-32"),
            (24, 24, SampleType::Int, "int24"),
            (16, 16, SampleType::Int, "int16"),
        ];

        let mut chosen: Option<(WaveFormat, &str)> = None;
        println!("\n  【格式协商】");
        for (store, valid, ty, name) in candidates {
            let fmt = WaveFormat::new(store, valid, &ty, sr, channels, None);
            match client.is_supported_exclusive_with_quirks(&fmt) {
                Ok(accepted) => {
                    println!("    ✅ {name} —— 被接受");
                    chosen = Some((accepted, name));
                    break;
                }
                Err(_) => println!("    ❌ {name}"),
            }
        }

        let Some((fmt, fmt_name)) = chosen else {
            println!("\n  ❌ 没有任何候选格式被独占模式接受。");
            return outcome;
        };
        outcome.format = format!("{fmt_name} @ {sr}Hz {channels}ch");

        // ---- 真正去开流 ----
        //
        // 从硬件最小周期开始，失败就逐级放宽。
        // 驱动经常声称支持某个周期，实际 Initialize 却报 UNSUPPORTED。
        println!("\n  【实开独占流】");
        let min_frames = hns_to_frames(min_hns).max(32);
        let candidates_frames: Vec<u32> = [
            min_frames,
            min_frames * 2,
            min_frames * 3,
            outcome.shared_frames,
        ]
        .into_iter()
        .filter(|f| *f > 0)
        .collect();

        for frames in candidates_frames {
            let period = calculate_period_100ns(frames as i64, sr as i64);
            let mode = StreamMode::EventsExclusive { period_hns: period };
            match client.initialize_client(&fmt, &direction, &mode) {
                Ok(()) => {
                    let actual = client.get_buffer_size().unwrap_or(frames);
                    println!(
                        "    ✅ 请求 {frames} 帧（{:.2} ms）开成 —— 实际缓冲 {actual} 帧（{:.2} ms）",
                        frames as f64 * 1000.0 / sr as f64,
                        actual as f64 * 1000.0 / sr as f64
                    );
                    outcome.frames = Some(actual);
                    break;
                }
                Err(e) => {
                    println!(
                        "    ❌ {frames} 帧（{:.2} ms）：{e}",
                        frames as f64 * 1000.0 / sr as f64
                    );
                    // 同一个 client 初始化失败后不保证可复用，重新取一个
                    client = match device.get_iaudioclient() {
                        Ok(c) => c,
                        Err(e) => {
                            println!("    重建 client 失败：{e}");
                            break;
                        }
                    };
                }
            }
        }

        if outcome.frames.is_none() {
            println!("\n  ❌ 独占模式全部尝试失败。");
            // 作为对照，确认共享模式确实能开 —— 排除"设备本身有问题"
            match device.get_iaudioclient() {
                Ok(mut c) => {
                    let mode = StreamMode::EventsShared {
                        autoconvert: true,
                        buffer_duration_hns: default_hns,
                    };
                    match c.initialize_client(&mix, &direction, &mode) {
                        Ok(()) => println!("     （共享模式可以开，说明设备正常，是独占被拒）"),
                        Err(e) => println!("     （共享模式也开不了：{e}）"),
                    }
                }
                Err(e) => println!("     对照测试失败：{e}"),
            }
        }

        outcome
    }

    /// I/O 延迟 = 输入块 + max(输入,输出)×1.5（环形缓冲） + 输出块。
    /// 与 `engine.rs` 和 `wasapi-probe` 保持同一公式。
    fn io_latency_ms(in_block: f64, out_block: f64) -> f64 {
        in_block + in_block.max(out_block) * 1.5 + out_block
    }

    fn summarize(inp: &Outcome, out: &Outcome) {
        println!("═══ 结论 ═══\n");

        let ms = |frames: u32, sr: usize| frames as f64 * 1000.0 / sr as f64;

        println!("  {:<14} {:>12} {:>14} {}", "", "共享（cpal）", "独占（实开）", "格式");
        println!("  {}", "─".repeat(62));
        for o in [inp, out] {
            let excl = match o.frames {
                Some(f) => format!("{} 帧 {:.2}ms", f, ms(f, o.sample_rate)),
                None => "失败".to_string(),
            };
            println!(
                "  {:<14} {:>12} {:>14} {}",
                o.label,
                format!("{} 帧", o.shared_frames),
                excl,
                o.format
            );
        }

        let (Some(in_f), Some(out_f)) = (inp.frames, out.frames) else {
            println!("\n  ❌ 至少一侧独占模式开不成，本路线在本机不可用。");
            println!("     下一步：换 miniaudio 试，或检查是否有程序占着设备。");
            return;
        };

        let in_ms = ms(in_f, inp.sample_rate);
        let out_ms = ms(out_f, out.sample_rate);
        let io = io_latency_ms(in_ms, out_ms);

        println!("\n  实测可达 I/O 延迟：{io:.2} ms（输入 {in_ms:.2} + 环形 {:.2} + 输出 {out_ms:.2}）",
            in_ms.max(out_ms) * 1.5);

        println!("\n  {:<14} {:>10} {:>12} {:>8}", "--f0-floor", "DSP", "端到端", "判定");
        println!("  {}", "─".repeat(48));
        let mut any_pass = false;
        for floor in [100.0f64, 115.0, 130.0, 160.0] {
            let dsp = 2000.0 / floor; // PSOLA 固定延迟 = 2 个基音周期
            let total = io + dsp;
            let pass = total <= NO_GO_MS;
            any_pass |= pass;
            println!(
                "  {floor:<14.0} {dsp:>8.2}ms {total:>10.2}ms {:>8}",
                if pass { "✅" } else { "❌" }
            );
        }

        println!();
        if io + DSP_MS <= NO_GO_MS {
            println!("  ✅ 默认配置（f0_floor=100）即可进线：{:.2} ms", io + DSP_MS);
        } else if any_pass {
            println!("  🟡 需要调高 f0_floor 才能进线 —— 代价是低音区音质。");
        } else {
            println!("  ❌ 即便 DSP 压到极限也进不了 {NO_GO_MS:.0}ms，本机现有设备无解。");
        }

        println!("\n  ⚠️ 这是可达性验证，不是端到端实测。");
        println!("     真实数字仍需把引擎接到这条链路上，再用 --latency 打脉冲测。");
    }
}
