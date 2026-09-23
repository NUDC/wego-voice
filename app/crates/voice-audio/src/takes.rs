//! 录音目录的管理：列出、配对、重命名、删除。
//!
//! # 为什么这些逻辑不写在 `src-tauri/commands.rs` 里
//!
//! command 要有 `AppHandle` 才跑得起来，而 `AppHandle` 要有 WebView ——
//! 也就是说写在那里的逻辑**测不了**。路径包含校验、文件名净化、
//! 干声与校准版的配对，每一条错了都会造成实际损害（删错文件、
//! 把录音改成一个打不开的名字），恰恰是最需要测试的部分。
//!
//! 所以这里放纯逻辑 + 文件操作，command 那边只负责「录音目录在哪」。
//!
//! # 配对
//!
//! 离线校准的产物叫 `<原名>-corrected.wav`，就放在原录音旁边。
//! 列目录时把它**挂在原录音下面**，而不是当成平级的第二条录音 ——
//! 它们是同一次演唱的两个版本，平铺会让列表在几次处理之后失去意义。

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// 离线校准产物的文件名后缀（在 `.wav` 之前）。
///
/// ⚠️ 必须和 `job::output_path` 保持一致。那边改了这边不改，
/// 产物就会变成列表里一条孤零零的录音。
pub const CORRECTED_SUFFIX: &str = "-corrected";

/// 录音目录里的一条 take。
#[derive(Debug, Clone, PartialEq)]
pub struct Take {
    pub name: String,
    pub path: PathBuf,
    pub seconds: f32,
    pub bytes: u64,
    /// 修改时间，UNIX 秒。取不到时为 0。
    pub modified: u64,
    /// 配套的离线校准产物。
    pub corrected: Option<PathBuf>,
    pub corrected_seconds: f32,
    pub corrected_bytes: u64,
}

fn is_wav(p: &Path) -> bool {
    p.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
}

/// 文件名（不含扩展名）是不是离线校准的产物。
pub fn is_corrected_stem(stem: &str) -> bool {
    stem.ends_with(CORRECTED_SUFFIX)
}

/// 列出录音目录里的所有 take，新的在前，校准版挂在对应的干声下面。
///
/// 目录不存在或读不动时返回空表 —— 这不是错误，是"还没录过"。
pub fn scan(dir: &Path) -> Vec<Take> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut plain: Vec<PathBuf> = Vec::new();
    let mut corrected: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();

    for e in rd.flatten() {
        let p = e.path();
        if !p.is_file() || !is_wav(&p) {
            continue;
        }
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if is_corrected_stem(stem) {
            let src = stem[..stem.len() - CORRECTED_SUFFIX.len()].to_string();
            corrected.insert(src, p);
        } else {
            plain.push(p);
        }
    }

    let mut out: Vec<Take> = plain
        .into_iter()
        .map(|p| {
            let stem = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let c = corrected.remove(&stem);
            Take {
                name: p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                seconds: secs_of(&p),
                bytes: bytes_of(&p),
                modified: modified_of(&p),
                corrected_seconds: c.as_deref().map(secs_of).unwrap_or(0.0),
                corrected_bytes: c.as_deref().map(bytes_of).unwrap_or(0),
                corrected: c,
                path: p,
            }
        })
        .collect();

    // 校准版找不到对应干声时（用户手工删了原录音）仍然列出来 ——
    // 悄悄藏掉一个磁盘上确实存在的文件，用户会以为处理没成功。
    for (stem, p) in corrected {
        out.push(Take {
            name: p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            seconds: secs_of(&p),
            bytes: bytes_of(&p),
            modified: modified_of(&p),
            corrected: None,
            corrected_seconds: 0.0,
            corrected_bytes: 0,
            path: p,
        });
        let _ = stem;
    }

    out.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.name.cmp(&b.name)));
    out
}

/// 只读文件头拿时长 —— 列目录不该把每个文件都读进内存。
fn secs_of(p: &Path) -> f32 {
    crate::wav::probe(p).map(|(_, s)| s).unwrap_or(0.0)
}

fn bytes_of(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn modified_of(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 确认 `path` 真的落在 `dir` 里面，返回规范化后的路径。
///
/// # 为什么必须有
///
/// 路径是前端传来的。前端被注入、或者我自己哪天写错一个拼接，
/// 都可能让「删除录音」删到别的地方去。这道闸放在**能删东西之前**。
///
/// 用 `canonicalize` 而不是字符串前缀比较：`..` 和符号链接
/// 在字符串层面看不出来，规范化之后才现形。
pub fn within(dir: &Path, path: &Path) -> Result<PathBuf> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("录音目录不可用：{}", dir.display()))?;
    let p = path
        .canonicalize()
        .with_context(|| format!("文件不存在：{}", path.display()))?;
    if !p.starts_with(&dir) {
        bail!("这个文件不在录音目录里，拒绝操作：{}", path.display());
    }
    if !is_wav(&p) {
        bail!("只处理 .wav 文件");
    }
    Ok(plain(p))
}

/// 去掉 `canonicalize` 加上的 `\\?\` 前缀。
///
/// # 为什么必须去
///
/// Windows 上 `canonicalize` 一律返回扩展长度路径（`\\?\C:\...`）。
/// 它在 Rust 的文件 API 里能用，但**外面的世界大多不认**：
///
/// - `SHFileOperationW`（删到回收站）直接返回 124 `DE_INVALIDFILES`
/// - `explorer /select,` 定位不到文件
/// - 这个路径还会原样显示在界面上，用户看着莫名其妙
///
/// UNC 形式（`\\?\UNC\server\share`）不能这么剥 —— 剥完就不是路径了，
/// 所以只处理 `\\?\X:\` 这一种。
fn plain(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    let Some(rest) = s.strip_prefix(r"\\?\") else {
        return p;
    };
    let b = rest.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
    if drive {
        PathBuf::from(rest)
    } else {
        p
    }
}

/// 把用户输入净化成一个能落盘的文件名（带 `.wav`）。
///
/// 拒绝而不是悄悄替换非法字符：用户输入了 `a/b`，
/// 悄悄存成 `a_b` 会让他下次按 `a/b` 去找，找不到。
pub fn sanitize_name(input: &str) -> Result<String> {
    let s = input.trim();
    let s = s.strip_suffix(".wav").unwrap_or(s);
    let s = s.strip_suffix(".WAV").unwrap_or(s);
    if s.is_empty() {
        bail!("名字不能为空");
    }
    if s.len() > 120 {
        bail!("名字太长了（最多 120 字节）");
    }
    // Windows 的非法字符 + 路径分隔符。`..` 单独挡一次。
    if let Some(bad) = s.chars().find(|c| r#"\/:*?"<>|"#.contains(*c) || (*c as u32) < 0x20) {
        bail!("名字里不能有 {bad:?}");
    }
    if s == "." || s == ".." || s.ends_with(' ') || s.ends_with('.') {
        bail!("Windows 不接受这个名字");
    }
    Ok(format!("{s}.wav"))
}

/// 重命名一条录音。**配套的校准版跟着改**，否则配对关系就断了。
///
/// 返回新路径。
pub fn rename(dir: &Path, path: &Path, new_name: &str) -> Result<PathBuf> {
    let src = within(dir, path)?;
    let name = sanitize_name(new_name)?;
    let dst = src.with_file_name(&name);
    if dst == src {
        return Ok(dst);
    }
    if dst.exists() {
        bail!("已经有一条录音叫这个名字了");
    }

    // 先改校准版：干声改完再失败的话，配对关系是断的，
    // 而用户看到的是"改成功了"。宁可先动那个不要紧的。
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    let corrected = src.with_file_name(format!("{stem}{CORRECTED_SUFFIX}.wav"));
    if corrected.exists() {
        let new_stem = dst.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        let target = dst.with_file_name(format!("{new_stem}{CORRECTED_SUFFIX}.wav"));
        std::fs::rename(&corrected, &target).with_context(|| "改校准版的名字失败")?;
    }

    std::fs::rename(&src, &dst).with_context(|| format!("改名失败：{}", src.display()))?;
    Ok(dst)
}

/// 删除一条录音（连同它的校准版）。
///
/// **走回收站，不是直接删掉。** 录音是用户唱出来的东西，
/// 删错了没有任何办法找回来 —— 一次误点的代价太大。
pub fn delete(dir: &Path, path: &Path, with_corrected: bool) -> Result<()> {
    let src = within(dir, path)?;

    let mut victims = vec![src.clone()];
    if with_corrected {
        let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        let c = src.with_file_name(format!("{stem}{CORRECTED_SUFFIX}.wav"));
        if c.exists() {
            victims.push(c);
        }
    }
    recycle(&victims)
}

/// 把文件移进回收站。
///
/// 用 `SHFileOperationW` + `FOF_ALLOWUNDO`。没有走 `std::fs::remove_file`
/// 的后备路径 —— 后备就意味着"有时候直接删掉"，那正是这个函数要避免的事。
/// 失败就如实报错，让用户自己去资源管理器里删。
#[cfg(windows)]
fn recycle(paths: &[PathBuf]) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const FO_DELETE: u32 = 0x0003;
    const FOF_SILENT: u16 = 0x0004;
    const FOF_NOCONFIRMATION: u16 = 0x0010;
    const FOF_ALLOWUNDO: u16 = 0x0040;
    const FOF_NOERRORUI: u16 = 0x0400;

    #[repr(C)]
    struct ShFileOpStructW {
        hwnd: *mut std::ffi::c_void,
        w_func: u32,
        p_from: *const u16,
        p_to: *const u16,
        f_flags: u16,
        f_any_operations_aborted: i32,
        h_name_mappings: *mut std::ffi::c_void,
        lpsz_progress_title: *const u16,
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHFileOperationW(lp_file_op: *mut ShFileOpStructW) -> i32;
    }

    // `pFrom` 是**双 null 结尾**的字符串列表。少写一个 null,
    // API 会读过界 —— 这是这段代码最容易错的地方。
    let mut from: Vec<u16> = Vec::new();
    for p in paths {
        from.extend(p.as_os_str().encode_wide());
        from.push(0);
    }
    from.push(0);

    let mut op = ShFileOpStructW {
        hwnd: std::ptr::null_mut(),
        w_func: FO_DELETE,
        p_from: from.as_ptr(),
        p_to: std::ptr::null(),
        f_flags: FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI,
        f_any_operations_aborted: 0,
        h_name_mappings: std::ptr::null_mut(),
        lpsz_progress_title: std::ptr::null(),
    };

    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 {
        bail!("移进回收站失败（SHFileOperation 返回 {rc}）—— 可以在资源管理器里手动删除");
    }
    if op.f_any_operations_aborted != 0 {
        bail!("删除被中断");
    }
    Ok(())
}

#[cfg(not(windows))]
fn recycle(paths: &[PathBuf]) -> Result<()> {
    // 本项目只支持 Windows（WASAPI 独占）。这条分支只为让非 Windows 上的
    // `cargo check` 过得去，不做"直接删掉"的等价实现 —— 那不是等价的。
    let _ = paths;
    bail!("只在 Windows 上支持删除")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wego-takes-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn tone(dir: &Path, name: &str, secs: f32) -> PathBuf {
        let n = (secs * 8_000.0) as usize;
        let x: Vec<f32> = (0..n).map(|i| (i as f32 * 0.01).sin() * 0.3).collect();
        let p = dir.join(name);
        crate::wav::write(&p, &x, 8_000).unwrap();
        p
    }

    #[test]
    fn pairs_corrected_output_with_its_source() {
        let d = tmpdir("pair");
        tone(&d, "wego-1.wav", 1.0);
        tone(&d, "wego-1-corrected.wav", 1.0);
        tone(&d, "wego-2.wav", 2.0);

        let t = scan(&d);
        assert_eq!(t.len(), 2, "校准版被当成了独立的一条");
        let one = t.iter().find(|x| x.name == "wego-1.wav").unwrap();
        assert!(one.corrected.is_some(), "没配上校准版");
        let two = t.iter().find(|x| x.name == "wego-2.wav").unwrap();
        assert!(two.corrected.is_none());
        assert!((two.seconds - 2.0).abs() < 0.05, "时长不对：{}", two.seconds);
    }

    /// 干声被手工删掉之后，校准版仍然要出现在列表里 ——
    /// 磁盘上有的文件却不显示，用户会以为处理失败了。
    #[test]
    fn orphan_corrected_is_still_listed() {
        let d = tmpdir("orphan");
        tone(&d, "wego-9-corrected.wav", 1.0);
        let t = scan(&d);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].name, "wego-9-corrected.wav");
    }

    #[test]
    fn scan_of_missing_dir_is_empty_not_an_error() {
        assert!(scan(Path::new(r"C:\definitely\not\here\wego")).is_empty());
    }

    #[test]
    fn rejects_paths_outside_the_recordings_dir() {
        let d = tmpdir("guard");
        let outside = std::env::temp_dir().join("wego-outside.wav");
        tone(std::env::temp_dir().as_path(), "wego-outside.wav", 0.2);

        let e = within(&d, &outside).unwrap_err().to_string();
        assert!(e.contains("不在录音目录"), "{e}");

        // 目录里的正常文件要放行
        let inside = tone(&d, "ok.wav", 0.2);
        assert!(within(&d, &inside).is_ok());

        let _ = std::fs::remove_file(&outside);
    }

    /// `..` 在字符串层面看不出来，必须靠规范化拦下。
    #[test]
    fn rejects_dot_dot_escape() {
        let d = tmpdir("escape");
        tone(std::env::temp_dir().as_path(), "wego-escape.wav", 0.2);
        let sneaky = d.join("..").join("wego-escape.wav");
        assert!(within(&d, &sneaky).is_err(), "`..` 逃逸没被拦住");
        let _ = std::fs::remove_file(std::env::temp_dir().join("wego-escape.wav"));
    }

    #[test]
    fn sanitize_rejects_what_windows_rejects() {
        assert_eq!(sanitize_name("第一条").unwrap(), "第一条.wav");
        assert_eq!(sanitize_name(" 带空格 ").unwrap(), "带空格.wav");
        assert_eq!(sanitize_name("已经带了.wav").unwrap(), "已经带了.wav");
        for bad in ["", "   ", "a/b", "a\\b", "a:b", "a*b", "a?b", "..", "x."] {
            assert!(sanitize_name(bad).is_err(), "{bad:?} 应该被拒绝");
        }
    }

    #[test]
    fn rename_takes_the_corrected_file_along() {
        let d = tmpdir("rename");
        let src = tone(&d, "wego-7.wav", 0.5);
        tone(&d, "wego-7-corrected.wav", 0.5);

        let dst = rename(&d, &src, "副歌第三条").unwrap();
        assert_eq!(dst.file_name().unwrap(), "副歌第三条.wav");
        assert!(dst.exists());
        assert!(
            d.join("副歌第三条-corrected.wav").exists(),
            "校准版没跟着改名，配对关系断了"
        );
        assert!(!src.exists());

        let t = scan(&d);
        assert_eq!(t.len(), 1);
        assert!(t[0].corrected.is_some());
    }

    #[test]
    fn rename_refuses_to_overwrite() {
        let d = tmpdir("clash");
        let a = tone(&d, "a.wav", 0.2);
        tone(&d, "b.wav", 0.2);
        let e = rename(&d, &a, "b").unwrap_err().to_string();
        assert!(e.contains("已经有一条"), "{e}");
        assert!(a.exists(), "改名失败却把原文件弄没了");
    }

    /// 删除走回收站。这里只验"文件确实从目录里消失了" ——
    /// 回收站里躺着哪一份不归我们管，也没法在测试里断言。
    #[test]
    fn delete_removes_both_files() {
        let d = tmpdir("delete");
        let src = tone(&d, "wego-8.wav", 0.3);
        let c = d.join("wego-8-corrected.wav");
        tone(&d, "wego-8-corrected.wav", 0.3);

        delete(&d, &src, true).unwrap();
        assert!(!src.exists(), "干声还在");
        assert!(!c.exists(), "校准版还在");
        assert!(scan(&d).is_empty());
    }

    #[test]
    fn delete_can_keep_the_corrected_version() {
        let d = tmpdir("delete-one");
        let src = tone(&d, "wego-5.wav", 0.3);
        tone(&d, "wego-5-corrected.wav", 0.3);

        delete(&d, &src, false).unwrap();
        assert!(!src.exists());
        assert!(d.join("wego-5-corrected.wav").exists(), "不该删校准版");
    }

    /// ⚠️ 回归测试：交出去的路径不许带 `\\?\`。
    ///
    /// `canonicalize` 在 Windows 上一定会加这个前缀，而加了之后
    /// `SHFileOperationW` 直接返回 124（删除全线失败）、`explorer /select,`
    /// 定位不到文件、界面上还会把这串前缀显示给用户看。
    /// 这一条是被上面那两个删除测试**实际抓出来**的。
    #[test]
    fn returned_paths_carry_no_verbatim_prefix() {
        let d = tmpdir("verbatim");
        let p = tone(&d, "v.wav", 0.2);
        let got = within(&d, &p).unwrap();
        assert!(
            !got.to_string_lossy().starts_with(r"\\?\"),
            "路径带了扩展长度前缀：{}",
            got.display()
        );
        assert!(got.exists());
    }

    /// 产物后缀必须和 `job::output_path` 一致，否则列表配不上对。
    #[test]
    fn suffix_matches_the_job_module() {
        let out = crate::job::output_path(Path::new(r"C:\x\wego-1.wav"));
        let stem = out.file_stem().unwrap().to_str().unwrap();
        assert!(is_corrected_stem(stem), "job 产出的名字：{stem}");
    }
}
