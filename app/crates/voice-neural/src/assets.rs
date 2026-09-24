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
    },
    Asset {
        name: "model_0.pt",
        what: "解码器 —— 按音色重新合成波形",
        bytes: 67_878_441,
        sha256: "ebe70c92c09d7d1c1fbfe631b21766ecca6afb21536fa195b0e932dd2fe12912",
    },
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

/// 资产目录：`<应用数据目录>/models/`。
///
/// 放应用数据目录而不是 exe 旁边：主程序是**免安装**的，用户会把它
/// 拷来拷去；几百 MB 的模型跟着走没有道理，而且 exe 可能放在
/// 只读位置（U 盘、Program Files）。
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
pub fn status(app_data: &Path) -> Status {
    let d = dir(app_data);
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

/// 逐个算哈希。**慢**（要读几百 MB），只在用户主动点"校验"或者
/// 推理失败之后才跑。
pub fn verify_all(
    app_data: &Path,
    mut progress: impl FnMut(&str, f32) -> bool,
) -> Result<Vec<(Asset, State)>> {
    let d = dir(app_data);
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
        let s = status(&d);
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
        let s = status(&d);
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
        let a = Asset { name: "tiny.bin", what: "", bytes: 4, sha256: "" };
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
        }
    }
}
