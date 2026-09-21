//! 音频线程的运行时参数。
//!
//! 控制线程写、音频线程读，全部走原子量 —— 音频回调里不许加锁。
//! 这是 UI 上那些滑杆/下拉框与 DSP 之间唯一的通道。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use voice_core::{Key, ScaleKind};

const REL: Ordering = Ordering::Relaxed;

pub struct Params {
    /// 修正速度（毫秒）。0 = 电音档。以 f32 位模式存放。
    retune_ms: AtomicU32,
    /// 主音，0..=11。
    tonic: AtomicU32,
    /// 调式，见 `scale_from_index`。
    scale: AtomicU32,
    /// 旁路：不修音，但保持同样的延迟（便于 A/B 盲测）。
    pub bypass: AtomicBool,
    /// 耳返静音（蓝牙降级模式下只看音高条，不听修正声）。
    pub monitor_muted: AtomicBool,
    /// 耳返增益，f32 位模式。
    monitor_gain: AtomicU32,
    /// 角色：整体移调（半音）。f32 位模式。
    pitch_shift: AtomicU32,
    /// 角色：共振峰平移（半音）。f32 位模式。
    formant_shift: AtomicU32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            retune_ms: AtomicU32::new(40.0f32.to_bits()),
            tonic: AtomicU32::new(0),
            scale: AtomicU32::new(0),
            bypass: AtomicBool::new(false),
            monitor_muted: AtomicBool::new(false),
            monitor_gain: AtomicU32::new(1.0f32.to_bits()),
            pitch_shift: AtomicU32::new(0.0f32.to_bits()),
            formant_shift: AtomicU32::new(0.0f32.to_bits()),
        }
    }
}

impl Params {
    pub fn retune_ms(&self) -> f32 {
        f32::from_bits(self.retune_ms.load(REL))
    }

    pub fn set_retune_ms(&self, v: f32) {
        self.retune_ms.store(v.max(0.0).to_bits(), REL);
    }

    pub fn monitor_gain(&self) -> f32 {
        f32::from_bits(self.monitor_gain.load(REL))
    }

    pub fn set_monitor_gain(&self, v: f32) {
        self.monitor_gain.store(v.clamp(0.0, 4.0).to_bits(), REL);
    }

    pub fn pitch_shift(&self) -> f32 {
        f32::from_bits(self.pitch_shift.load(REL))
    }

    pub fn set_pitch_shift(&self, v: f32) {
        self.pitch_shift.store(v.clamp(-12.0, 12.0).to_bits(), REL);
    }

    pub fn formant_shift(&self) -> f32 {
        f32::from_bits(self.formant_shift.load(REL))
    }

    pub fn set_formant_shift(&self, v: f32) {
        self.formant_shift.store(v.clamp(-12.0, 12.0).to_bits(), REL);
    }

    pub fn key(&self) -> Key {
        Key {
            tonic: (self.tonic.load(REL) % 12) as u8,
            scale: scale_from_index(self.scale.load(REL)),
        }
    }

    pub fn set_key(&self, key: Key) {
        self.tonic.store(key.tonic as u32 % 12, REL);
        self.scale.store(scale_to_index(key.scale), REL);
    }
}

pub fn scale_from_index(i: u32) -> ScaleKind {
    match i {
        1 => ScaleKind::Major,
        2 => ScaleKind::NaturalMinor,
        3 => ScaleKind::HarmonicMinor,
        4 => ScaleKind::MajorPentatonic,
        5 => ScaleKind::MinorPentatonic,
        _ => ScaleKind::Chromatic,
    }
}

pub fn scale_to_index(s: ScaleKind) -> u32 {
    match s {
        ScaleKind::Chromatic => 0,
        ScaleKind::Major => 1,
        ScaleKind::NaturalMinor => 2,
        ScaleKind::HarmonicMinor => 3,
        ScaleKind::MajorPentatonic => 4,
        ScaleKind::MinorPentatonic => 5,
    }
}

/// 从字符串解析调名，如 "C", "A#", "F#m", "Dmaj"。用于 CLI 与 UI。
pub fn parse_key(s: &str) -> Option<Key> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let mut idx = match bytes[0].to_ascii_uppercase() {
        b'C' => 0,
        b'D' => 2,
        b'E' => 4,
        b'F' => 5,
        b'G' => 7,
        b'A' => 9,
        b'B' => 11,
        _ => return None,
    };
    let mut pos = 1;
    if pos < bytes.len() {
        match bytes[pos] {
            b'#' => {
                idx = (idx + 1) % 12;
                pos += 1;
            }
            b'b' => {
                idx = (idx + 11) % 12;
                pos += 1;
            }
            _ => {}
        }
    }
    let rest = s[pos..].trim().to_ascii_lowercase();
    let scale = match rest.as_str() {
        "" | "maj" | "major" => ScaleKind::Major,
        "m" | "min" | "minor" => ScaleKind::NaturalMinor,
        "hm" | "harmonic" => ScaleKind::HarmonicMinor,
        "pent" | "majpent" => ScaleKind::MajorPentatonic,
        "minpent" => ScaleKind::MinorPentatonic,
        "chrom" | "chromatic" => ScaleKind::Chromatic,
        _ => return None,
    };
    Some(Key { tonic: idx as u8, scale })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_index_roundtrip() {
        for s in [
            ScaleKind::Chromatic,
            ScaleKind::Major,
            ScaleKind::NaturalMinor,
            ScaleKind::HarmonicMinor,
            ScaleKind::MajorPentatonic,
            ScaleKind::MinorPentatonic,
        ] {
            assert_eq!(scale_from_index(scale_to_index(s)), s);
        }
    }

    #[test]
    fn parses_key_names() {
        assert_eq!(parse_key("C"), Some(Key { tonic: 0, scale: ScaleKind::Major }));
        assert_eq!(parse_key("A#"), Some(Key { tonic: 10, scale: ScaleKind::Major }));
        assert_eq!(parse_key("Db"), Some(Key { tonic: 1, scale: ScaleKind::Major }));
        assert_eq!(parse_key("F#m"), Some(Key { tonic: 6, scale: ScaleKind::NaturalMinor }));
        assert_eq!(parse_key("Gchrom"), Some(Key { tonic: 7, scale: ScaleKind::Chromatic }));
        assert_eq!(parse_key("H"), None);
    }

    #[test]
    fn params_roundtrip() {
        let p = Params::default();
        p.set_retune_ms(12.5);
        assert!((p.retune_ms() - 12.5).abs() < 1e-6);
        let k = Key { tonic: 9, scale: ScaleKind::MinorPentatonic };
        p.set_key(k);
        assert_eq!(p.key(), k);
    }
}
