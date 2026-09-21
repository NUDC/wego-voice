//! 音频线程的实时优先级提升。
//!
//! cpal **不会**帮你做这件事。不做的后果是：操作系统调度器会在你不注意的
//! 时候抢走 CPU，表现为**偶发爆音** —— 而且往往只在用户机器上复现，
//! 在开发机上怎么测都是好的。
//!
//! 对应平台机制：
//! - Windows：`AvSetMmThreadCharacteristics(L"Pro Audio")`
//! - macOS：`thread_policy_set` + `THREAD_TIME_CONSTRAINT_POLICY`
//! - Linux：`SCHED_FIFO`（需 rtkit 或 limits.conf 配合）
//!
//! `audio_thread_priority` crate 把这三套封装成了一个调用。
//!
//! # Phase 0 的一项对照实验
//!
//! 用 `--no-rt` 关掉提权跑一遍，再打开跑一遍，对比 xrun 次数与回调耗时。
//! 这个差值能直接量化提权的价值，也验证我们确实提权成功了。

use std::sync::atomic::{AtomicBool, Ordering};

static PROMOTED: AtomicBool = AtomicBool::new(false);

/// 把**当前线程**提升到实时优先级。必须从音频回调线程内部调用。
///
/// 返回是否成功。失败不是致命错误（某些系统配置下不允许提权）。
///
/// # ⚠️ 这里不打日志，是刻意的
///
/// 本函数在**音频线程**里跑，而音频线程的纪律是：无分配、无锁、无日志、无 syscall。
/// `log::warn!` 会分配、会取锁、最终会写 IO —— 全都违规。
///
/// 早期版本在这里 `eprintln!` 了一行警告。它只在首次回调执行一次，
/// 看起来"代价可忽略"，但这正是纪律被慢慢腐蚀的方式：
/// 下一个人会照着加第二行、第三行。
///
/// 正确做法：**把结果交给调用方记进原子量**（[`crate::metrics::Metrics`] 的
/// `rt_promotions` / `rt_failures`），由控制线程或 UI 去呈现。
/// 诊断页已经在显示这两个数了。
pub fn promote_audio_thread(buffer_frames: u32, sample_rate: u32) -> bool {
    match audio_thread_priority::promote_current_thread_to_real_time(
        buffer_frames,
        sample_rate,
    ) {
        Ok(_handle) => {
            // 注意：`RtPriorityHandle` 没有实现 `Drop` —— 丢掉它**不会**降级。
            // 降级需要显式调用 `demote_current_thread_from_real_time(handle)`。
            //
            // 我们希望优先级在整个流的生命周期内保持，所以这里直接丢弃即可。
            // （早先版本在这里 `mem::forget`，属于对 API 的误解：
            //   既无必要，也让注释说了假话。）
            PROMOTED.store(true, Ordering::Relaxed);
            true
        }
        Err(_e) => {
            // 同上：不在音频线程里打日志。失败通过返回值上报。
            false
        }
    }
}

/// 是否有线程成功提权过。诊断页显示用。
pub fn was_promoted() -> bool {
    PROMOTED.load(Ordering::Relaxed)
}
