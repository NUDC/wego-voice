//! 内容编码器（ContentVec）。
//!
//! # 它抽的是什么
//!
//! 「唱的是什么内容」，而**不是**「谁在唱」。这是整条声线转换链路的核心技巧：
//! 把内容和音色拆开，扔掉音色，再拿目标的音色重新填上。
//!
//! 音高不在这里 —— f0 是**另外显式给**的（`voice-core::track_pitch`）。
//! 这一点对唱歌是决定性的：音高走你的，音色走他的。
//! 靠最近邻替换帧的做法（kNN-VC）在说话上很漂亮，但替换进来的帧自带
//! 目标人的音高，唱歌时旋律会被拖走。
//!
//! # 16 kHz 不是可选项
//!
//! ContentVec 建在 HuBERT 上，训练时就是 16 kHz。喂 48 kHz 进去
//! **不会报错**，只会安静地给出垃圾特征 —— 这正是最难查的那类错。
//! 所以重采样是必需步骤，而且必须**抗混叠**：
//! 直接每 3 个取 1 个会把 8 kHz 以上的能量折回可听区，
//! 特征照样出得来，照样是垃圾。

use anyhow::{Context, Result};
use std::path::Path;

use crate::session::{expect_shape, Contract};
use crate::{ENCODER_DIM, ENCODER_HOP, ENCODER_RATE};

/// 一段音频的内容特征。
pub struct Features {
    /// 行优先：`data[t * dim + d]`。
    pub data: Vec<f32>,
    pub frames: usize,
    pub dim: usize,
}

impl Features {
    pub fn frame(&self, t: usize) -> &[f32] {
        &self.data[t * self.dim..(t + 1) * self.dim]
    }
}

pub struct ContentEncoder {
    session: ort::session::Session,
    contract: Contract,
}

impl ContentEncoder {
    /// 打开编码器，并**立刻校验形状**。
    ///
    /// 不校验的话，下错一个模型（比如 256 维那版）会一路跑到训练完成，
    /// 才以"声音不像"的形式暴露出来。
    pub fn open(path: &Path, threads: usize) -> Result<Self> {
        let (session, contract) = crate::session::open(path, threads)?;

        let out = contract
            .outputs
            .first()
            .context("模型没有输出口 —— 文件多半损坏了")?;
        // 输出是 [batch, 帧, 维] 或 [batch, 维, 帧]，两种导出都见过。
        // 这里只确认「三维，且有一维等于 768」，具体哪一维在 encode 里判。
        if out.rank() != 3 {
            anyhow::bail!(
                "内容特征应当是 3 维，模型给的是 {} 维 {}。多半下错了模型文件。",
                out.rank(),
                out.shape_str()
            );
        }
        let has_dim = out
            .dims
            .iter()
            .any(|d| matches!(d, Some(v) if *v as usize == ENCODER_DIM) || d.is_none());
        if !has_dim {
            anyhow::bail!(
                "内容特征里找不到 {ENCODER_DIM} 维（模型是 {}）。\n\
                 256 维那版是给旧模型用的，这里要 vec-768-layer-12。",
                out.shape_str()
            );
        }

        let inp = contract.inputs.first().context("模型没有输入口")?;
        expect_shape(inp, &vec![None; inp.rank()], "音频输入")?;

        Ok(Self { session, contract })
    }

    pub fn contract(&self) -> &Contract {
        &self.contract
    }

    /// 把一段音频抽成内容特征。`rate` 是输入的采样率。
    pub fn encode(&mut self, samples: &[f32], rate: u32) -> Result<Features> {
        let mono16 = resample(samples, rate, ENCODER_RATE);
        if mono16.len() < ENCODER_HOP * 2 {
            anyhow::bail!(
                "音频太短：重采样后只有 {} 个样本，不够一帧（需要至少 {}）",
                mono16.len(),
                ENCODER_HOP * 2
            );
        }

        let n = mono16.len();
        let input_rank = self.contract.inputs[0].rank();
        let shape: Vec<usize> = match input_rank {
            3 => vec![1, 1, n],
            2 => vec![1, n],
            r => anyhow::bail!("音频输入是 {r} 维，不认识这种导出"),
        };
        let tensor = ort::value::Tensor::from_array((shape, mono16))
            .map_err(|e| anyhow::anyhow!("构造输入张量失败：{e}"))?;

        let name = self.contract.inputs[0].name.clone();
        let outputs = self
            .session
            .run(ort::inputs![name.as_str() => tensor])
            .map_err(|e| anyhow::anyhow!("推理失败：{e}"))?;

        let out_name = self.contract.outputs[0].name.clone();
        let (shape, data) = outputs[out_name.as_str()]
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("取输出张量失败：{e}"))?;

        // 两种导出都见过：[1, 帧, 768] 与 [1, 768, 帧]。
        // 按哪一维等于 768 来判，而不是赌一种 —— 赌错了特征会被转置，
        // 而转置后的特征照样能喂进去、照样出声音，只是全是垃圾。
        let (frames, dim, time_last) = match (shape[1] as usize, shape[2] as usize) {
            (t, d) if d == ENCODER_DIM => (t, d, false),
            (d, t) if d == ENCODER_DIM => (t, d, true),
            _ => anyhow::bail!(
                "输出形状 {:?} 里没有 {ENCODER_DIM} 维，认不出哪一维是特征",
                shape
            ),
        };

        let mut v = vec![0.0f32; frames * dim];
        if time_last {
            for t in 0..frames {
                for d in 0..dim {
                    v[t * dim + d] = data[d * frames + t];
                }
            }
        } else {
            v.copy_from_slice(&data[..frames * dim]);
        }

        Ok(Features { data: v, frames, dim })
    }
}

/// 抗混叠重采样。
///
/// 用窗函数化 sinc（Hann 窗，零点跨 `TAPS` 个输出周期）。
/// **降采样时截止频率跟着目标奈奎斯特走**，不是跟着源走 ——
/// 这一条写反的话滤波器形同虚设，8 kHz 以上的能量会原样折回来。
///
/// 采样率相同时直接返回，不做任何处理：常见情况不该白付一次卷积。
pub fn resample(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || x.is_empty() {
        return x.to_vec();
    }
    /// 每侧的过零点数。越大越陡峭、越贵。16 对语音特征足够。
    const TAPS: i64 = 16;

    let ratio = to as f64 / from as f64;
    let n_out = ((x.len() as f64) * ratio).floor() as usize;
    let mut out = Vec::with_capacity(n_out);

    // 降采样时把截止压到目标的奈奎斯特以下；升采样时不必压
    let cutoff = ratio.min(1.0);
    let half = (TAPS as f64 / cutoff).ceil() as i64;

    for i in 0..n_out {
        let center = i as f64 / ratio;
        let base = center.floor() as i64;
        let mut acc = 0.0f64;
        let mut norm = 0.0f64;
        for k in -half..=half {
            let idx = base + k;
            if idx < 0 || idx as usize >= x.len() {
                continue;
            }
            let t = center - idx as f64;
            let w = hann(t, half as f64);
            if w <= 0.0 {
                continue;
            }
            let h = sinc(t * cutoff) * cutoff * w;
            acc += x[idx as usize] as f64 * h;
            norm += h;
        }
        // 归一化：窗口在边界被截断时增益会掉，不补会让首尾变轻
        out.push(if norm.abs() > 1e-12 { (acc / norm) as f32 } else { 0.0 });
    }
    out
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        let p = std::f64::consts::PI * x;
        p.sin() / p
    }
}

fn hann(t: f64, half: f64) -> f64 {
    if t.abs() > half {
        return 0.0;
    }
    0.5 + 0.5 * (std::f64::consts::PI * t / half).cos()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn tone(hz: f32, rate: u32, secs: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        (0..n).map(|i| (TAU * hz * i as f32 / rate as f32).sin()).collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// 在目标带宽内的信号要**基本原样**通过。
    #[test]
    fn passband_survives_downsampling() {
        let x = tone(1000.0, 48_000, 0.5);
        let y = resample(&x, 48_000, 16_000);
        assert_eq!(y.len(), 8_000, "输出长度不对");
        // 掐掉首尾各 200 个样本，避开边界瞬态
        let core = &y[200..y.len() - 200];
        assert!(
            (rms(core) - rms(&x)).abs() < 0.03,
            "1 kHz 通过后能量变了：{} → {}",
            rms(&x),
            rms(core)
        );
    }

    /// ⚠️ 这是这个模块**唯一真正危险**的失败模式。
    ///
    /// 12 kHz 在 16 kHz 采样率下放不下（奈奎斯特 8 kHz）。抗混叠没做对的话，
    /// 它不会消失，而是**折回 4 kHz** —— 波形看着正常、特征照样出得来、
    /// 模型照样有输出，只是全是垃圾。没有任何一步会报错。
    #[test]
    fn out_of_band_energy_is_removed_not_folded_back() {
        let x = tone(12_000.0, 48_000, 0.5);
        let y = resample(&x, 48_000, 16_000);
        let core = &y[200..y.len() - 200];
        assert!(
            rms(core) < 0.05,
            "12 kHz 没被滤掉，残留 RMS {:.3} —— 它已经折回 4 kHz 了",
            rms(core)
        );
    }

    /// 直流不许被改动：整体电平偏移是滤波器归一化写错的典型表现。
    #[test]
    fn dc_is_preserved() {
        let x = vec![0.5f32; 48_000];
        let y = resample(&x, 48_000, 16_000);
        let core = &y[100..y.len() - 100];
        let mean = core.iter().sum::<f32>() / core.len() as f32;
        assert!((mean - 0.5).abs() < 0.01, "直流从 0.5 变成了 {mean}");
    }

    #[test]
    fn same_rate_is_a_passthrough() {
        let x = tone(440.0, 48_000, 0.1);
        let y = resample(&x, 48_000, 48_000);
        assert_eq!(x, y, "同采样率不该动数据");
    }

    /// 帧率必须落在 50 fps 上 —— f0 轨与响度都要对齐到这个栅格。
    #[test]
    fn hop_gives_fifty_frames_per_second() {
        assert_eq!(ENCODER_RATE as usize / ENCODER_HOP, 50);
    }
}
