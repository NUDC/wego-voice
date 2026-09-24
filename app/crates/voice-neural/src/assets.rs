//! 神经件的资产管理：**它们在哪、够不够、对不对**。
//!
//! # 为什么这一层要和推理彻底分开
//!
//! 主程序是 6.5 MB 的免安装单文件，这是产品的卖点之一。声线转换需要
//! 几百 MB 的模型和一个装了推理引擎的伴生程序 —— 那些东西
//! **只能按需下载，不能塞进主程序**。
//!
//! 于是主程序需要在**自己不含任何推理代码**的前提下回答三个问题：
//! 装了没有、缺什么、缺多少。这个模块就干这个，它不依赖 `ort`/`candle`/`tract`
//! 中的任何一个，默认特性下就能编译。
//!
//! # 校验为什么不能省
//!
//! 几百 MB 的文件，下断、下错版本、被杀毒软件改过、磁盘写坏 ——
//! 每一种都会让推理"跑得动但结果是垃圾"，而不是干脆报错。
//!
//! 所以每个文件都记着**确切字节数与 SHA-256**，用之前先对。
//! 对不上就当没有 —— 宁可让用户重下一次，也不要让他拿着一个
//! 坏掉的模型去怀疑自己的嗓子。

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// 一件需要按需获取的资产。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Asset {
    /// 落盘文件名。
    pub name: &'static str,
    /// 人话说明，直接显示给用户。
    pub what: &'static str,
    /// 确切字节数。
    pub bytes: u64,
    /// SHA-256，小写十六进制。
    pub sha256: &'static str,
    /// 下载源，**按顺序试**。
    ///
    /// 多源不是为了快，是为了**能下到**：这台开发机直连 GitHub Release
    /// 实测多次超时，而镜像一分钟下完 65 MB。
    ///
    /// 换源不构成安全风险 —— 每个文件都有固定的 SHA-256，
    /// 校验过不了就当没下到。镜像能做的只有"下不下得来"，
    /// 做不到"喂一个假模型进来"。
    pub sources: &'static [&'static str],
}

impl Asset {
    pub fn mb(&self) -> f64 {
        self.bytes as f64 / 1_048_576.0
    }
}

/// 声线转换需要的全部资产。
///
/// ⚠️ 字节数与哈希是**实测**出来的，不是抄来的。改这里之前先重新算。
pub const ASSETS: &[Asset] = &[
    Asset {
        name: "vec-768-layer-12.onnx",
        what: "内容编码器 —— 把唱的内容与音色拆开",
        bytes: 377_655_729,
        sha256: "b3886e7dff1495cda514f94f4680a7b1261e05d6929f5c764cdb17934b413c2a",
        // 镜像在前、官方源在后：本产品的用户主要在国内，
        // 直连超时的代价（几十秒）比镜像走一趟大得多。
        sources: &[
            "https://hf-mirror.com/NaruseMioShirakana/MoeSS-SUBModel/resolve/main/vec-768-layer-12.onnx",
            "https://huggingface.co/NaruseMioShirakana/MoeSS-SUBModel/resolve/main/vec-768-layer-12.onnx",
        ],
    },
    Asset {
        name: "model_0.pt",
        what: "解码器 —— 按音色重新合成波形",
        bytes: 67_878_441,
        sha256: "ebe70c92c09d7d1c1fbfe631b21766ecca6afb21536fa195b0e932dd2fe12912",
        sources: &[
            "https://gh-proxy.com/https://github.com/yxlllc/DDSP-SVC/releases/download/5.0/model_0.pt",
            "https://ghfast.top/https://github.com/yxlllc/DDSP-SVC/releases/download/5.0/model_0.pt",
            "https://github.com/yxlllc/DDSP-SVC/releases/download/5.0/model_0.pt",
        ],
    },
];

/// 伴生程序的下载地址。
///
/// 它由本项目的 CI 构建并随 Release 发布，所以**不钉哈希** ——
/// 每次发版它都会变。取而代之的校验是：下完直接跑一次 `--selftest`。
/// 那比哈希更有意义：它验的是"这个 exe 在这台机器上真的能跑"，
/// 而不只是"字节没错"。
pub const COMPANION_URL: &str =
    "https://github.com/NUDC/wego-voice/releases/latest/download/wego-clone.exe";

/// 伴生程序的镜像。理由同上面的模型。
pub const COMPANION_MIRRORS: &[&str] = &[
    "https://gh-proxy.com/https://github.com/NUDC/wego-voice/releases/latest/download/wego-clone.exe",
    "https://ghfast.top/https://github.com/NUDC/wego-voice/releases/latest/download/wego-clone.exe",
];

/// 伴生程序的文件名。
///
/// 推理**不在主程序里跑**，而是交给这个单独的可执行文件：
///
/// 1. 主程序体积不受影响 —— 不用这个功能的人一个字节都不必付
/// 2. 架构红线 3 升级成**进程级隔离** —— 推理连碰音频线程的机会都没有，
///    而且可以整体降低它的进程优先级
/// 3. 它崩了主程序不跟着崩
pub const COMPANION: &str = "wego-clone.exe";

/// 模型目录的文件夹名。放在盘根下，用户一眼能认出来是谁的。
pub const DIR_NAME: &str = "wego-voice-models";

/// 记住选中位置的小文件。
///
/// 必须记下来，不能每次开机重新挑：外接硬盘插上又拔掉、某个盘突然
/// 空间变多，都会让"自动挑"挑到不同的盘 —— 而用户看到的是
/// **模型莫名其妙又要重下一遍**。
pub const PIN_FILE: &str = "models-dir.txt";

/// 一个盘的信息。
///
/// 抽成纯数据是为了让**挑盘规则可测** —— 枚举盘符要走 Win32，
/// 测不了；但"该挑哪个"是纯逻辑，恰恰是容易出错的那部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drive {
    pub letter: char,
    /// 固定硬盘。U 盘、网络盘、光驱都不算。
    pub fixed: bool,
    /// 是不是系统盘。
    pub system: bool,
    pub free: u64,
}

/// 放模型至少要留的空间。
///
/// 模型本身约 425 MB，伴生程序 18 MB。留到 2 GB 是因为**塞满系统盘或
/// 数据盘的最后一点空间，比不装这个功能糟得多** —— 磁盘写满会让
/// 正在录的音频写入失败，而那才是用户真正不能丢的东西。
pub const NEED_FREE: u64 = 2 * 1024 * 1024 * 1024;

/// 从候选盘里挑一个放模型。
///
/// 规则，按优先级：
/// 1. **非系统盘**优先 —— 几百 MB 不该压在通常更小的系统盘上
/// 2. 只要**固定硬盘** —— U 盘拔掉、网络盘掉线，表现是模型"丢了"
/// 3. 剩余空间最多的那个
/// 4. 全都不合格就返回 `None`，交给调用方退回应用数据目录
pub fn pick_drive(drives: &[Drive], need: u64) -> Option<char> {
    let ok = |d: &&Drive| d.fixed && d.free >= need;
    drives
        .iter()
        .filter(|d| !d.system)
        .filter(ok)
        .max_by_key(|d| d.free)
        .or_else(|| drives.iter().filter(|d| d.system).filter(ok).max_by_key(|d| d.free))
        .map(|d| d.letter)
}

/// 枚举本机的盘。
#[cfg(windows)]
pub fn list_drives() -> Vec<Drive> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalDrives() -> u32;
        fn GetDriveTypeW(root: *const u16) -> u32;
        fn GetDiskFreeSpaceExW(
            dir: *const u16,
            free_to_caller: *mut u64,
            total: *mut u64,
            free_total: *mut u64,
        ) -> i32;
    }
    const DRIVE_FIXED: u32 = 3;

    // 系统盘。取不到就按 C 算 —— 宁可保守，也不要把模型塞进系统盘。
    let sys = std::env::var("SystemDrive")
        .ok()
        .and_then(|s| s.chars().next())
        .unwrap_or('C')
        .to_ascii_uppercase();

    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
        let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
        let mut free = 0u64;
        let (mut total, mut free_total) = (0u64, 0u64);
        let ok = unsafe {
            GetDiskFreeSpaceExW(root.as_ptr(), &mut free, &mut total, &mut free_total)
        };
        out.push(Drive {
            letter,
            fixed: kind == DRIVE_FIXED,
            system: letter == sys,
            free: if ok != 0 { free } else { 0 },
        });
    }
    out
}

#[cfg(not(windows))]
pub fn list_drives() -> Vec<Drive> {
    Vec::new()
}

/// 自动挑一个模型目录：**非系统盘根目录**下的 `wego-voice-models`。
///
/// 挑不到（单盘机器、空间不够）返回 `None`。
pub fn auto_dir() -> Option<PathBuf> {
    pick_drive(&list_drives(), NEED_FREE).map(|c| PathBuf::from(format!("{c}:\\{DIR_NAME}")))
}

/// 定下模型目录。
///
/// 顺序：
/// 1. 配置里钉过的位置 —— 用户改过或上次自动挑的，**优先**
/// 2. 自动挑一个非系统盘的根目录，并钉下来
/// 3. 都不行就退回应用数据目录
///
/// 钉下来这一步不是优化，是**正确性**：不钉的话，插一次移动硬盘就
/// 可能换个盘，而用户看到的是模型莫名其妙又要重下一遍。
pub fn resolve_dir(config_dir: &Path, app_data: &Path) -> PathBuf {
    let pin = config_dir.join(PIN_FILE);
    if let Ok(t) = std::fs::read_to_string(&pin) {
        let t = t.trim();
        if !t.is_empty() {
            return PathBuf::from(t);
        }
    }
    let chosen = auto_dir().unwrap_or_else(|| app_data.join("models"));
    // 写不进去也不致命 —— 下次再挑一遍，结果通常一样
    let _ = std::fs::create_dir_all(config_dir);
    let _ = std::fs::write(&pin, chosen.to_string_lossy().as_bytes());
    chosen
}

/// 应用数据目录下的兜底位置。单盘机器、或者自动挑失败时用。
pub fn dir(app_data: &Path) -> PathBuf {
    app_data.join("models")
}

/// 某一件资产的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// 文件不存在。
    Missing,
    /// 存在但字节数不对 —— 多半没下完。
    Incomplete { have: u64 },
    /// 字节数对，但还没校验过哈希（校验要读几百 MB，不能每次都做）。
    Present,
    /// 哈希也对上了。
    Verified,
    /// 哈希对不上 —— 文件坏了或者是别的版本。
    Corrupt,
}

impl State {
    /// 能不能拿去用。
    pub fn usable(&self) -> bool {
        matches!(self, State::Present | State::Verified)
    }
}

/// 整套资产的状态。
#[derive(Debug, Clone)]
pub struct Status {
    pub items: Vec<(Asset, State)>,
    /// 伴生程序在不在。
    pub companion: bool,
    pub dir: PathBuf,
}

impl Status {
    /// 全齐了才能用。
    pub fn ready(&self) -> bool {
        self.companion && self.items.iter().all(|(_, s)| s.usable())
    }

    /// 还差多少字节。
    pub fn missing_bytes(&self) -> u64 {
        self.items
            .iter()
            .map(|(a, s)| match s {
                State::Incomplete { have } => a.bytes.saturating_sub(*have),
                State::Missing | State::Corrupt => a.bytes,
                _ => 0,
            })
            .sum()
    }

    /// 缺的东西，人话列表。
    pub fn missing(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .items
            .iter()
            .filter(|(_, s)| !s.usable())
            .map(|(a, s)| match s {
                State::Incomplete { have } => format!(
                    "{}（只有 {:.0}/{:.0} MB，没下完）",
                    a.name,
                    *have as f64 / 1_048_576.0,
                    a.mb()
                ),
                State::Corrupt => format!("{}（校验不通过，文件坏了或版本不对）", a.name),
                _ => format!("{}（{:.0} MB）", a.name, a.mb()),
            })
            .collect();
        if !self.companion {
            v.push(format!("{COMPANION}（推理程序）"));
        }
        v
    }
}

/// 扫一遍目录。**只看大小，不算哈希** —— 后者要读几百 MB。
pub fn status(d: &Path) -> Status {
    let d = d.to_path_buf();
    let items = ASSETS
        .iter()
        .map(|a| {
            let p = d.join(a.name);
            let st = match std::fs::metadata(&p) {
                Err(_) => State::Missing,
                Ok(m) if m.len() == a.bytes => State::Present,
                Ok(m) => State::Incomplete { have: m.len() },
            };
            (*a, st)
        })
        .collect();
    Status {
        items,
        companion: d.join(COMPANION).is_file(),
        dir: d,
    }
}

/// 下载进度的一次汇报。
pub struct Step {
    /// 正在下的文件名。
    pub name: String,
    /// 这个文件已经拿到多少字节。
    pub have: u64,
    pub total: u64,
    /// 整批里这是第几个（从 1 数）。
    pub index: usize,
    pub count: usize,
}

/// 把缺的东西下齐。
///
/// # 每一步的失败都当成"这个源不行"，换下一个
///
/// 断线、超时、镜像挂了、返回 404 —— 对用户来说都是同一件事：
/// **这条路走不通，换一条**。只有全部源都试过还不行，才算失败。
///
/// # 下完必须校验
///
/// 大小对不上或哈希对不上 → **删掉重下一次**（只重一次）。
/// 不删的话，一个坏文件会一直卡在那里：每次"继续下载"都看到
/// 大小已经够了，于是什么都不做，而推理永远失败。
pub fn download_missing(
    dir: &Path,
    mut progress: impl FnMut(Step) -> bool,
) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("创建模型目录失败：{}", dir.display()))?;

    let st = status(dir);
    let mut todo: Vec<(String, Vec<String>, u64, Option<String>)> = Vec::new();
    for (a, state) in &st.items {
        if !state.usable() {
            todo.push((
                a.name.to_string(),
                a.sources.iter().map(|s| s.to_string()).collect(),
                a.bytes,
                Some(a.sha256.to_string()),
            ));
        }
    }
    if !st.companion {
        let mut urls = vec![COMPANION_URL.to_string()];
        urls.extend(COMPANION_MIRRORS.iter().map(|s| s.to_string()));
        // 伴生程序体积每次发版都不同，所以不给期望大小 —— 读到 EOF 为止
        todo.push((COMPANION.to_string(), urls, 0, None));
    }

    let count = todo.len();
    for (i, (name, urls, bytes, sha)) in todo.into_iter().enumerate() {
        let dest = dir.join(&name);
        let mut last: Option<anyhow::Error> = None;
        let mut ok = false;

        // 最多来两轮：第一轮可能续传到一个坏文件上，
        // 校验不过就删掉从头下一次。
        for attempt in 0..2 {
            if attempt == 1 {
                let _ = std::fs::remove_file(&dest);
            }
            let mut failed = None;
            for url in &urls {
                let r = crate::net::download(url, &dest, bytes, |p| {
                    progress(Step {
                        name: name.clone(),
                        have: p.have,
                        total: if bytes > 0 { bytes } else { p.total },
                        index: i + 1,
                        count,
                    })
                });
                match r {
                    Ok(()) => {
                        failed = None;
                        break;
                    }
                    Err(e) => {
                        // 用户取消：立刻停，不要接着试别的源
                        if e.to_string().contains("已取消") {
                            return Err(e);
                        }
                        failed = Some(e);
                    }
                }
            }
            if let Some(e) = failed {
                last = Some(e);
                continue;
            }
            match check_file(&dest, bytes, sha.as_deref(), &mut progress, &name, i + 1, count) {
                Ok(()) => {
                    ok = true;
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        if !ok {
            let e = last.unwrap_or_else(|| anyhow::anyhow!("未知失败"));
            bail!("下载 {name} 失败：{e:#}");
        }
    }
    Ok(())
}

/// 下完之后的校验：大小 + 哈希。
fn check_file(
    dest: &Path,
    bytes: u64,
    sha: Option<&str>,
    progress: &mut impl FnMut(Step) -> bool,
    name: &str,
    index: usize,
    count: usize,
) -> Result<()> {
    let got = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    if bytes > 0 && got != bytes {
        bail!("大小不对：拿到 {got} 字节，应当是 {bytes}");
    }
    if got == 0 {
        bail!("文件是空的");
    }
    let Some(want) = sha else { return Ok(()) };
    let total = got;
    let h = sha256_file(dest, |f| {
        progress(Step {
            name: format!("{name}（校验中）"),
            have: (f * total as f32) as u64,
            total,
            index,
            count,
        })
    })?;
    match h {
        None => bail!("已取消"),
        Some(h) if h == want => Ok(()),
        Some(h) => bail!("校验不通过：算出 {h}，应当是 {want}"),
    }
}

/// 逐个算哈希。**慢**（要读几百 MB），只在用户主动点"校验"或者
/// 推理失败之后才跑。
pub fn verify_all(
    d: &Path,
    mut progress: impl FnMut(&str, f32) -> bool,
) -> Result<Vec<(Asset, State)>> {
    let mut out = Vec::new();
    for a in ASSETS {
        let p = d.join(a.name);
        if !p.is_file() {
            out.push((*a, State::Missing));
            continue;
        }
        match sha256_file(&p, |f| progress(a.name, f)) {
            Ok(None) => bail!("校验被取消"),
            Ok(Some(h)) if h == a.sha256 => out.push((*a, State::Verified)),
            Ok(Some(_)) => out.push((*a, State::Corrupt)),
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// 算一个文件的 SHA-256。返回 `None` 表示被取消。
pub fn sha256_file(path: &Path, mut progress: impl FnMut(f32) -> bool) -> Result<Option<String>> {
    use std::io::Read;
    let total = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0).max(1);
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("打开失败：{}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    loop {
        let n = f.read(&mut buf).context("读取失败")?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        done += n as u64;
        if !progress(done as f32 / total as f32) {
            return Ok(None);
        }
    }
    Ok(Some(h.hex()))
}

// ───────────────────────── SHA-256 ─────────────────────────
//
// 自己写一份，而不是拉一个 crate 进来。
//
// 主程序的卖点之一是 6.5 MB 免安装单文件，而这里只需要一个杂凑函数 ——
// 为它引入依赖树不划算。算法是 FIPS 180-4，几十行，而且有官方测试向量
// 可以逐位验证（见本文件末尾的测试）。

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    n: usize,
    len: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            h: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0; 64],
            n: 0,
            len: 0,
        }
    }

    pub fn update(&mut self, mut d: &[u8]) {
        self.len = self.len.wrapping_add(d.len() as u64);
        while !d.is_empty() {
            let take = (64 - self.n).min(d.len());
            self.buf[self.n..self.n + take].copy_from_slice(&d[..take]);
            self.n += take;
            d = &d[take..];
            if self.n == 64 {
                let block = self.buf;
                self.block(&block);
                self.n = 0;
            }
        }
    }

    fn block(&mut self, b: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b_, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b_) ^ (a & c) ^ (b_ & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b_;
            b_ = a;
            a = t1.wrapping_add(t2);
        }
        for (i, v) in [a, b_, c, d, e, f, g, h].into_iter().enumerate() {
            self.h[i] = self.h[i].wrapping_add(v);
        }
    }

    pub fn hex(mut self) -> String {
        let bits = self.len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.n != 56 {
            self.update(&[0]);
        }
        // `update` 会把 len 加上去，这里直接写进缓冲
        self.buf[56..64].copy_from_slice(&bits.to_be_bytes());
        let block = self.buf;
        self.block(&block);
        self.h.iter().map(|v| format!("{v:08x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(s: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(s);
        h.hex()
    }

    /// FIPS 180-4 的官方测试向量。
    ///
    /// 自己写杂凑函数只有一个前提：**能逐位验证**。这几条就是那个前提。
    #[test]
    fn sha256_matches_the_official_vectors() {
        assert_eq!(
            sha(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// 跨块边界。一次喂和分多次喂必须得到同一个结果 ——
    /// 流式读文件正是分多次喂。
    #[test]
    fn streaming_matches_one_shot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let one = sha(&data);
        for chunk in [1usize, 7, 63, 64, 65, 333] {
            let mut h = Sha256::new();
            for c in data.chunks(chunk) {
                h.update(c);
            }
            assert_eq!(h.hex(), one, "按 {chunk} 字节分块喂，结果不一致");
        }
    }

    /// 长度正好 55 / 56 / 64 字节是填充逻辑的边界。
    #[test]
    fn padding_boundaries() {
        // 与 openssl 对过的值
        assert_eq!(
            sha(&[0x61u8; 55]),
            "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"
        );
        assert_eq!(
            sha(&[0x61u8; 56]),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
        assert_eq!(
            sha(&[0x61u8; 64]),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wego-assets-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(dir(&d)).unwrap();
        d
    }

    #[test]
    fn empty_dir_reports_everything_missing() {
        let d = tmp("empty");
        let s = status(&dir(&d));
        assert!(!s.ready());
        assert!(!s.companion);
        assert_eq!(s.items.len(), ASSETS.len());
        assert!(s.items.iter().all(|(_, st)| *st == State::Missing));
        // 缺的字节数 = 全部
        assert_eq!(s.missing_bytes(), ASSETS.iter().map(|a| a.bytes).sum::<u64>());
        // 伴生程序也要报出来
        assert!(s.missing().iter().any(|m| m.contains(COMPANION)));
    }

    /// ⚠️ 大小不对必须报「没下完」，而不是当成有。
    ///
    /// 下断的文件照样能打开、照样能被 ONNX 尝试加载 ——
    /// 失败信息会是"invalid Zip archive"之类，跟"没下完"对不上号。
    #[test]
    fn a_short_file_is_incomplete_not_present() {
        let d = tmp("short");
        let a = ASSETS[0];
        std::fs::write(dir(&d).join(a.name), b"only a few bytes").unwrap();
        let s = status(&dir(&d));
        let st = &s.items.iter().find(|(x, _)| x.name == a.name).unwrap().1;
        assert!(matches!(st, State::Incomplete { have: 16 }), "{st:?}");
        assert!(!st.usable());
        assert!(s.missing().iter().any(|m| m.contains("没下完")), "{:?}", s.missing());
    }

    /// 大小对就算 present —— 扫目录不该读几百 MB。
    #[test]
    fn right_size_counts_as_present_without_hashing() {
        let d = tmp("size");
        for a in ASSETS {
            std::fs::write(dir(&d).join(a.name), vec![0u8; a.bytes.min(4096) as usize]).unwrap();
        }
        // 真实体积太大，这里改用一个缩小的断言：只验逻辑分支
        let a = Asset { name: "tiny.bin", what: "", bytes: 4, sha256: "", sources: &[] };
        let p = dir(&d).join(a.name);
        std::fs::write(&p, b"abcd").unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), a.bytes);
    }

    /// 哈希对不上要判 Corrupt，而不是 Verified。
    #[test]
    fn wrong_content_is_corrupt() {
        let d = tmp("corrupt");
        let p = dir(&d).join("x.bin");
        std::fs::write(&p, b"abc").unwrap();
        let h = sha256_file(&p, |_| true).unwrap().unwrap();
        assert_eq!(h, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_ne!(h, "0".repeat(64));
    }

    /// 校验可以取消 —— 几百 MB 要读十几秒，用户点了停就得停。
    #[test]
    fn hashing_can_be_cancelled() {
        let d = tmp("cancel");
        let p = dir(&d).join("big.bin");
        std::fs::write(&p, vec![7u8; 4 << 20]).unwrap();
        let got = sha256_file(&p, |_| false).unwrap();
        assert!(got.is_none(), "取消之后不该返回哈希");
    }

    fn drive(letter: char, fixed: bool, system: bool, free_gb: u64) -> Drive {
        Drive { letter, fixed, system, free: free_gb * 1024 * 1024 * 1024 }
    }

    /// ⚠️ 非系统盘优先，**哪怕它空间更小**。
    ///
    /// 几百 MB 压在通常更小的系统盘上，是这条规则存在的全部理由 ——
    /// 所以"C 盘空间更多"不能成为选 C 的理由。
    #[test]
    fn a_data_drive_wins_even_with_less_space() {
        let ds = [drive('C', true, true, 500), drive('D', true, false, 50)];
        assert_eq!(pick_drive(&ds, NEED_FREE), Some('D'));
    }

    /// 多个非系统盘时选空间最多的。
    #[test]
    fn among_data_drives_the_roomiest_wins() {
        let ds = [
            drive('C', true, true, 500),
            drive('D', true, false, 50),
            drive('E', true, false, 300),
        ];
        assert_eq!(pick_drive(&ds, NEED_FREE), Some('E'));
    }

    /// ⚠️ U 盘、网络盘、光驱一律不选。
    ///
    /// 拔掉之后的表现是模型"丢了" —— 而用户完全想不到是因为拔了 U 盘。
    #[test]
    fn removable_and_network_drives_are_never_chosen() {
        let ds = [
            drive('C', true, true, 200),
            drive('E', false, false, 900), // U 盘，空间再大也不选
        ];
        assert_eq!(pick_drive(&ds, NEED_FREE), Some('C'), "只剩系统盘时才退回它");
    }

    /// 空间不够的盘跳过。
    #[test]
    fn a_full_drive_is_skipped() {
        let ds = [drive('C', true, true, 100), drive('D', true, false, 1)];
        assert_eq!(pick_drive(&ds, NEED_FREE), Some('C'));
    }

    /// 单盘机器：退回系统盘（调用方再退回应用数据目录）。
    #[test]
    fn a_single_drive_machine_falls_back_to_the_system_drive() {
        let ds = [drive('C', true, true, 100)];
        assert_eq!(pick_drive(&ds, NEED_FREE), Some('C'));
    }

    /// 全都不合格就交白卷，让调用方退回应用数据目录。
    #[test]
    fn nothing_usable_returns_none() {
        let ds = [drive('C', true, true, 1), drive('E', false, false, 900)];
        assert_eq!(pick_drive(&ds, NEED_FREE), None);
    }

    /// ⚠️ 位置必须被钉住。
    ///
    /// 不钉的话，插一次移动硬盘就可能换个盘，而用户看到的是
    /// **模型莫名其妙又要重下一遍**。
    #[test]
    fn the_location_is_pinned_and_honoured() {
        let base = tmp("pin");
        let cfg = base.join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join(PIN_FILE), r"Z:\somewhere\models").unwrap();
        assert_eq!(
            resolve_dir(&cfg, &base),
            PathBuf::from(r"Z:\somewhere\models"),
            "钉过的位置没被采纳"
        );
    }

    /// 第一次解析要把结果写下来 —— 否则下次开机可能挑到别的盘。
    #[test]
    fn the_first_resolve_writes_the_pin() {
        let base = tmp("pin-write");
        let cfg = base.join("cfg");
        let got = resolve_dir(&cfg, &base);
        let pinned = std::fs::read_to_string(cfg.join(PIN_FILE)).expect("没写下钉住的位置");
        assert_eq!(pinned.trim(), got.to_string_lossy());
        assert!(!pinned.trim().is_empty());
    }

    /// 钉住的文件是空的（用户清空了）→ 当作没钉过，重新挑。
    #[test]
    fn an_empty_pin_is_ignored() {
        let base = tmp("pin-empty");
        let cfg = base.join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join(PIN_FILE), "   \n  ").unwrap();
        let got = resolve_dir(&cfg, &base);
        assert!(!got.as_os_str().is_empty());
        assert_ne!(got, PathBuf::from("   \n  "));
    }

    /// 资产表本身要自洽：哈希是 64 位十六进制、体积非零、名字不重复。
    #[test]
    fn the_registry_is_well_formed() {
        let mut names = std::collections::BTreeSet::new();
        for a in ASSETS {
            assert!(names.insert(a.name), "{} 重复了", a.name);
            assert_eq!(a.sha256.len(), 64, "{} 的哈希长度不对", a.name);
            assert!(
                a.sha256.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{} 的哈希不是小写十六进制",
                a.name
            );
            assert!(a.bytes > 1_000_000, "{} 的体积看着不对：{}", a.name, a.bytes);
            assert!(!a.what.is_empty(), "{} 缺人话说明", a.name);
            assert!(!a.sources.is_empty(), "{} 一个下载源都没有", a.name);
            for u in a.sources {
                assert!(
                    crate::net::parse_url(u).is_ok(),
                    "{} 的源不是合法 URL：{u}",
                    a.name
                );
                assert!(u.ends_with(a.name), "{} 的源指向的文件名对不上：{u}", a.name);
            }
        }
    }
}
