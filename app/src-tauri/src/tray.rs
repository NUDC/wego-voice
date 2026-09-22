//! 系统托盘。
//!
//! # 为什么这个应用**必须**有托盘
//!
//! 不是为了"看起来专业"。低延迟模式会**独占声卡** ——
//! 运行期间系统其他声音全部静音。
//!
//! 窗口一旦被别的程序挡住或最小化，用户就处在这个状态里：
//! 「电脑突然没声音了，而且不知道是谁干的」。任务栏图标在多窗口下很容易被
//! 忽略，托盘却是常驻可见的。
//!
//! 所以托盘图标的首要职责是**如实显示"声卡正被我占着"**，
//! 快捷操作是顺带的。图标颜色因此随引擎状态变化，而不是一个静态 logo。
//!
//! # 关闭 = 收进托盘，退出只能从这里
//!
//! 窗口可以消失而进程继续跑，这让上面那个风险变成了常态 ——
//! 所以**托盘不再是锦上添花，而是承重件**：
//!
//! - 图标颜色是"声卡现在是不是被占着"的唯一持续可见的信号
//! - 菜单里的「退出 wego-voice」是唯一的退出口，而且它是纯 Rust 侧的，
//!   前端出任何问题都不影响它 —— 不存在"窗口关不掉又退不出"的死角
//!
//! 关闭窗口不弹任何提示 —— 提前说明放在标题栏关闭按钮的 tooltip 上。
//!
//! # 图标是画出来的，不是资源文件
//!
//! 需要"停止/运行/告警"三种状态，配图标文件就要多带三份资源、
//! 还要各出 16/32/48 三种尺寸。而这个图标本身极简（几根竖条），
//! 直接生成 RGBA 更省事，也不会出现资源丢失导致托盘没图标的情况。

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use tauri::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Runtime};

use crate::state::AppState;

const REL: Ordering = Ordering::Relaxed;

/// 托盘图标的三种状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    /// 引擎未启动，声卡是自由的。
    Idle = 0,
    /// 正在跑，**声卡被独占**。
    Live = 1,
    /// 在跑但有 xrun —— 用户正在听到爆音，得让他知道。
    Trouble = 2,
}

impl Look {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Look::Live,
            2 => Look::Trouble,
            _ => Look::Idle,
        }
    }

    /// 图标主色（RGB）。与应用内的语义色同源。
    fn rgb(self) -> [u8; 3] {
        match self {
            Look::Idle => [0x65, 0x6c, 0x7e],    // --fg-3 灰：没在工作
            Look::Live => [0x56, 0xcc, 0xf2],    // --accent 蓝：在跑
            Look::Trouble => [0xf2, 0xb5, 0x3c], // --caution 琥珀：有 xrun
        }
    }
}

/// 图标边长。32 足够 Windows 托盘在各种缩放下取用。
const N: usize = 32;

/// 画一个电平条图标。
///
/// 选竖条而不是应用 logo 那根波形线：**16~20 像素下细线会糊成一团**，
/// 而几根粗竖条在任何缩放下都还认得出来。托盘图标的唯一要求是可辨识。
fn icon(look: Look) -> tauri::image::Image<'static> {
    let [r, g, b] = look.rgb();
    let mut px = vec![0u8; N * N * 4];

    // 四根高度不同的竖条，像电平表。停止态画得矮一些，一眼能看出差别。
    let heights: [usize; 4] = match look {
        Look::Idle => [10, 14, 11, 8],
        _ => [16, 26, 20, 12],
    };
    let bar_w = 5;
    let gap = 2;
    let total = heights.len() * bar_w + (heights.len() - 1) * gap;
    let x0 = (N - total) / 2;

    for (i, &h) in heights.iter().enumerate() {
        let bx = x0 + i * (bar_w + gap);
        let by = (N - h) / 2;
        for y in by..by + h {
            for x in bx..bx + bar_w {
                let o = (y * N + x) * 4;
                px[o] = r;
                px[o + 1] = g;
                px[o + 2] = b;
                px[o + 3] = 0xff;
            }
        }
    }

    tauri::image::Image::new_owned(px, N as u32, N as u32)
}

/// 托盘句柄 + 上一次渲染的状态。
///
/// 存上一次状态是为了**只在变化时才更新** —— 指标推送是 20Hz，
/// 每秒改 20 次托盘图标和提示文字既浪费又会让某些 Windows 版本闪烁。
pub struct Tray<R: Runtime> {
    icon: TrayIcon<R>,
    monitor_item: CheckMenuItem<R>,
    status_item: MenuItem<R>,
    engine_item: MenuItem<R>,
    look: AtomicU8,
    /// 上一次的提示文字，用于去重。
    last_tip: std::sync::Mutex<String>,
}

pub fn build<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Arc<Tray<R>>> {
    // 状态行不可点，只用来显示。放在最上面是因为它是托盘的主要职责。
    let status = MenuItem::with_id(app, "status", "引擎未启动", false, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
    let hide = MenuItem::with_id(app, "hide", "隐藏窗口", true, None::<&str>)?;
    let monitor = CheckMenuItem::with_id(app, "monitor", "耳返", true, false, None::<&str>)?;
    let engine = MenuItem::with_id(app, "engine", "停止引擎", false, None::<&str>)?;
    // 关闭窗口只收进托盘，**这里是唯一的退出口** —— 文案要写满，
    // 不能只写「退出」让用户猜是退出窗口还是退出程序。
    let quit = MenuItem::with_id(app, "quit", "退出 wego-voice", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &status,
            &PredefinedMenuItem::separator(app)?,
            &show,
            &hide,
            &PredefinedMenuItem::separator(app)?,
            &monitor,
            &engine,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let icon = TrayIconBuilder::with_id("main")
        .icon(icon(Look::Idle))
        .tooltip("wego-voice — 引擎未启动")
        .menu(&menu)
        // 左键单击显示窗口。Windows 上这是约定，不做的话用户会以为图标是死的。
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                reveal(tray.app_handle());
            }
        })
        .on_menu_event(on_menu)
        .build(app)?;

    Ok(Arc::new(Tray {
        icon,
        monitor_item: monitor,
        status_item: status,
        engine_item: engine,
        look: AtomicU8::new(Look::Idle as u8),
        last_tip: std::sync::Mutex::new(String::new()),
    }))
}

/// 把主窗口拿到前台。最小化过、隐藏过都要能恢复。
fn reveal<R: Runtime>(app: &AppHandle<R>) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn on_menu<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    let state = app.state::<AppState>();
    match event.id().as_ref() {
        "show" => reveal(app),
        "hide" => {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.hide();
            }
        }
        "monitor" => {
            // 取反当前值。菜单项的勾选状态由 update() 统一回写，
            // 这里不自己改 —— 否则下发失败时勾选状态会和实际不符。
            if let Some(p) = state.params() {
                let now = p.monitor_muted.load(REL);
                p.monitor_muted.store(!now, REL);
            }
        }
        "engine" => state.stop(),
        "quit" => {
            // 先停引擎再退出：独占模式下设备要显式释放，
            // 直接 exit 会让声卡在系统里挂一小会儿
            state.stop();
            app.exit(0);
        }
        _ => {}
    }
}

impl<R: Runtime> Tray<R> {
    /// 按当前状态刷新图标、提示与菜单。由指标推送线程调用。
    ///
    /// **只在内容变化时才真的写下去** —— 推送是 20Hz，
    /// 每秒改 20 次托盘既浪费也会在部分 Windows 版本上闪烁。
    pub fn update(&self, tick: &crate::state::Tick, muted: bool) {
        let xruns = tick.metrics.xruns;
        let look = if !tick.running {
            Look::Idle
        } else if xruns > 0 {
            Look::Trouble
        } else {
            Look::Live
        };

        let prev = Look::from_u8(self.look.load(REL));
        if look != prev {
            self.look.store(look as u8, REL);
            let _ = self.icon.set_icon(Some(icon(look)));
        }

        let tip = if tick.running {
            format!(
                "wego-voice — {:.1} ms{}\n⚠️ 声卡被独占，系统其他声音会静音",
                tick.latency_ms,
                if xruns > 0 {
                    format!(" · xrun {xruns}")
                } else {
                    String::new()
                }
            )
        } else {
            "wego-voice — 引擎未启动".to_string()
        };

        if let Ok(mut last) = self.last_tip.lock() {
            if *last != tip {
                let _ = self.icon.set_tooltip(Some(&tip));
                let _ = self.status_item.set_text(if tick.running {
                    format!("运行中 · {:.1} ms", tick.latency_ms)
                } else {
                    "引擎未启动".to_string()
                });
                let _ = self.engine_item.set_enabled(tick.running);
                *last = tip;
            }
        }

        let _ = self.monitor_item.set_checked(tick.running && !muted);
        let _ = self.monitor_item.set_enabled(tick.running);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_wellformed_rgba() {
        for look in [Look::Idle, Look::Live, Look::Trouble] {
            let img = icon(look);
            assert_eq!(img.width(), N as u32);
            assert_eq!(img.height(), N as u32);
            assert_eq!(img.rgba().len(), N * N * 4);
        }
    }

    /// 三种状态必须是**三种颜色**。
    ///
    /// 托盘图标存在的全部意义就是一眼看出"声卡是不是被占着"；
    /// 两种状态撞色就等于没有这个功能。
    #[test]
    fn the_three_states_look_different() {
        let colors: Vec<[u8; 3]> = [Look::Idle, Look::Live, Look::Trouble]
            .iter()
            .map(|l| l.rgb())
            .collect();
        assert_ne!(colors[0], colors[1]);
        assert_ne!(colors[1], colors[2]);
        assert_ne!(colors[0], colors[2]);
    }

    /// 停止态与运行态的竖条高度不同 —— 单色显示器或色觉障碍下
    /// 仍要能区分，不能只靠颜色编码。
    #[test]
    fn state_is_not_conveyed_by_color_alone() {
        let lit = |look: Look| {
            icon(look)
                .rgba()
                .chunks_exact(4)
                .filter(|p| p[3] > 0)
                .count()
        };
        assert_ne!(
            lit(Look::Idle),
            lit(Look::Live),
            "停止态和运行态的图形完全一样，只有颜色不同"
        );
    }

    #[test]
    fn look_roundtrips_through_u8() {
        for l in [Look::Idle, Look::Live, Look::Trouble] {
            assert_eq!(Look::from_u8(l as u8), l);
        }
    }
}
