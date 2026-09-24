//! ONNX 会话的建立与**自检**。
//!
//! # 为什么建会话这件事值得单独一个模块
//!
//! 模型是按需下载的几百 MB 文件，放在用户机器上。可能的失败一大把：
//! 下到一半、下错版本、被杀毒软件动过、磁盘满、路径里有中文。
//!
//! 这些失败如果只在推理时以 `Err` 冒出来，用户看到的是"转换失败"，
//! 而真正的原因是文件少了 3 MB。**所以建会话时就把模型的形状问一遍，
//! 对不上立刻报出人话**，不要拖到跑完才说。

use anyhow::{bail, Result};
use std::path::Path;

/// 一个模型的输入/输出契约。
///
/// 图里的名字和维度是我们与模型之间唯一的约定。写死在代码里靠"应该是这样"
/// 是行不通的 —— 换一个来源的导出件，名字就可能从 `source` 变成 `input`。
#[derive(Debug, Clone, PartialEq)]
pub struct Contract {
    pub inputs: Vec<Port>,
    pub outputs: Vec<Port>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Port {
    pub name: String,
    /// 每一维；`None` 表示动态维（batch、时长）。
    pub dims: Vec<Option<i64>>,
}

impl Port {
    /// 维数（含动态维）。
    pub fn rank(&self) -> usize {
        self.dims.len()
    }

    /// 人类可读，用于报错：`[?, 1, ?]`
    pub fn shape_str(&self) -> String {
        let s: Vec<String> = self
            .dims
            .iter()
            .map(|d| d.map(|v| v.to_string()).unwrap_or_else(|| "?".into()))
            .collect();
        format!("[{}]", s.join(", "))
    }
}

impl Contract {
    pub fn describe(&self) -> String {
        let mut s = String::new();
        for p in &self.inputs {
            s.push_str(&format!("  输入 {:<16} {}\n", p.name, p.shape_str()));
        }
        for p in &self.outputs {
            s.push_str(&format!("  输出 {:<16} {}\n", p.name, p.shape_str()));
        }
        s
    }
}

/// 打开一个 ONNX 模型，返回会话与它的契约。
///
/// 线程数**刻意限制**：默认会开满所有核。这个 crate 的调用方
/// （离线任务）已经被红线 3 挡在引擎运行之外，但即便如此，
/// 占满所有核会让界面本身卡顿 —— 用户会以为程序死了。
pub fn open(path: &Path, threads: usize) -> Result<(ort::session::Session, Contract)> {
    if !path.exists() {
        bail!(
            "模型文件不存在：{}\n（神经声线转换需要按需下载模型，请先在界面上下载）",
            path.display()
        );
    }
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size < 1024 * 1024 {
        bail!(
            "模型文件只有 {} 字节，多半没下完：{}",
            size,
            path.display()
        );
    }

    // ort 的错误类型带泛型参数，用不了 anyhow 的 `.context()`，逐个转一下
    let session = ort::session::Session::builder()
        .map_err(|e| anyhow::anyhow!("创建 ONNX 会话构建器失败：{e}"))?
        .with_intra_threads(threads.max(1))
        .map_err(|e| anyhow::anyhow!("设置线程数失败：{e}"))?
        .commit_from_file(path)
        .map_err(|e| anyhow::anyhow!("加载模型失败：{}（{e}）", path.display()))?;

    let contract = contract_of(&session);
    Ok((session, contract))
}

fn contract_of(session: &ort::session::Session) -> Contract {
    let port = |o: &ort::value::Outlet| Port {
        name: o.name().to_string(),
        // 动态维在 ort 里是 -1
        dims: match o.dtype() {
            ort::value::ValueType::Tensor { shape, .. } => shape
                .iter()
                .map(|d| if *d < 0 { None } else { Some(*d) })
                .collect(),
            _ => Vec::new(),
        },
    };
    Contract {
        inputs: session.inputs().iter().map(port).collect(),
        outputs: session.outputs().iter().map(port).collect(),
    }
}

/// 指定 onnxruntime.dll 的位置。**必须在建任何会话之前调用一次。**
///
/// 运行时动态加载是刻意的：DLL（约 16 MB）随模型一起按需下载，
/// 主程序体积不受影响 —— 不用神经功能的人一个字节都不必付。
pub fn init(dylib: &Path) -> Result<()> {
    if !dylib.exists() {
        bail!(
            "找不到 onnxruntime.dll：{}\n（它随模型一起下载，请先在界面上完成下载）",
            dylib.display()
        );
    }
    // `commit()` 返回 bool：true = 这次调用装上了全局环境，
    // false = 之前已经装过了。后者不是错误 —— 一个进程只需要一个环境。
    ort::init_from(dylib)
        .map_err(|e| anyhow::anyhow!("加载 onnxruntime.dll 失败：{e}"))?
        .commit();
    Ok(())
}

/// 用 tract（纯 Rust 推理引擎）作为后端。
///
/// # 为什么值得试
///
/// `ort` 从 rc.10 起支持替代后端：`alternative-backend` 让它不去链接
/// 微软的实现，改用注入进来的 C API。`ort-tract` 就是拿 tract 实现了同一套
/// 接口 —— **调用代码一行不改**，只是启动时换一句初始化。
///
/// 对这个产品的意义是能删掉整个 onnxruntime.dll：
/// 15.7 MB 的下载、动态加载那一套、"DLL 找不到"这一类失败、
/// 以及杀毒软件对一个下载来的 DLL 的怀疑 —— 全都不存在了。
///
/// ⚠️ 代价要实测，不能假设：tract 的算子覆盖不如 ONNX Runtime 全，
/// 而且它是静态链接进 exe 的，体积算在**所有人**头上，不只是用克隆的人。
#[cfg(feature = "tract")]
pub fn init_pure_rust() {
    ort::set_api(ort_tract::api());
}

/// 契约检查：维数与已知的固定维必须对上。
///
/// 只检查**维数**和**非动态维**，不检查名字 —— 名字随导出工具变，
/// 而形状是模型真正的约定。名字由调用方按序取。
pub fn expect_shape(port: &Port, want: &[Option<i64>], what: &str) -> Result<()> {
    if port.rank() != want.len() {
        bail!(
            "{what} 的维数不对：模型是 {} 维 {}，预期 {} 维。\n\
             多半是下错了模型文件。",
            port.rank(),
            port.shape_str(),
            want.len()
        );
    }
    for (i, (got, exp)) in port.dims.iter().zip(want).enumerate() {
        if let (Some(g), Some(e)) = (got, exp) {
            if g != e {
                bail!(
                    "{what} 的第 {i} 维是 {g}，预期 {e}（完整形状 {}）。\n\
                     多半是下错了模型文件。",
                    port.shape_str()
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(dims: Vec<Option<i64>>) -> Port {
        Port { name: "x".into(), dims }
    }

    #[test]
    fn shape_check_accepts_dynamic_dims() {
        // 模型是动态的、我们期望固定值 —— 放行（运行时才知道）
        assert!(expect_shape(&port(vec![None, None, Some(768)]), &[None, None, Some(768)], "t").is_ok());
        // 我们不关心的维给 None —— 放行
        assert!(expect_shape(&port(vec![Some(1), Some(2)]), &[None, None], "t").is_ok());
    }

    #[test]
    fn shape_check_catches_the_wrong_model() {
        let e = expect_shape(&port(vec![None, Some(256)]), &[None, Some(768)], "内容特征")
            .unwrap_err()
            .to_string();
        assert!(e.contains("256"), "{e}");
        assert!(e.contains("下错了模型"), "报错没给出人话：{e}");

        let e = expect_shape(&port(vec![None, None]), &[None, None, None], "内容特征")
            .unwrap_err()
            .to_string();
        assert!(e.contains("维数不对"), "{e}");
    }

    #[test]
    fn shape_str_marks_dynamic_dims() {
        assert_eq!(port(vec![None, Some(1), Some(768)]).shape_str(), "[?, 1, 768]");
    }

    #[test]
    fn missing_file_says_what_to_do() {
        let e = open(Path::new("definitely-not-here.onnx"), 1).unwrap_err().to_string();
        assert!(e.contains("按需下载"), "缺文件时没说该怎么办：{e}");
    }
}
