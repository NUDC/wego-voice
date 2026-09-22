//! wego-voice 桌面工具的 Tauri 外壳。
//!
//! # 这一层做什么、不做什么
//!
//! **做**：装配引擎、暴露 command、按固定频率推送指标快照。
//!
//! **不做**：任何音频处理。DSP 在 `voice-core`，I/O 与调度在 `voice-audio`。
//!
//! # 同一个可执行文件，两种模式
//!
//! - 默认：开窗口，跑 UI
//! - `--bench`：**跳过 WebView**，只跑引擎并打印数字
//!
//! 这是刻意的（实施方案定调表 #5b）：两种模式跑的是完全相同的引擎代码，
//! 相减即得 Tauri 外壳的净开销 —— 比拿两个不同程序对比可信得多。

pub mod characters;
pub mod commands;
pub mod state;
pub mod tray;

use state::AppState;

/// 指标推送频率。
///
/// 诊断页 20Hz 足够；将来音高条要 60Hz 时，应当**单开一路**高频事件，
/// 而不是把整个快照都提到 60Hz —— 快照里大部分字段没必要那么快。
const TICK_HZ: u32 = 20;

/// 开窗跑 UI。
///
/// `autostart` 为真时开窗即启动引擎 —— 给两类人用：
/// 想一打开就能唱的用户，以及要做自动化验证的我们。
/// WebView2 运行时缺失时弹个原生对话框，然后退出。
///
/// # 为什么值得为它写这段
///
/// 免安装版是**单个 exe**，没有安装器去引导安装 WebView2。
/// 缺了它，Tauri 建窗口会失败，而这是个 `windows_subsystem = "windows"`
/// 的程序 —— 没有控制台、stderr 进黑洞，用户看到的是
/// **双击之后什么都没发生**。这是最难自查的一类故障。
///
/// Win10 较新版本与 Win11 都自带 WebView2（随 Edge 分发），
/// 所以绝大多数人碰不到；但碰到的那个人，必须知道原因。
///
/// 用 `MessageBoxW` 而不是引入对话框插件：这段代码要在 Tauri 起来**之前**
/// 跑，那时什么插件都还没初始化；而且为一句话拉一个依赖不划算。
#[cfg(windows)]
fn webview2_missing_dialog(err: &str) {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(
            hwnd: *mut core::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            utype: u32,
        ) -> i32;
    }

    let wide = |s: &str| {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };

    let text = format!(
        "缺少 Microsoft Edge WebView2 运行时，界面无法启动。

         请安装「Microsoft Edge WebView2 Runtime」（微软官方免费组件，
         搜索该名称即可下载）后重新打开本程序。

         Windows 11 与较新的 Windows 10 一般自带该组件。

         技术细节：{err}"
    );
    // 0x10 = MB_ICONERROR
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(&text).as_ptr(),
            wide("wego-voice 无法启动").as_ptr(),
            0x10,
        );
    }
}

pub fn run(autostart: bool, f0_floor: f32) {
    // 先探一下 WebView2 在不在。失败就给出人话，而不是静默什么都不发生。
    if let Err(e) = tauri::webview_version() {
        log::error!("WebView2 运行时不可用：{e}");
        #[cfg(windows)]
        webview2_missing_dialog(&e.to_string());
        std::process::exit(1);
    }

    tauri::Builder::default()
        .manage(AppState::default())
        .setup(move |app| {
            let tray = tray::build(app.handle())?;
            state::spawn_pusher(app.handle().clone(), TICK_HZ, Some(tray));
            if autostart {
                let handle = app.handle().clone();
                // 放到后台线程：独占模式协商要几百毫秒，
                // 卡在 setup 里会让窗口迟迟不显示。
                std::thread::spawn(move || {
                    use tauri::Manager;
                    let state = handle.state::<AppState>();
                    let cfg = voice_audio::EngineConfig {
                        f0_floor,
                        ..Default::default()
                    };
                    match state.start(cfg) {
                        Ok(info) => {
                            // 自动启动时默认静音耳返：用户还没戴上耳机就出声会啸叫
                            if let Some(p) = state.params() {
                                p.monitor_muted
                                    .store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                            log::info!("autostart：{} / {:.2} ms", info.backend, info.theoretical_latency_ms());
                        }
                        Err(e) => log::error!("autostart 启动失败：{e}"),
                    }
                });
            }
            Ok(())
        })
        // 关闭 = 收进托盘，**不退出**。退出只能从托盘菜单。
        //
        // ⚠️ 这个设计有个真实风险，实现里必须消化掉：
        // 独占模式下声卡是被我们占着的，而现在窗口可以消失、进程还在跑 ——
        // 用户完全可能处在「电脑没声音，而且不知道是谁干的」这个状态里。
        //
        // 三道防线：
        //   1. 托盘图标按引擎状态变色（灰/蓝/琥珀），提示文字写明声卡被独占。
        //      托盘从"锦上添花"变成了**承重件**。
        //   2. 标题栏关闭按钮的 tooltip 提前说清楚，不等用户点了才知道。
        //
        // 隐藏这件事**全部在 Rust 侧做完**，不绕前端一圈：前端只要有一处
        // 没响应，窗口就关不掉了。而托盘菜单的「退出 wego-voice」同样是
        // 纯 Rust 侧的 —— 整条关闭/退出链路不依赖 WebView 是否健康。
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::devices,
            commands::start,
            commands::stop,
            commands::engine_info,
            commands::tick,
            commands::reset_metrics,
            commands::set_params,
            commands::measure_latency,
            commands::start_recording,
            commands::stop_recording,
            commands::recording_status,
            commands::reveal_recordings,
            commands::list_recordings,
            commands::suggest_character,
            commands::characters_load,
            commands::characters_save,
            commands::characters_builtins,
            commands::apply_character,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 启动失败");
}
