//! `wasapi-probe` —— WASAPI 硬件周期能力探针。
//!
//! # 它回答什么
//!
//! Phase 0 实测发现：cpal（0.15 与 0.18 都一样）在 WASAPI **共享模式**下
//! 恒定拿到 480 帧（10ms）的块，光 I/O 就吃掉 35ms，端到端 55ms，
//! 远超 30ms 的 No-Go 线。
//!
//! 出路有三条（独占模式 / IAudioClient3 低延迟共享 / ASIO），
//! 但每条都要绕过 cpal 自己写后端 —— 那是好几天的工作。
//!
//! **在投入之前，先花一百行问清楚硬件到底支持到什么程度。**
//! 这个探针直接调 Windows 音频 API，报告：
//!
//! - `IAudioClient::GetDevicePeriod` → 默认周期 / **硬件最小周期**
//! - `IAudioClient3::GetSharedModeEnginePeriod` → 低延迟共享模式的
//!   最小 / 最大 / 默认 / 基本周期
//!
//! 有了这些数字就能直接算出每条路线能达到的延迟，据此决定投哪一条 ——
//! 或者确认哪条都不行。
//!
//! ```text
//! cargo run -p voice-audio --release --bin wasapi-probe
//! ```

#[cfg(not(windows))]
fn main() {
    eprintln!("本探针只在 Windows 上有意义。");
}

#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    probe::run()
}

#[cfg(windows)]
mod probe {
    // `Interface` 提供 `cast::<T>()`，用来把 IAudioClient 升级成 IAudioClient3
    use windows::core::{Interface, Result, PCWSTR};
    use windows::Win32::Media::Audio::{
        eCapture, eConsole, eRender, IAudioClient, IAudioClient3, IMMDevice, IMMDeviceEnumerator,
        MMDeviceEnumerator, AUDCLNT_SHAREMODE_EXCLUSIVE, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
    };
    use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
        COINIT_MULTITHREADED, STGM_READ,
    };
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;

    /// REFERENCE_TIME 的单位是 100 纳秒。
    const REFTIMES_PER_MS: f64 = 10_000.0;

    /// 当前 PSOLA 默认算法延迟（f0_floor = 100Hz）。
    const DSP_MS: f64 = 20.0;
    const NO_GO_MS: f64 = 30.0;

    /// 一个设备在三种路线下的块周期（毫秒）。
    #[derive(Debug, Clone, Copy, Default)]
    struct Caps {
        sample_rate: u32,
        /// 共享模式默认周期 —— cpal 现在拿到的就是它。
        shared_default: f64,
        /// 独占模式的硬件最小周期。
        exclusive_min: f64,
        /// IAudioClient3 低延迟共享模式的最小周期。None = 不支持。
        client3_min: Option<f64>,
        /// 独占模式能否直接用混音格式（f32）。
        /// false 意味着还要加一层整数格式转换。
        exclusive_accepts_mix_format: bool,
    }

    impl Caps {
        /// 三条路线里能达到的最小块周期，以及对应路线名。
        fn best(&self) -> (f64, &'static str) {
            let mut best = (self.shared_default, "共享模式");
            if let Some(c3) = self.client3_min {
                if c3 < best.0 {
                    best = (c3, "IAudioClient3");
                }
            }
            if self.exclusive_min < best.0 {
                best = (self.exclusive_min, "独占模式");
            }
            best
        }
    }

    /// 端到端 I/O 延迟。
    ///
    /// 构成：输入块 + 环形缓冲目标水位 + 输出块。
    /// 环形缓冲要吸收两侧的相位抖动，按较大的那个块的 1.5 倍取。
    ///
    /// ⚠️ 不能像早先那样"拿单一块大小 × 3.5" —— 输入和输出的块周期
    /// 可能差好几倍（本机输入能到 2ms，输出卡在 10ms），用单一值会算错。
    fn io_latency_ms(in_block: f64, out_block: f64) -> f64 {
        in_block + in_block.max(out_block) * 1.5 + out_block
    }

    pub fn run() -> Result<()> {
        unsafe {
            // 忽略"已初始化"的返回码：进程里可能已有人初始化过 COM
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let result = probe_all();
        unsafe { CoUninitialize() };
        result
    }

    fn probe_all() -> Result<()> {
        println!("═══ WASAPI 硬件周期探针 ═══\n");
        println!("目的：判断绕过 cpal 自写低延迟后端是否值得投入。");
        println!("参照：当前 cpal 共享模式实测 480 帧 / 10ms，端到端 55ms（No-Go 线 30ms）\n");

        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

        let probe_default = |label: &str, flow| -> Caps {
            println!("─── {label} ───");
            let caps = match unsafe { enumerator.GetDefaultAudioEndpoint(flow, eConsole) } {
                Ok(dev) => probe_device(&dev).unwrap_or_else(|e| {
                    println!("  探测失败：{e}");
                    Caps::default()
                }),
                Err(e) => {
                    println!("  取默认设备失败：{e}");
                    Caps::default()
                }
            };
            println!();
            caps
        };

        let in_caps = probe_default("输入（采集）", eCapture);
        let out_caps = probe_default("输出（渲染）", eRender);

        combined_analysis(&in_caps, &out_caps);

        // 列出所有活动设备，便于发现"某个设备周期更低"的情况
        println!("─── 全部活动设备的最小周期 ───");
        for (label, flow) in [("输出", eRender), ("输入", eCapture)] {
            let Ok(coll) = (unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) })
            else {
                continue;
            };
            let count = unsafe { coll.GetCount() }.unwrap_or(0);
            for i in 0..count {
                let Ok(dev) = (unsafe { coll.Item(i) }) else { continue };
                let name = friendly_name(&dev).unwrap_or_else(|| "<未知>".into());
                match min_period_ms(&dev) {
                    Some((def, min, sr)) => println!(
                        "  [{label}] {name}\n          默认 {def:.2} ms / 最小 {min:.2} ms @ {sr} Hz",
                    ),
                    None => println!("  [{label}] {name}\n          <无法查询>"),
                }
            }
        }

        println!("\n{}", "═".repeat(60));
        Ok(())
    }

    fn probe_device(dev: &IMMDevice) -> Result<Caps> {
        let name = friendly_name(dev).unwrap_or_else(|| "<未知>".into());
        println!("  设备：{name}");

        let client: IAudioClient = unsafe { dev.Activate(CLSCTX_ALL, None)? };

        // ---- 混音格式（决定采样率）----
        let fmt_ptr = unsafe { client.GetMixFormat()? };
        let (sample_rate, channels) = unsafe {
            let f: &WAVEFORMATEX = &*fmt_ptr;
            (f.nSamplesPerSec, f.nChannels)
        };
        println!("  混音格式：{sample_rate} Hz，{channels} 声道");

        // ---- 经典 API：默认周期与硬件最小周期 ----
        let mut default_rt = 0i64;
        let mut min_rt = 0i64;
        unsafe { client.GetDevicePeriod(Some(&mut default_rt), Some(&mut min_rt))? };
        let default_ms = default_rt as f64 / REFTIMES_PER_MS;
        let min_ms = min_rt as f64 / REFTIMES_PER_MS;

        println!("\n  【IAudioClient::GetDevicePeriod】");
        println!(
            "    默认周期（共享模式）  {:.2} ms（{} 帧）  ← cpal 现在拿到的就是它",
            default_ms,
            frames(default_ms, sample_rate)
        );
        println!(
            "    最小周期（独占模式）  {:.2} ms（{} 帧）",
            min_ms,
            frames(min_ms, sample_rate)
        );

        // ---- IAudioClient3：低延迟共享模式 ----
        println!("\n  【IAudioClient3::GetSharedModeEnginePeriod】");
        let mut client3_min = None;
        match client.cast::<IAudioClient3>() {
            Ok(c3) => {
                let mut default_frames = 0u32;
                let mut fundamental = 0u32;
                let mut min_frames = 0u32;
                let mut max_frames = 0u32;
                let r = unsafe {
                    c3.GetSharedModeEnginePeriod(
                        fmt_ptr,
                        &mut default_frames,
                        &mut fundamental,
                        &mut min_frames,
                        &mut max_frames,
                    )
                };
                match r {
                    Ok(()) => {
                        let to_ms = |f: u32| f as f64 * 1000.0 / sample_rate as f64;
                        println!("    默认  {default_frames} 帧（{:.2} ms）", to_ms(default_frames));
                        println!(
                            "    最小  {min_frames} 帧（{:.2} ms）  ← 低延迟共享的下限",
                            to_ms(min_frames)
                        );
                        println!("    最大  {max_frames} 帧（{:.2} ms）", to_ms(max_frames));
                        println!("    基本单位 {fundamental} 帧（{:.2} ms）", to_ms(fundamental));
                        if min_frames >= default_frames {
                            println!("    ⚠️ 最小 = 默认：本设备驱动**不支持**低延迟共享，此路不通");
                        }
                        client3_min = Some(to_ms(min_frames));
                    }
                    Err(e) => println!("    查询失败：{e}"),
                }
            }
            Err(e) => println!("    本机不支持 IAudioClient3（需 Win10 1703+）：{e}"),
        }

        // ---- 独占模式能不能直接吃 f32 混音格式 ----
        //
        // 这决定要不要额外加一层格式转换：独占模式绕过系统混音器，
        // 设备往往只认自己的原生整数格式（16/24 bit），不认 f32。
        let exclusive_accepts_mix_format = unsafe {
            client
                .IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, fmt_ptr, None)
                .is_ok()
        };
        println!("\n  【独占模式格式兼容】");
        if exclusive_accepts_mix_format {
            println!("    ✅ 直接接受混音格式（f32），无需额外转换");
        } else {
            println!("    ⚠️ 不接受 f32 混音格式 —— 独占模式下需加一层整数格式转换");
        }

        unsafe { CoTaskMemFree(Some(fmt_ptr as *const _)) };
        Ok(Caps {
            sample_rate,
            shared_default: default_ms,
            exclusive_min: min_ms,
            client3_min,
            exclusive_accepts_mix_format,
        })
    }

    /// 把输入与输出的能力合起来算真实可达延迟。
    ///
    /// 这一段才是探针的结论所在 —— 单看某一侧会得出错误判断。
    fn combined_analysis(inp: &Caps, out: &Caps) {
        println!("═══ 组合路线分析 ═══\n");
        if inp.sample_rate == 0 || out.sample_rate == 0 {
            println!("设备信息不全，跳过。");
            return;
        }

        println!("公式：I/O = 输入块 + max(输入,输出)×1.5（环形缓冲） + 输出块\n");
        println!(
            "  {:<34} {:>9} {:>9} {:>9}",
            "路线（输入 / 输出）", "I/O", "含DSP", "判定"
        );
        println!("  {}", "─".repeat(64));

        let (best_in, best_in_name) = inp.best();
        let (best_out, best_out_name) = out.best();

        let routes: Vec<(String, f64, f64)> = vec![
            (
                "① 现状：共享 / 共享（cpal）".to_string(),
                inp.shared_default,
                out.shared_default,
            ),
            (
                format!("② 输入{best_in_name} / 输出共享"),
                best_in,
                out.shared_default,
            ),
            (
                format!("③ 输入共享 / 输出{best_out_name}"),
                inp.shared_default,
                best_out,
            ),
            (
                format!("④ 双侧最优：{best_in_name} / {best_out_name}"),
                best_in,
                best_out,
            ),
        ];

        for (name, i, o) in &routes {
            let io = io_latency_ms(*i, *o);
            let total = io + DSP_MS;
            let mark = if total <= NO_GO_MS { "✅" } else { "❌" };
            println!("  {name:<34} {io:>7.2}ms {total:>7.2}ms {mark:>7}");
        }

        // ---- DSP 这一侧还能省多少 ----
        let best_io = io_latency_ms(best_in, best_out);
        println!("\n  最优 I/O = {best_io:.2} ms，剩给 DSP 的预算 = {:.2} ms", NO_GO_MS - best_io);
        println!("\n  {:<18} {:>10} {:>12} {:>8}", "--f0-floor", "DSP 延迟", "端到端", "判定");
        println!("  {}", "─".repeat(52));
        for floor in [100.0f64, 115.0, 130.0, 160.0] {
            // PSOLA 固定延迟 = 2 × 周期 = 2000/f0 毫秒
            let dsp = 2000.0 / floor;
            let total = best_io + dsp;
            let mark = if total <= NO_GO_MS { "✅" } else { "❌" };
            println!("  {floor:<18.0} {dsp:>8.2}ms {total:>10.2}ms {mark:>8}");
        }

        // ---- 结论 ----
        println!("\n─── 结论 ───\n");
        let best_total = best_io + DSP_MS;
        if best_total <= NO_GO_MS {
            println!("  ✅ 有可行路线：输入走{best_in_name}，输出走{best_out_name}。");
        } else if best_io + 2000.0 / 160.0 <= NO_GO_MS {
            println!("  🟡 靠「最优 I/O + 收紧 DSP」可以进线，但余量很薄。");
            println!("     需要同时做两件事：换低延迟后端 + 调高 f0_floor。");
        } else {
            println!("  ❌ 即便双侧取最优、DSP 压到极限，仍然进不了 {NO_GO_MS:.0}ms。");
            println!("     本机现有设备无解，需外置声卡或 ASIO。");
        }

        if !out.exclusive_accepts_mix_format && best_out_name == "独占模式" {
            println!("\n  ⚠️ 输出侧走独占模式还需加一层整数格式转换（设备不吃 f32）。");
        }
        if out.client3_min.map(|c| c >= out.shared_default).unwrap_or(true) {
            println!("\n  ⚠️ 输出设备不支持 IAudioClient3 低延迟共享 —— 输出侧只能走独占模式，");
            println!("     代价是运行期间独占声卡、系统其他声音静音。");
            println!("     （工具定位下这是可接受的）");
        }
    }

    fn frames(ms: f64, sample_rate: u32) -> u32 {
        (ms * sample_rate as f64 / 1000.0).round() as u32
    }

    /// 只取"默认周期 / 最小周期 / 采样率"，用于批量列设备。
    fn min_period_ms(dev: &IMMDevice) -> Option<(f64, f64, u32)> {
        let client: IAudioClient = unsafe { dev.Activate(CLSCTX_ALL, None) }.ok()?;
        let fmt_ptr = unsafe { client.GetMixFormat() }.ok()?;
        let sr = unsafe { (*fmt_ptr).nSamplesPerSec };
        let mut default_rt = 0i64;
        let mut min_rt = 0i64;
        let ok = unsafe { client.GetDevicePeriod(Some(&mut default_rt), Some(&mut min_rt)) }.is_ok();
        unsafe { CoTaskMemFree(Some(fmt_ptr as *const _)) };
        if !ok {
            return None;
        }
        Some((
            default_rt as f64 / REFTIMES_PER_MS,
            min_rt as f64 / REFTIMES_PER_MS,
            sr,
        ))
    }

    fn friendly_name(dev: &IMMDevice) -> Option<String> {
        unsafe {
            let store = dev.OpenPropertyStore(STGM_READ).ok()?;
            let prop = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
            let pwstr = PropVariantToStringAlloc(&prop).ok()?;
            let s = pwstr_to_string(pwstr);
            CoTaskMemFree(Some(pwstr.0 as *const _));
            Some(s)
        }
    }

    unsafe fn pwstr_to_string(p: windows::core::PWSTR) -> String {
        let ptr = PCWSTR(p.0);
        ptr.to_string().unwrap_or_default()
    }
}
