//! 解码器网络（`Unit2Control`）—— DDSP 解码器里唯一带权重的部分。
//!
//! # 它吐的不是波形
//!
//! 这是 DDSP 路线的核心：网络吐的是**一个经典合成器的参数**，
//! 具体说是每帧两组复数滤波器（谐波的与噪声的）。波形由
//! `source.rs`（激励）与 `synth.rs`（滤波+重建）合成出来。
//!
//! 好处是网络小得多 —— 它不用学"怎么造波形"，只要学"该怎么滤"。
//!
//! # 为什么这一半用 candle，而内容编码器用 ONNX
//!
//! 内容编码器**冻结、永不训练**，用现成的 ONNX 导出件即可。
//!
//! 这一半不一样：它要按用户的素材训练，而 `ort` 是纯推理、没有梯度。
//! 所以它走 candle，**推理与训练共用同一份实现** ——
//! 同一个模型写两遍，两份只要有一处对不上，表现就是"能跑但不像"，
//! 而那种 bug 没有参考实现在手根本没法二分。
//!
//! # 结构（移植自 DDSP-SVC，`conv_only=True` 配置）
//!
//! ```text
//! units[768] ─→ Conv1d(768→256,k3) → GroupNorm(4) → LeakyReLU → Conv1d(256→256,k3)
//!                     ↓ 加上四路条件嵌入
//!        log(1+f0/700) → Linear(1→256)
//!            phase/π   → Linear(1→256)
//!            volume    → Linear(1→256)
//!            spk_id    → Embedding(n_spk,256)
//!                     ↓
//!              3 × [ x = x + ConvModule(x) ]
//!                     ↓
//!              LayerNorm(256) → Linear(256→4×bins)
//!                     ↓
//!   谐波幅度 | 谐波相位 | 噪声幅度 | 噪声相位
//! ```
//!
//! ⚠️ **这一档配置里没有注意力。** `CFNEncoderLayer` 在 `conv_only=True`
//! 时整个跳过 attention 分支，每层就是一个残差卷积块。
//! 照着"Conformer"这个名字去实现多头注意力，是会白写一大块还对不上权重的。

use anyhow::{bail, Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::{
    conv1d, group_norm, layer_norm, linear, Conv1d, Conv1dConfig, GroupNorm, LayerNorm,
    LayerNormConfig, Linear, Module, VarBuilder,
};

/// 深度可分离卷积的核长。移植自参考实现，改了就对不上权重。
const CONV_KERNEL: usize = 31;
/// 卷积模块内部的扩张倍数。
const EXPANSION: usize = 2;
/// 隐藏维度。
pub const DIM: usize = 256;
/// 编码层数。
const LAYERS: usize = 3;

/// 一个残差卷积块（参考实现的 `ConformerConvModule`）。
struct ConvModule {
    /// 逐点卷积，升到 `2 × inner`，给 GLU 用
    pw_in: Conv1d,
    /// 深度卷积（`groups == inner`）
    dw: Conv1d,
    /// 逐点卷积，降回 `dim`
    pw_out: Conv1d,
}

impl ConvModule {
    fn load(vb: VarBuilder, dim: usize) -> Result<Self> {
        let inner = dim * EXPANSION;
        // 参考实现是 `nn.Sequential`，权重名就是序号。
        // `use_norm=False` 时第 0 位是 `Identity`，第 1 位是 `Transpose`，
        // 两者都没有参数 —— 所以带权重的是 2 / 4 / 6。
        let pw_in = conv1d(dim, inner * 2, 1, Conv1dConfig::default(), vb.pp("net.2"))
            .context("加载 net.2（逐点升维）失败")?;
        let dw = conv1d(
            inner,
            inner,
            CONV_KERNEL,
            Conv1dConfig {
                padding: CONV_KERNEL / 2,
                groups: inner,
                ..Default::default()
            },
            vb.pp("net.4"),
        )
        .context("加载 net.4（深度卷积）失败")?;
        let pw_out = conv1d(inner, dim, 1, Conv1dConfig::default(), vb.pp("net.6"))
            .context("加载 net.6（逐点降维）失败")?;
        Ok(Self { pw_in, dw, pw_out })
    }

    /// 输入输出都是 `[B, T, C]`；内部转成 `[B, C, T]` 做卷积。
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let h = x.transpose(1, 2)?; // [B, C, T]
        let h = self.pw_in.forward(&h)?;
        let h = glu(&h, 1)?;
        let h = self.dw.forward(&h)?;
        let h = silu(&h)?;
        let h = self.pw_out.forward(&h)?;
        Ok(h.transpose(1, 2)?)
    }
}

/// 门控线性单元：沿 `dim` 对半劈开，后半过 sigmoid 当门。
fn glu(x: &Tensor, dim: usize) -> Result<Tensor> {
    let n = x.dim(dim)?;
    if n % 2 != 0 {
        bail!("GLU 要求该维是偶数，实际 {n}");
    }
    let a = x.narrow(dim, 0, n / 2)?;
    let b = x.narrow(dim, n / 2, n / 2)?;
    Ok((a * candle_nn::ops::sigmoid(&b)?)?)
}

fn silu(x: &Tensor) -> Result<Tensor> {
    Ok((x * candle_nn::ops::sigmoid(x)?)?)
}

/// `x + ConvModule(x)`。
///
/// ⚠️ 参考实现在 `conv_only=True` 时**完全跳过** attention 分支 ——
/// `self.attn` 是 `None`，那一行根本不执行。
struct EncoderLayer {
    conv: ConvModule,
}

impl EncoderLayer {
    fn load(vb: VarBuilder, dim: usize) -> Result<Self> {
        Ok(Self {
            conv: ConvModule::load(vb.pp("conformer"), dim)?,
        })
    }
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        Ok((x + self.conv.forward(x)?)?)
    }
}

/// 解码器网络。
pub struct Unit2Control {
    stack_a: Conv1d,
    stack_norm: GroupNorm,
    stack_b: Conv1d,
    f0_embed: Linear,
    phase_embed: Linear,
    volume_embed: Linear,
    spk_embed: Option<candle_nn::Embedding>,
    layers: Vec<EncoderLayer>,
    norm: LayerNorm,
    dense_out: Linear,
    /// 每组滤波器的频点数（`win_length / 2 + 1`）。
    pub bins: usize,
    pub n_spk: usize,
}

/// 网络每帧吐出来的四组控制量。
///
/// `Debug` 只报形状：四组各 `frames × bins` 个浮点数，
/// 一次 `unwrap` 失败就能把终端刷爆，真正的错误反而被冲掉。
pub struct Controls {
    pub frames: usize,
    pub bins: usize,
    /// 对数幅度。
    pub harmonic_magnitude: Vec<f32>,
    /// 相位，单位是 π。
    pub harmonic_phase: Vec<f32>,
    pub noise_magnitude: Vec<f32>,
    pub noise_phase: Vec<f32>,
}

/// 一次前向的全部逐帧输入。
///
/// # 为什么要打个包
///
/// 这四路里有三路都是 `&[f32]`，摊成参数列表的话，
/// **调用处把 f0 和 volume 写反了编译器一声不吭** ——
/// 而它的表现是"能跑但声音怪"，正是这条链路上最难查的那类错。
///
/// 打成具名字段之后，写反就是写错字段名，编译期就拦住了。
pub struct Inputs<'a> {
    /// 行优先内容特征，`frames × dim`。
    pub units: &'a [f32],
    pub dim: usize,
    /// Hz。
    pub f0: &'a [f32],
    /// 弧度（网络内部会除以 π）。
    pub phase: &'a [f32],
    /// 线性 RMS。
    pub volume: &'a [f32],
}

impl Unit2Control {
    /// 从 PyTorch 检查点加载。
    pub fn load(vb: VarBuilder, n_unit: usize, n_spk: usize, bins: usize) -> Result<Self> {
        let stack_a = conv1d(
            n_unit,
            DIM,
            3,
            Conv1dConfig { padding: 1, ..Default::default() },
            vb.pp("stack.0"),
        )
        .context("加载 stack.0 失败 —— 多半是 n_unit 对不上（这里要 768）")?;
        // GroupNorm(4, 256)：4 组，每组 64 通道
        let stack_norm = group_norm(4, DIM, 1e-5, vb.pp("stack.1")).context("加载 stack.1 失败")?;
        let stack_b = conv1d(
            DIM,
            DIM,
            3,
            Conv1dConfig { padding: 1, ..Default::default() },
            vb.pp("stack.3"),
        )
        .context("加载 stack.3 失败")?;

        let f0_embed = linear(1, DIM, vb.pp("f0_embed")).context("加载 f0_embed 失败")?;
        let phase_embed = linear(1, DIM, vb.pp("phase_embed")).context("加载 phase_embed 失败")?;
        let volume_embed =
            linear(1, DIM, vb.pp("volume_embed")).context("加载 volume_embed 失败")?;

        let spk_embed = if n_spk > 1 {
            Some(
                candle_nn::embedding(n_spk, DIM, vb.pp("spk_embed"))
                    .context("加载 spk_embed 失败")?,
            )
        } else {
            None
        };

        let mut layers = Vec::with_capacity(LAYERS);
        for i in 0..LAYERS {
            layers.push(
                EncoderLayer::load(vb.pp(format!("decoder.encoder_layers.{i}")), DIM)
                    .with_context(|| format!("加载第 {i} 层失败"))?,
            );
        }

        let norm = layer_norm(DIM, LayerNormConfig::default(), vb.pp("norm"))
            .context("加载 norm 失败")?;
        let dense_out = load_weight_norm_linear(vb.pp("dense_out"), DIM, bins * 4)
            .context("加载 dense_out 失败")?;

        Ok(Self {
            stack_a,
            stack_norm,
            stack_b,
            f0_embed,
            phase_embed,
            volume_embed,
            spk_embed,
            layers,
            norm,
            dense_out,
            bins,
            n_spk,
        })
    }

    /// 跑一遍。
    ///
    /// `spk_id` 从 1 开始（与参考实现一致，它内部会减 1）。
    pub fn forward(&self, inp: &Inputs<'_>, spk_id: usize, device: &Device) -> Result<Controls> {
        let Inputs { units, dim, f0, phase, volume } = *inp;
        let frames = f0.len();
        if volume.len() != frames || phase.len() != frames {
            bail!(
                "各路长度不一致：f0={frames} phase={} volume={}",
                phase.len(),
                volume.len()
            );
        }
        if units.len() != frames * dim {
            bail!("内容特征长度 {} ≠ {frames}×{dim}", units.len());
        }

        let u = Tensor::from_slice(units, (1, frames, dim), device)?;
        // Conv1d 吃 [B, C, T]
        let h = u.transpose(1, 2)?;
        let h = self.stack_a.forward(&h)?;
        let h = self.stack_norm.forward(&h)?;
        let h = leaky_relu(&h, 0.01)?;
        let h = self.stack_b.forward(&h)?;
        let mut x = h.transpose(1, 2)?; // [B, T, 256]

        // 条件嵌入。f0 走 log(1 + f0/700) —— mel 式压缩，
        // 让低音区的分辨率高于高音区，和人耳一致。
        let f0c: Vec<f32> = f0.iter().map(|v| (1.0 + v / 700.0).ln()).collect();
        let phc: Vec<f32> = phase.iter().map(|v| v / std::f32::consts::PI).collect();

        let col = |v: &[f32]| -> Result<Tensor> {
            Ok(Tensor::from_slice(v, (1, frames, 1), device)?)
        };
        x = (x + self.f0_embed.forward(&col(&f0c)?)?)?;
        x = (x + self.phase_embed.forward(&col(&phc)?)?)?;
        x = (x + self.volume_embed.forward(&col(volume)?)?)?;

        if let Some(e) = &self.spk_embed {
            if spk_id == 0 || spk_id > self.n_spk {
                bail!("说话人编号 {spk_id} 越界（模型有 {} 个）", self.n_spk);
            }
            // 参考实现是 `spk_embed(spk_id - 1)`
            let ids = Tensor::from_slice(&[(spk_id - 1) as u32], (1, 1), device)?;
            let v = e.forward(&ids)?; // [1, 1, 256]
            x = x.broadcast_add(&v)?;
        }

        for l in &self.layers {
            x = l.forward(&x)?;
        }
        let x = self.norm.forward(&x)?;
        let e = self.dense_out.forward(&x)?; // [1, T, 4*bins]

        let take = |i: usize| -> Result<Vec<f32>> {
            let t = e.narrow(2, i * self.bins, self.bins)?.flatten_all()?;
            Ok(t.to_vec1::<f32>()?)
        };

        Ok(Controls {
            frames,
            bins: self.bins,
            harmonic_magnitude: take(0)?,
            harmonic_phase: take(1)?,
            noise_magnitude: take(2)?,
            noise_phase: take(3)?,
        })
    }
}

impl Controls {
    /// 把四组控制量变成两组逐帧复数滤波器。
    ///
    /// # 两个照抄的细节
    ///
    /// **噪声滤波器要除以 128。** 参考实现里写死的。少了这一下，
    /// 噪声成分会盖过谐波，听起来就是一片嘶声。
    ///
    /// **两组都要把最后一帧复制一份接在后面。** STFT 的 `center=true`
    /// 会比 block 帧多出一帧（`n_blocks + 1`），而网络只吐 `n_blocks` 帧。
    /// 不补的话最后一帧没有滤波器可用。
    pub fn filters(&self) -> (crate::synth::Spectrum, crate::synth::Spectrum) {
        let build = |mag: &[f32], phase: &[f32], scale: f32| -> crate::synth::Spectrum {
            let mut data = Vec::with_capacity((self.frames + 1) * self.bins);
            for t in 0..self.frames {
                let r = t * self.bins..(t + 1) * self.bins;
                data.extend(crate::synth::filter_from(&mag[r.clone()], &phase[r], scale));
            }
            // 末帧复制一份
            let last = self.frames.saturating_sub(1) * self.bins;
            let tail: Vec<_> = data[last..last + self.bins].to_vec();
            data.extend(tail);
            crate::synth::Spectrum { data, frames: self.frames + 1, bins: self.bins }
        };
        (
            build(&self.harmonic_magnitude, &self.harmonic_phase, 1.0),
            build(&self.noise_magnitude, &self.noise_phase, 1.0 / 128.0),
        )
    }
}

impl std::fmt::Debug for Controls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Controls {{ {} 帧 × {} 频点 × 4 组 }}", self.frames, self.bins)
    }
}

fn leaky_relu(x: &Tensor, slope: f64) -> Result<Tensor> {
    let zeros = x.zeros_like()?;
    let pos = x.maximum(&zeros)?;
    let neg = (x.minimum(&zeros)? * slope)?;
    Ok((pos + neg)?)
}

/// 加载一个被 `weight_norm` 包过的 `Linear`。
///
/// # 为什么要单独处理
///
/// `weight_norm` 不存 `weight`，而是存 `weight_g`（每行的模长）和
/// `weight_v`（方向）。实际权重是 `g · v / ‖v‖`。
///
/// 直接按 `weight` 去取会**找不到键**（这还算好的，至少会报错）；
/// 而如果哪天有人把 `weight_v` 当成 `weight` 用，模型能跑、输出的数
/// 也在正常范围 —— 只是每一行的尺度全错。那就是典型的"能跑但不像"。
///
/// PyTorch 2.1 之后 `nn.utils.parametrizations.weight_norm` 换了键名
/// （`parametrizations.weight.original0/1`），这里两种都认。
fn load_weight_norm_linear(vb: VarBuilder, in_dim: usize, out_dim: usize) -> Result<Linear> {
    let (g, v) = if vb.contains_tensor("weight_g") {
        (
            vb.get((out_dim, 1), "weight_g")?,
            vb.get((out_dim, in_dim), "weight_v")?,
        )
    } else if vb.contains_tensor("parametrizations.weight.original0") {
        (
            vb.get((out_dim, 1), "parametrizations.weight.original0")?,
            vb.get((out_dim, in_dim), "parametrizations.weight.original1")?,
        )
    } else if vb.contains_tensor("weight") {
        // 没包 weight_norm 的普通 Linear
        let w = vb.get((out_dim, in_dim), "weight")?;
        let b = vb.get(out_dim, "bias")?;
        return Ok(Linear::new(w, Some(b)));
    } else {
        bail!(
            "dense_out 里既没有 weight_g/weight_v，也没有 parametrizations.*，\
             也没有裸 weight —— 检查点结构不认识"
        );
    };

    // ‖v‖ 按行（dim=1）求，与 PyTorch 的 `dim=0` 语义一致：
    // 那里的 dim=0 指"保留第 0 维"，即对每个输出通道单独归一化。
    let norm = v.sqr()?.sum_keepdim(1)?.sqrt()?;
    let w = v.broadcast_div(&norm)?.broadcast_mul(&g)?;
    let b = vb.get(out_dim, "bias")?;
    Ok(Linear::new(w, Some(b)))
}

/// 本实现**期望**的全部参数名与形状。
///
/// # 它凭什么值得存在
///
/// "权重名跟代码里假设的对不上"是这类移植失败的大头，而症状是
/// **能跑但声音怪**，不是报错。把期望显式列出来之后，就能拿检查点跟它
/// 对一遍：缺了什么、多了什么、形状差在哪，一目了然 —— 而不是等音频
/// 出来再猜。
///
/// 顺带它也是这份移植的**可执行文档**：想知道我假设了什么结构，读这里。
pub fn expected_keys(n_unit: usize, n_spk: usize, bins: usize) -> Vec<(String, Vec<usize>)> {
    let inner = DIM * EXPANSION;
    let mut v: Vec<(String, Vec<usize>)> = vec![
        ("stack.0.weight".into(), vec![DIM, n_unit, 3]),
        ("stack.0.bias".into(), vec![DIM]),
        ("stack.1.weight".into(), vec![DIM]),
        ("stack.1.bias".into(), vec![DIM]),
        ("stack.3.weight".into(), vec![DIM, DIM, 3]),
        ("stack.3.bias".into(), vec![DIM]),
        ("f0_embed.weight".into(), vec![DIM, 1]),
        ("f0_embed.bias".into(), vec![DIM]),
        ("phase_embed.weight".into(), vec![DIM, 1]),
        ("phase_embed.bias".into(), vec![DIM]),
        ("volume_embed.weight".into(), vec![DIM, 1]),
        ("volume_embed.bias".into(), vec![DIM]),
        ("norm.weight".into(), vec![DIM]),
        ("norm.bias".into(), vec![DIM]),
        ("dense_out.bias".into(), vec![bins * 4]),
        ("dense_out.weight_g".into(), vec![bins * 4, 1]),
        ("dense_out.weight_v".into(), vec![bins * 4, DIM]),
    ];
    if n_spk > 1 {
        v.push(("spk_embed.weight".into(), vec![n_spk, DIM]));
    }
    for i in 0..LAYERS {
        let p = format!("decoder.encoder_layers.{i}.conformer.net");
        v.push((format!("{p}.2.weight"), vec![inner * 2, DIM, 1]));
        v.push((format!("{p}.2.bias"), vec![inner * 2]));
        v.push((format!("{p}.4.weight"), vec![inner, 1, CONV_KERNEL]));
        v.push((format!("{p}.4.bias"), vec![inner]));
        v.push((format!("{p}.6.weight"), vec![DIM, inner, 1]));
        v.push((format!("{p}.6.bias"), vec![DIM]));
    }
    v.sort();
    v
}

/// 拿检查点跟 [`expected_keys`] 对一遍，返回人话报告。
///
/// 对上了返回 `Ok(报告)`；对不上时 `Err` 里是完整的差异清单 ——
/// 缺哪些、形状差在哪、检查点里多出来些什么。
pub fn check_against(
    path: &std::path::Path,
    n_unit: usize,
    n_spk: usize,
    bins: usize,
) -> Result<String> {
    use std::collections::{BTreeMap, BTreeSet};
    let actual: BTreeMap<String, Vec<usize>> = dump_keys(path)?
        .into_iter()
        .filter_map(|(k, v)| k.strip_prefix(WEIGHT_PREFIX).map(|k| (k.to_string(), v)))
        .collect();
    if actual.is_empty() {
        bail!(
            "检查点里没有任何 `{WEIGHT_PREFIX}` 开头的张量。\n             这可能是纯扩散模型（只有 `diff_model.*`），或者根本不是 DDSP-SVC 的检查点。"
        );
    }
    let want = expected_keys(n_unit, n_spk, bins);

    let mut missing = Vec::new();
    let mut wrong = Vec::new();
    for (k, shape) in &want {
        match actual.get(k) {
            None => missing.push(k.clone()),
            Some(got) if got != shape => {
                wrong.push(format!("  {k}\n    检查点 {got:?}\n    本实现 {shape:?}"))
            }
            _ => {}
        }
    }
    let wanted: BTreeSet<&String> = want.iter().map(|(k, _)| k).collect();
    let extra: Vec<&String> = actual.keys().filter(|k| !wanted.contains(k)).collect();

    if missing.is_empty() && wrong.is_empty() {
        // 检查点里确实有几个本实现用不到的张量，而且**已知为什么**：
        //
        // - `*.norm.*`（每个编码层各一对）：那是 attention 分支的 LayerNorm。
        //   `conv_only=True` 下 attention 整个不执行，它也就用不上。
        // - `aug_shift_embed`：训练时的移调增广，推理不给 aug_shift 就不参与。
        //
        // 把"已知无用"和"没想到的多余"分开报 —— 后者才值得警惕。
        let known = |k: &str| {
            k.starts_with("aug_shift_embed") || (k.contains("encoder_layers.") && k.contains(".norm."))
        };
        let (expected_extra, surprise): (Vec<&String>, Vec<&String>) =
            extra.into_iter().partition(|k| known(k.as_str()));
        let mut note = String::new();
        if !expected_extra.is_empty() {
            note.push_str(&format!(
                "\n（另有 {} 个本实现用不到但已知原因的张量：attention 分支的 norm、训练期的移调增广）",
                expected_extra.len()
            ));
        }
        if !surprise.is_empty() {
            note.push_str(&format!(
                "\n⚠️ 还有 {} 个没料到的张量，值得看一眼：{}",
                surprise.len(),
                surprise
                    .iter()
                    .take(5)
                    .map(|s| s.as_str())
                    .collect::<Vec<&str>>()
                    .join("、")
            ));
        }
        return Ok(format!("✅ {} 个张量全部对上{note}", want.len()));
    }

    let mut msg = String::from("检查点与本实现对不上：\n");
    if !missing.is_empty() {
        msg.push_str(&format!("\n缺少 {} 个：\n", missing.len()));
        for k in missing.iter().take(20) {
            msg.push_str(&format!("  {k}\n"));
        }
    }
    if !wrong.is_empty() {
        msg.push_str(&format!("\n形状不符 {} 个：\n", wrong.len()));
        for w in wrong.iter().take(20) {
            msg.push_str(&format!("{w}\n"));
        }
    }
    if !extra.is_empty() {
        msg.push_str(&format!("\n检查点里多出来的（前 20 个，共 {}）：\n", extra.len()));
        for k in extra.iter().take(20) {
            msg.push_str(&format!("  {k}\n"));
        }
    }
    bail!(msg)
}

/// 从 PyTorch 检查点里把参数名列出来 —— 排查"对不上"时的第一件事。
pub fn dump_keys(path: &std::path::Path) -> Result<Vec<(String, Vec<usize>)>> {
    // DDSP-SVC 把 state_dict 塞在顶层的 `model` 键下面。
    // 不指定 key 的话 candle 只看顶层，会一个张量都找不到 ——
    // 那种"成功返回空表"比报错更容易骗过人。
    for key in [Some(PICKLE_ROOT), None] {
        let tensors = candle_core::pickle::read_pth_tensor_info(path, false, key)
            .with_context(|| format!("读取检查点失败：{}", path.display()))?;
        if tensors.is_empty() {
            continue;
        }
        let mut out: Vec<(String, Vec<usize>)> = tensors
            .into_iter()
            .map(|t| (t.name, t.layout.shape().dims().to_vec()))
            .collect();
        out.sort();
        return Ok(out);
    }
    bail!(
        "检查点里找不到任何张量：{}（试过顶层和 `{PICKLE_ROOT}` 两层）",
        path.display()
    )
}

/// 检查点里 state_dict 所在的顶层键。
pub const PICKLE_ROOT: &str = "model";

/// 打开检查点，**进到 `model` 这一层**。
///
/// ⚠️ `VarBuilder::from_pth` 写死了 `PthTensors::new(p, None)`，只看顶层。
/// 而 DDSP-SVC 的顶层是 `{"model": {...}, "global_step": ...}` ——
/// 直接用它会**一个张量都找不到**，而且报的是"cannot find tensor X"，
/// 看着像层名写错了，其实是层级没进去。
fn open_var_builder<'a>(path: &std::path::Path, device: &Device) -> Result<VarBuilder<'a>> {
    let pth = candle_core::pickle::PthTensors::new(path, Some(PICKLE_ROOT))
        .with_context(|| format!("加载检查点失败：{}", path.display()))?;
    Ok(VarBuilder::from_backend(Box::new(pth), DType::F32, device.clone()))
}

/// 解码器网络在 state_dict 里的前缀。
///
/// ⚠️ 实测出来的，不是猜的。DDSP-SVC 5.0 的 `model_0.pt` 里同时装着
/// **两个**模型：`ddsp_model.*`（本实现要的）与 `diff_model.*`
/// （扩散细化模型，另一条路）。按 `model.` 直接找会一个都找不到。
pub const WEIGHT_PREFIX: &str = "ddsp_model.unit2ctrl.";

/// 从检查点里读出配置，而不是写死在代码里。
///
/// 写死的后果是换一个模型（比如 44.1 kHz 换成 48 kHz、block 512 换成 480）
/// 照样能加载、照样出声，只是**整体音高和时长都不对** —— 而且没有任何一步报错。
///
/// 这里能从形状反推的就反推：
/// `window` 的长度就是 `win_length`，`stack.0.weight` 的第二维就是 `n_unit`，
/// `spk_embed.weight` 在不在决定单说话人还是多说话人。
pub fn read_config(path: &std::path::Path) -> Result<Config> {
    let keys: std::collections::BTreeMap<String, Vec<usize>> = dump_keys(path)?
        .into_iter()
        .map(|(k, v)| (k.strip_prefix("ddsp_model.").unwrap_or(&k).to_string(), v))
        .collect();

    let win_length = keys
        .get("window")
        .and_then(|s| s.first().copied())
        .context("检查点里没有 `window` —— 这不是 DDSP 的 CombSub 模型")?;

    let n_unit = keys
        .get("unit2ctrl.stack.0.weight")
        .and_then(|s| s.get(1).copied())
        .context("检查点里没有 `unit2ctrl.stack.0.weight`")?;

    let n_spk = keys
        .get("unit2ctrl.spk_embed.weight")
        .and_then(|s| s.first().copied())
        .unwrap_or(1);

    // dense_out 的输出宽度必须正好是 4 组频点 —— 对不上说明这不是
    // CombSub（比如 Sins 的输出结构完全不同）
    let n_out = keys
        .get("unit2ctrl.dense_out.bias")
        .and_then(|s| s.first().copied())
        .context("检查点里没有 `unit2ctrl.dense_out.bias`")?;
    let bins = win_length / 2 + 1;
    if n_out != bins * 4 {
        bail!(
            "dense_out 宽度是 {n_out}，而 win_length={win_length} 要求 {}（4 组 × {bins} 频点）。\n             这多半不是 CombSub 模型。",
            bins * 4
        );
    }

    // block_size 与 sampling_rate 是 0 维缓冲，形状里读不出来，
    // 必须真的把值取出来。写死默认值的后果是换个模型照样能跑、
    // 照样出声，只是**整体音高和时长都不对**，而且没有一步报错。
    let device = Device::Cpu;
    let vb = open_var_builder(path, &device)?;
    let scalar = |name: &str| -> Result<f32> {
        let t = vb
            .get((), name)
            .with_context(|| format!("检查点里没有 `{name}`"))?;
        Ok(t.to_scalar::<f32>()?)
    };
    let block_size = scalar("ddsp_model.block_size")? as usize;
    let sample_rate = scalar("ddsp_model.sampling_rate")? as u32;
    if block_size == 0 || sample_rate == 0 {
        bail!("检查点里的 block_size={block_size} / sampling_rate={sample_rate} 不合理");
    }

    Ok(Config { win_length, bins, n_unit, n_spk, block_size, sample_rate })
}

/// 从检查点读出来的结构参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub win_length: usize,
    /// `win_length / 2 + 1`
    pub bins: usize,
    pub n_unit: usize,
    pub n_spk: usize,
    /// 解码器的帧步进（样本）。
    pub block_size: usize,
    /// 解码器的采样率。输入必须重采样到这个值。
    pub sample_rate: u32,
}

/// 打开检查点并构造网络。结构参数从检查点里读，不写死。
///
/// 加载前先跑一遍 [`check_against`]：对不上就在这里以人话失败，
/// 而不是等音频出来听着不对再回头猜。
pub fn load_from_pth(path: &std::path::Path) -> Result<(Unit2Control, Config)> {
    let cfg = read_config(path)?;
    let report = check_against(path, cfg.n_unit, cfg.n_spk, cfg.bins)?;
    log::info!("{report}");

    let device = Device::Cpu;
    let vb = open_var_builder(path, &device)?;
    let vb = vb.pp("ddsp_model").pp("unit2ctrl");
    let net = Unit2Control::load(vb, cfg.n_unit, cfg.n_spk, cfg.bins)?;
    Ok((net, cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GLU 沿指定维对半劈：前半是值，后半是门。
    #[test]
    fn glu_halves_the_channel_dim() {
        let d = Device::Cpu;
        // [1, 4, 2]：通道维 4 → 输出 2
        let x = Tensor::from_slice(
            &[1.0f32, 2.0, 3.0, 4.0, 0.0, 0.0, 100.0, 100.0],
            (1, 4, 2),
            &d,
        )
        .unwrap();
        let y = glu(&x, 1).unwrap();
        assert_eq!(y.dims(), &[1, 2, 2]);
        let v = y.flatten_all().unwrap().to_vec1::<f32>().unwrap();
        // 前两通道 [1,2,3,4]，门 [0,0,100,100] → sigmoid ≈ [0.5,0.5,1,1]
        assert!((v[0] - 0.5).abs() < 1e-5, "{v:?}");
        assert!((v[1] - 1.0).abs() < 1e-5, "{v:?}");
        assert!((v[2] - 3.0).abs() < 1e-3, "{v:?}");
        assert!((v[3] - 4.0).abs() < 1e-3, "{v:?}");
    }

    #[test]
    fn glu_refuses_odd_dims() {
        let d = Device::Cpu;
        let x = Tensor::zeros((1, 3, 2), DType::F32, &d).unwrap();
        assert!(glu(&x, 1).is_err(), "奇数通道应当被拒绝");
    }

    #[test]
    fn silu_matches_the_definition() {
        let d = Device::Cpu;
        let x = Tensor::from_slice(&[-1.0f32, 0.0, 1.0, 2.0], 4, &d).unwrap();
        let y = silu(&x).unwrap().to_vec1::<f32>().unwrap();
        for (i, v) in [-1.0f32, 0.0, 1.0, 2.0].iter().enumerate() {
            let want = v / (1.0 + (-v).exp());
            assert!((y[i] - want).abs() < 1e-5, "silu({v}) = {}，应当是 {want}", y[i]);
        }
    }

    #[test]
    fn leaky_relu_has_the_right_slope() {
        let d = Device::Cpu;
        let x = Tensor::from_slice(&[-2.0f32, -1.0, 0.0, 3.0], 4, &d).unwrap();
        let y = leaky_relu(&x, 0.01).unwrap().to_vec1::<f32>().unwrap();
        assert!((y[0] + 0.02).abs() < 1e-6, "{y:?}");
        assert!((y[1] + 0.01).abs() < 1e-6, "{y:?}");
        assert_eq!(y[2], 0.0);
        assert!((y[3] - 3.0).abs() < 1e-6, "{y:?}");
    }

    /// 期望的键集要自洽：没有重名、形状里没有 0。
    #[test]
    fn expected_keys_are_self_consistent() {
        let ks = expected_keys(768, 3, 1025);
        let names: std::collections::BTreeSet<&String> = ks.iter().map(|(k, _)| k).collect();
        assert_eq!(names.len(), ks.len(), "期望的键里有重名");
        for (k, shape) in &ks {
            assert!(!shape.is_empty(), "{k} 形状是空的");
            assert!(shape.iter().all(|d| *d > 0), "{k} 形状里有 0：{shape:?}");
        }
        assert_eq!(
            ks.iter().filter(|(k, _)| k.contains("encoder_layers.2")).count(),
            6,
            "每层应当有 6 个带权重的张量"
        );
        assert!(names.contains(&"spk_embed.weight".to_string()));
        assert!(
            !expected_keys(768, 1, 1025).iter().any(|(k, _)| k == "spk_embed.weight"),
            "单说话人不该有 spk_embed"
        );
    }

    /// 按期望的键集造一份随机权重。
    fn fake_weights(n_unit: usize, n_spk: usize, bins: usize) -> candle_nn::VarMap {
        let d = Device::Cpu;
        let vm = candle_nn::VarMap::new();
        {
            let mut data = vm.data().lock().unwrap();
            for (name, shape) in expected_keys(n_unit, n_spk, bins) {
                let n: usize = shape.iter().product();
                // 小幅确定性"随机"：exp() 不会溢出，而且可复现
                let vals: Vec<f32> = (0..n).map(|i| (i as f32 * 0.7).sin() * 0.05).collect();
                let t = Tensor::from_vec(vals, shape, &d).unwrap();
                data.insert(name, candle_core::Var::from_tensor(&t).unwrap());
            }
        }
        vm
    }

    /// ⚠️ 整条图能跑通：随机权重 → 四组控制量，形状对、无 NaN。
    ///
    /// 这验的是**接线**，不是音质 —— 随机权重当然不会好听。
    /// 但"卷积 groups 写错""转置漏了一次""维度对不上"这些全会在这里炸出来，
    /// 而它们在真权重上只表现为"声音怪"，根本没法二分。
    #[test]
    fn the_whole_graph_runs_with_random_weights() {
        let d = Device::Cpu;
        let (n_unit, n_spk, bins, frames) = (768usize, 2usize, 65usize, 17usize);
        let vm = fake_weights(n_unit, n_spk, bins);
        let vb = VarBuilder::from_varmap(&vm, DType::F32, &d);
        let net = Unit2Control::load(vb, n_unit, n_spk, bins).expect("按期望的键集应当能加载");

        let units: Vec<f32> = (0..frames * n_unit).map(|i| (i as f32 * 0.01).sin()).collect();
        let f0: Vec<f32> = (0..frames).map(|i| 200.0 + i as f32).collect();
        let phase = vec![0.3f32; frames];
        let volume = vec![0.05f32; frames];

        let c = net
            .forward(
                &Inputs { units: &units, dim: n_unit, f0: &f0, phase: &phase, volume: &volume },
                1,
                &d,
            )
            .expect("前向失败");

        assert_eq!(c.frames, frames);
        assert_eq!(c.bins, bins);
        for (label, v) in [
            ("谐波幅度", &c.harmonic_magnitude),
            ("谐波相位", &c.harmonic_phase),
            ("噪声幅度", &c.noise_magnitude),
            ("噪声相位", &c.noise_phase),
        ] {
            assert_eq!(v.len(), frames * bins, "{label} 长度不对");
            assert!(v.iter().all(|x| x.is_finite()), "{label} 里有非有限值");
        }
    }

    /// 同样的输入跑两遍必须逐位相同。
    #[test]
    fn forward_is_deterministic() {
        let d = Device::Cpu;
        let (n_unit, n_spk, bins, frames) = (64usize, 1usize, 9usize, 5usize);
        let vm = fake_weights(n_unit, n_spk, bins);
        let vb = VarBuilder::from_varmap(&vm, DType::F32, &d);
        let net = Unit2Control::load(vb, n_unit, n_spk, bins).unwrap();
        let units: Vec<f32> = (0..frames * n_unit).map(|i| (i as f32 * 0.03).cos()).collect();
        let f0 = vec![210.0f32; frames];
        let phase = vec![0.1f32; frames];
        let volume = vec![0.02f32; frames];
        let a = net.forward(&Inputs { units: &units, dim: n_unit, f0: &f0, phase: &phase, volume: &volume }, 1, &d).unwrap();
        let b = net.forward(&Inputs { units: &units, dim: n_unit, f0: &f0, phase: &phase, volume: &volume }, 1, &d).unwrap();
        assert_eq!(a.harmonic_magnitude, b.harmonic_magnitude);
        assert_eq!(a.noise_phase, b.noise_phase);
    }

    /// 说话人编号越界要**当场**报错，而不是取到别人的音色。
    #[test]
    fn out_of_range_speaker_is_refused() {
        let d = Device::Cpu;
        let (n_unit, n_spk, bins, frames) = (16usize, 2usize, 5usize, 3usize);
        let vm = fake_weights(n_unit, n_spk, bins);
        let vb = VarBuilder::from_varmap(&vm, DType::F32, &d);
        let net = Unit2Control::load(vb, n_unit, n_spk, bins).unwrap();
        let units = vec![0.0f32; frames * n_unit];
        let f0 = vec![200.0f32; frames];
        let phase = vec![0.0f32; frames];
        let volume = vec![0.0f32; frames];
        assert!(net.forward(&Inputs { units: &units, dim: n_unit, f0: &f0, phase: &phase, volume: &volume }, 0, &d).is_err(), "0 号应当被拒");
        assert!(net.forward(&Inputs { units: &units, dim: n_unit, f0: &f0, phase: &phase, volume: &volume }, 9, &d).is_err(), "越界应当被拒");
        assert!(net.forward(&Inputs { units: &units, dim: n_unit, f0: &f0, phase: &phase, volume: &volume }, 2, &d).is_ok());
    }

    /// 输入长度对不上要报错，而不是悄悄截断。
    #[test]
    fn mismatched_input_lengths_are_refused() {
        let d = Device::Cpu;
        let (n_unit, bins) = (16usize, 5usize);
        let vm = fake_weights(n_unit, 1, bins);
        let vb = VarBuilder::from_varmap(&vm, DType::F32, &d);
        let net = Unit2Control::load(vb, n_unit, 1, bins).unwrap();
        let units = vec![0.0f32; 4 * n_unit];
        let f0 = vec![200.0f32; 4];
        let phase = vec![0.0f32; 3]; // 少一帧
        let volume = vec![0.0f32; 4];
        let e = net.forward(&Inputs { units: &units, dim: n_unit, f0: &f0, phase: &phase, volume: &volume }, 1, &d).unwrap_err().to_string();
        assert!(e.contains("长度不一致"), "{e}");
    }

    /// ⚠️ weight_norm 的还原必须对。
    ///
    /// 拿 `weight_v` 当 `weight` 用的话，模型照样能跑、输出也在正常范围，
    /// 只是每一行的尺度全错 —— 典型的"能跑但不像"。
    /// 这里造一个已知的 g/v，验算出来的权重。
    #[test]
    fn weight_norm_is_reconstructed_not_taken_raw() {
        use candle_nn::VarMap;
        let d = Device::Cpu;
        let vm = VarMap::new();
        {
            let mut data = vm.data().lock().unwrap();
            // v = [[3,4],[0,5]]，‖v‖ = [5,5]；g = [[2],[10]]
            data.insert(
                "weight_v".to_string(),
                candle_core::Var::from_tensor(
                    &Tensor::from_slice(&[3.0f32, 4.0, 0.0, 5.0], (2, 2), &d).unwrap(),
                )
                .unwrap(),
            );
            data.insert(
                "weight_g".to_string(),
                candle_core::Var::from_tensor(
                    &Tensor::from_slice(&[2.0f32, 10.0], (2, 1), &d).unwrap(),
                )
                .unwrap(),
            );
            data.insert(
                "bias".to_string(),
                candle_core::Var::from_tensor(&Tensor::zeros(2, DType::F32, &d).unwrap()).unwrap(),
            );
        }
        let vb = VarBuilder::from_varmap(&vm, DType::F32, &d);
        let lin = load_weight_norm_linear(vb, 2, 2).unwrap();
        let w = lin.weight().flatten_all().unwrap().to_vec1::<f32>().unwrap();
        // 期望 g·v/‖v‖ = [[1.2,1.6],[0,10]]
        for (got, want) in w.iter().zip([1.2f32, 1.6, 0.0, 10.0]) {
            assert!((got - want).abs() < 1e-5, "还原出来的权重是 {w:?}");
        }
    }
}
