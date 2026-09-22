//! # voice-audio
//!
//! wego-voice 的实时音频引擎：cpal 双流桥接、时钟漂移补偿、实时线程提权、
//! 延迟与 xrun 测量。
//!
//! 这一层**只做 I/O 与调度**，所有 DSP 在 [`voice_core`] 里。
//! 分开是为了让 DSP 核心能脱离 cpal 复用（见实施方案 §9.3）。
//!
//! ## 两个宿主，同一套引擎
//!
//! - `wego-bench`（本 crate 的 bin）：headless 压测，不牵扯 Tauri，迭代快
//! - `src-tauri`：产品 App，`--bench` 参数跑的是同一套引擎
//!
//! 两者数字的差值就是 Tauri 外壳的净开销 —— 同源对比，没有框架差异混在里面。

pub mod backend;
pub mod duplex;
pub mod engine;
pub mod latency;
pub mod metrics;
pub mod params;
pub mod priority;
pub mod recorder;

pub use backend::{BackendConfig, BackendInfo, BackendKind};
pub use engine::{list_devices, AudioEngine, DeviceList, EngineConfig};
pub use latency::{ImpulseProbe, LatencyStats};
pub use metrics::{Metrics, MetricsSnapshot};
pub use params::{parse_key, Params};
pub use recorder::{timestamped_name, Recorder, RecorderSink, RecorderState};

/// 录音器的共享槽位。
///
/// 采集端（`RecorderSink`）住在音频线程里，控制端（`Recorder`）住在这里。
/// 用 `Option` 是因为它在 `duplex::new` 里才被创建 ——
/// 而后端为抢独占设备会重试，每次重试都会覆盖成一对新的。
///
/// ⚠️ 这把锁**只允许控制线程碰**（开始/停止录音、查状态）。
/// 音频线程走的是 `RecorderSink`，那条路上一个锁都没有。
pub type RecorderSlot = std::sync::Arc<std::sync::Mutex<Option<Recorder>>>;

/// Phase 0 的判定阈值，集中在这里，避免散落在各处各写一份。
///
/// 来源：实施方案 §3.3 延迟预算与 §7 验收指标。
pub mod thresholds {
    /// 端到端耳返延迟的 No-Go 线（毫秒）。超过即 Phase 0 不通过。
    pub const LATENCY_NO_GO_MS: f32 = 30.0;
    /// 理想目标（毫秒）。
    pub const LATENCY_TARGET_MS: f32 = 25.0;
    /// 人耳感知为"自己的声音"的上限（毫秒）。
    pub const LATENCY_NATURAL_MS: f32 = 20.0;
    /// 超过此值触发延迟听觉反馈（DAF）效应，产品直接失效。
    pub const LATENCY_DAF_MS: f32 = 50.0;

    /// 10 分钟连测允许的环形缓冲水位漂移上限（毫秒）。
    pub const DRIFT_NO_GO_MS: f32 = 5.0;

    /// 判定延迟处于哪一档。
    pub fn verdict(ms: f32) -> &'static str {
        if ms <= LATENCY_NATURAL_MS {
            "自然（感知为自己的声音）"
        } else if ms <= LATENCY_TARGET_MS {
            "达标"
        } else if ms <= LATENCY_NO_GO_MS {
            "勉强可用（已触及预算上限）"
        } else if ms < LATENCY_DAF_MS {
            "超标（会察觉到声音发飘）"
        } else {
            "失效（触发 DAF，比不开更糟）"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::thresholds::*;

    #[test]
    fn verdict_boundaries() {
        assert!(verdict(15.0).contains("自然"));
        assert!(verdict(23.0).contains("达标"));
        assert!(verdict(28.0).contains("勉强"));
        assert!(verdict(40.0).contains("超标"));
        assert!(verdict(60.0).contains("失效"));
    }
}
