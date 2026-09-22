//! 角色（声线预设）的定义与持久化。
//!
//! # 什么是"角色"
//!
//! 一个角色把「唱出来是什么样」所需的全部参数打成一个具名包：
//! 调、修正速度、整体移调、共振峰平移。
//!
//! 界面上原本裸露的「调」被收进了角色里 —— 因为对用户来说，
//! "我要唱成少女音"是一件事，而不是四个需要分别拧的旋钮。
//!
//! # 声线是怎么模拟的（以及做不到什么）
//!
//! 靠两个维度：
//!
//! - **共振峰平移**：缩放频谱包络 ≈ 改变声道长度。这是"音色粗细/性别感"的主因，
//!   在 PSOLA 里做**不增加任何延迟**（见 `psola::set_formant`），也最不失真。
//! - **整体移调**：把音高整体搬走。超过 ±5 半音会有明显金属感。
//!
//! 这两样合起来能做到"像另一个人"，但**做不到"像某个指定的人"** ——
//! 那需要神经声码器（说话人嵌入 + 声学模型），而那条线的
//! 许可证问题还没解决。
//!
//! 所以这里刻意不叫"变声"而叫"角色"：承诺的是可控的声线塑形，不是克隆。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Character {
    pub id: String,
    pub name: String,
    /// 调名，如 "C"、"Am"、"F#chrom"。沿用 `voice_audio::parse_key` 的语法。
    pub key: String,
    /// 修正速度（毫秒）。0 = 电音档。
    pub retune_ms: f32,
    /// 整体移调（半音）。
    pub pitch_shift: f32,
    /// 共振峰平移（半音）。
    pub formant_shift: f32,
    /// 频谱倾斜（dB/八度）。正 = 更亮。声线的第二个维度。
    #[serde(default)]
    pub tilt_db_per_oct: f32,
    /// 一句话听感说明。UI 直接显示，帮用户在不试听的情况下选。
    pub note: String,
    /// 内置角色：可以改、可以复位，但不能删。
    #[serde(default)]
    pub builtin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CharacterStore {
    pub characters: Vec<Character>,
    pub active_id: String,
}

impl Default for CharacterStore {
    fn default() -> Self {
        let characters = builtins();
        Self {
            active_id: characters[0].id.clone(),
            characters,
        }
    }
}

/// 内置角色。
///
/// 排序刻意从"几乎不动"到"变形最大"，因为失真是单调递增的 ——
/// 用户从上往下试，能直接听出代价是怎么涨上来的。
pub fn builtins() -> Vec<Character> {
    let c = |id: &str,
             name: &str,
             pitch: f32,
             formant: f32,
             tilt: f32,
             retune: f32,
             note: &str| Character {
        id: id.into(),
        name: name.into(),
        key: "C".into(),
        retune_ms: retune,
        pitch_shift: pitch,
        formant_shift: formant,
        tilt_db_per_oct: tilt,
        note: note.into(),
        builtin: true,
    };

    vec![
        c("origin", "原声", 0.0, 0.0, 0.0, 40.0, "只修音准，完全不动声线"),
        c("teen", "少年", 0.0, 2.0, 0.8, 40.0, "声道略短、清亮一点；音高不动，几乎听不出处理痕迹"),
        c("lady", "御姐", 1.0, 1.5, -0.5, 45.0, "偏低的女声，保留较多原音色"),
        c("girl", "少女", 2.0, 4.0, 1.2, 30.0, "女声方向最稳的一档"),
        c("kid", "童声", 5.0, 6.0, 1.8, 25.0, "变形最大：移调 5 个半音，会带明显金属感"),
        c("uncle", "大叔", -3.0, -3.0, -1.5, 50.0, "声道变长，整体压沉"),
        c("robot", "电音", 0.0, 0.0, 0.0, 0.0, "瞬间吸附到音阶上，不模拟任何人声"),
    ]
}

/// 角色文件的位置。放 app 配置目录，卸载时不会被误删用户自建的角色。
pub fn store_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("取配置目录失败：{e}"))?;
    Ok(dir.join("characters.json"))
}

/// 读取角色库。
///
/// **任何读取失败都不算错误** —— 返回内置角色即可。
/// 配置文件坏掉不该让用户连唱都唱不了，只在日志里留痕。
pub fn load(app: &tauri::AppHandle) -> CharacterStore {
    let Ok(path) = store_path(app) else {
        return CharacterStore::default();
    };
    let mut store = match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<CharacterStore>(&text) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("角色文件解析失败，改用内置角色：{e}（{}）", path.display());
                CharacterStore::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => CharacterStore::default(),
        Err(e) => {
            log::warn!("角色文件读取失败，改用内置角色：{e}");
            CharacterStore::default()
        }
    };

    // 版本升级后新增的内置角色要补进来，否则老用户永远看不到它们。
    // 已存在的 id 保持用户改过的值，不覆盖。
    for b in builtins() {
        if !store.characters.iter().any(|c| c.id == b.id) {
            store.characters.push(b);
        }
    }
    if !store.characters.iter().any(|c| c.id == store.active_id) {
        store.active_id = store
            .characters
            .first()
            .map(|c| c.id.clone())
            .unwrap_or_default();
    }
    store
}

/// 写回角色库。
///
/// 先写临时文件再改名：直接覆写时若中途断电/崩溃，
/// 用户自建的全部角色会变成半截 JSON，下次启动直接丢光。
pub fn save(app: &tauri::AppHandle, store: &CharacterStore) -> Result<(), String> {
    let path = store_path(app)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败：{e}"))?;
    }
    let text =
        serde_json::to_string_pretty(store).map_err(|e| format!("角色序列化失败：{e}"))?;

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写入角色文件失败：{e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("替换角色文件失败：{e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_ids_are_unique() {
        let b = builtins();
        for (i, x) in b.iter().enumerate() {
            assert!(
                !b[i + 1..].iter().any(|y| y.id == x.id),
                "内置角色 id 重复：{}",
                x.id
            );
        }
    }

    #[test]
    fn builtin_shifts_stay_within_engine_limits() {
        // Corrector 会把超限值钳掉，内置角色不该依赖那个兜底
        for c in builtins() {
            assert!(c.pitch_shift.abs() <= 12.0, "{} 移调越界", c.name);
            assert!(c.formant_shift.abs() <= 12.0, "{} 共振峰越界", c.name);
            assert!(
                c.tilt_db_per_oct.abs() <= voice_core::tilt::MAX_DB_PER_OCT,
                "{} 倾斜越界",
                c.name
            );
            assert!(c.retune_ms >= 0.0, "{} 修正速度为负", c.name);
            assert!(
                voice_audio::parse_key(&c.key).is_some(),
                "{} 的调名 {:?} 解析不了",
                c.name,
                c.key
            );
        }
    }

    #[test]
    fn default_store_points_at_an_existing_character() {
        let s = CharacterStore::default();
        assert!(s.characters.iter().any(|c| c.id == s.active_id));
    }

    #[test]
    fn store_roundtrips_through_json() {
        let s = CharacterStore::default();
        let text = serde_json::to_string(&s).unwrap();
        let back: CharacterStore = serde_json::from_str(&text).unwrap();
        assert_eq!(back.characters, s.characters);
        assert_eq!(back.active_id, s.active_id);
    }
}
