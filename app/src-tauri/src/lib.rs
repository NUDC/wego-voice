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
pub fn run(autostart: bool, f0_floor: f32) {
    tauri::Builder::default()
        .manage(AppState::default())
        .setup(move |app| {
            state::spawn_pusher(app.handle().clone(), TICK_HZ);
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
