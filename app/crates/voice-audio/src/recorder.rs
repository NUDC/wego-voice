//! 干声录制。
//!
//! # 为什么录的是干声，而不是耳返里那个声音
//!
//! 这是**架构红线 2**：落盘保存的必须是原始干声，
//! 修正音只用于耳返监听。
//!
//! 理由不是洁癖，是因为修正音是**有损且不可逆**的：PSOLA 改过的音高抽不回来，
//! 共振峰平移过的包络也复原不了。一旦只存了修正音，用户就永远失去了：
//!
//! - 换个角色重来一遍的机会
//! - 用离线算法重新校准的机会（见下）
//! - 把干声送进声线转换的机会 —— 转换模型要的就是干净的源
//!
//! # 顺带一个容易被忽略的收益
//!
//! 实时校准被 30ms 预算捆着手脚：不能回看、f0 提取只能用因果算法、
//! PSOLA 窗口被 `f0_floor` 截断。**离线重跑没有任何这些限制** ——
//! 可以全上下文提 f0、可以前瞻、可以用更贵的合成。
//!
//! 也就是说，干声不只是"原始素材"，它还是**质量更高的那一版校准的输入**。
//!
//! # 实时纪律
//!
//! 音频线程只做一件事：把样本推进无锁环形缓冲。
//! **绝不在音频线程里碰文件系统** —— `write` 会阻塞、会触发缺页、
//! 会被杀毒软件拦一下，任何一次都是几十毫秒的 xrun。
//!
//! 落盘由独立的写入线程做，它可以随便阻塞。

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};

const REL: Ordering = Ordering::Relaxed;

/// 环形缓冲容量（样本数）。
///
/// 48kHz 下约 2 秒。写入线程每 20ms 醒一次，2 秒的余量足够扛住
/// 杀毒扫描、缺页这类几百毫秒级的卡顿而不丢样本。
const RING_SAMPLES: usize = 96_000;

/// 写入线程的轮询间隔。
///
/// 太短会空转烧 CPU，太长会让环形缓冲吃紧。20ms 对 2 秒的缓冲是 1% 占用。
const POLL_MS: u64 = 20;

/// 录音的共享状态。音频线程只读 `active`，其余由控制/写入线程更新。
#[derive(Debug, Default)]
pub struct RecorderState {
    /// 是否正在录。音频线程每块读一次。
    pub active: AtomicBool,
    /// 已写入的样本数。UI 用它显示时长。
    pub frames_written: AtomicU64,
    /// 因环形缓冲满而丢弃的样本数。
    ///
    /// **必须暴露给 UI**：录音悄悄丢帧比录不上更糟 ——
    /// 用户会拿着一份有细微断裂的素材去做后续处理，而且永远查不出原因。
    pub dropped: AtomicU64,
}

/// 采集线程侧的句柄。只有一个方法，而且必须实时安全。
pub struct RecorderSink {
    producer: rtrb::Producer<f32>,
    state: Arc<RecorderState>,
}

impl RecorderSink {
    /// 把一块干声样本推入录音缓冲。**实时安全**：无分配、无锁、无系统调用。
    #[inline]
    pub fn push(&mut self, mono: &[f32]) {
        if !self.state.active.load(REL) {
            return;
        }
        let mut dropped = 0u64;
        for &s in mono {
            if self.producer.push(s).is_err() {
                dropped += 1;
            }
        }
        if dropped > 0 {
            self.state.dropped.fetch_add(dropped, REL);
        }
    }
}

/// 控制侧句柄。开始/停止录音，查状态。
pub struct Recorder {
    state: Arc<RecorderState>,
    consumer: Option<rtrb::Consumer<f32>>,
    sample_rate: u32,
    /// 正在写的那个线程。停止时 join 它，确保文件收尾完成。
    ///
    /// 返回值里**必须把 consumer 带回来**：它被移进了线程，不还回来的话
    /// 第二次录音就没有消费端可用了（第一版就是这么写的，被
    /// `a_second_take_does_not_inherit_the_first` 当场抓住）。
    /// 元组而不是 `Result<Consumer>`：写入出错时 consumer 也得回来。
    #[allow(clippy::type_complexity)]
    writer: Option<std::thread::JoinHandle<(rtrb::Consumer<f32>, Result<()>)>>,
    current: Option<PathBuf>,
}

impl Recorder {
    /// 建一对「采集侧 sink + 控制侧 recorder」。
    pub fn new(sample_rate: u32) -> (RecorderSink, Self) {
        let (producer, consumer) = rtrb::RingBuffer::<f32>::new(RING_SAMPLES);
        let state = Arc::new(RecorderState::default());
        (
            RecorderSink { producer, state: Arc::clone(&state) },
            Self {
                state,
                consumer: Some(consumer),
                sample_rate,
                writer: None,
                current: None,
            },
        )
    }

    pub fn state(&self) -> &Arc<RecorderState> {
        &self.state
    }

    #[inline]
    pub fn is_recording(&self) -> bool {
        self.state.active.load(REL)
    }

    pub fn current_path(&self) -> Option<&Path> {
        self.current.as_deref()
    }

    /// 已录时长（秒）。
    pub fn elapsed_secs(&self) -> f32 {
        self.state.frames_written.load(REL) as f32 / self.sample_rate.max(1) as f32
    }

    /// 开始录音。落盘到 `path`（32-bit float WAV）。
    pub fn start(&mut self, path: impl AsRef<Path>) -> Result<()> {
        if self.is_recording() {
            anyhow::bail!("已经在录音了");
        }
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("创建目录失败：{}", dir.display()))?;
        }

        let mut consumer = self
            .consumer
            .take()
            .ok_or_else(|| anyhow::anyhow!("录音器状态损坏：消费端已被取走"))?;

        // 排空上一轮可能残留的样本，否则会把上次的尾巴接到这次开头
        while consumer.pop().is_ok() {}

        self.state.frames_written.store(0, REL);
        self.state.dropped.store(0, REL);

        let mut wav = WavWriter::create(&path, self.sample_rate)?;
        let state = Arc::clone(&self.state);
        let target = path.clone();

        // 先建好文件再置 active：反过来的话，音频线程可能在文件就绪前
        // 就开始往缓冲里推，而那些样本的时间戳对不上文件头
        state.active.store(true, REL);

        self.writer = Some(std::thread::spawn(move || {
            let mut buf = vec![0.0f32; 4096];
            let res = (|| -> Result<()> {
                loop {
                    let active = state.active.load(REL);
                    let mut moved = 0usize;
                    while moved < buf.len() {
                        match consumer.pop() {
                            Ok(s) => {
                                buf[moved] = s;
                                moved += 1;
                            }
                            Err(_) => break,
                        }
                    }
                    if moved > 0 {
                        wav.write(&buf[..moved])?;
                        state.frames_written.fetch_add(moved as u64, REL);
                    }
                    // 停止之后还要再空转一轮把残留样本取干净，
                    // 否则最后 20ms 的声音会丢在缓冲里
                    if !active && moved == 0 {
                        break;
                    }
                    if moved == 0 {
                        std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
                    }
                }
                wav.finalize()
                    .with_context(|| format!("收尾失败：{}", target.display()))
            })();
            // consumer 无论成功失败都要还回去，否则录不了第二条
            (consumer, res)
        }));

        self.current = Some(path);
        Ok(())
    }

    /// 停止录音，返回落盘的文件路径。
    ///
    /// 会等写入线程把缓冲里剩下的样本全部写完并补好 WAV 头 ——
    /// 不等的话文件长度字段是错的，多数播放器会当成损坏文件。
    pub fn stop(&mut self) -> Result<Option<PathBuf>> {
        if !self.is_recording() {
            return Ok(None);
        }
        self.state.active.store(false, REL);
        if let Some(h) = self.writer.take() {
            match h.join() {
                Ok((consumer, res)) => {
                    self.consumer = Some(consumer);
                    res?;
                }
                Err(_) => anyhow::bail!("录音写入线程 panic"),
            }
        }
        Ok(self.current.take())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // 引擎被停掉时别留下一个头没补好的半截文件
        let _ = self.stop();
    }
}

/// 极简 32-bit float WAV 写入器。
///
/// 自己写而不引 `hound`：需求只有「单声道 f32，边写边流」，
/// 而 `voice-audio` 的依赖每多一个，将来做 CLAP 插件时就多一份审计。
struct WavWriter {
    out: BufWriter<File>,
    data_bytes: u32,
}

/// WAV 头长度（RIFF 12 + fmt 24 + data 8）。
const HEADER_LEN: u32 = 44;

impl WavWriter {
    fn create(path: &Path, sample_rate: u32) -> Result<Self> {
        let file = File::create(path)
            .with_context(|| format!("创建文件失败：{}", path.display()))?;
        let mut out = BufWriter::new(file);

        // 先写一份长度占位的头，收尾时回填 —— 流式写入无法预知总长
        let byte_rate = sample_rate * 4;
        out.write_all(b"RIFF")?;
        out.write_all(&0u32.to_le_bytes())?; // 占位：文件长度 - 8
        out.write_all(b"WAVE")?;
        out.write_all(b"fmt ")?;
        out.write_all(&16u32.to_le_bytes())?; // fmt chunk 长度
        out.write_all(&3u16.to_le_bytes())?; // 3 = IEEE float
        out.write_all(&1u16.to_le_bytes())?; // 单声道
        out.write_all(&sample_rate.to_le_bytes())?;
        out.write_all(&byte_rate.to_le_bytes())?;
        out.write_all(&4u16.to_le_bytes())?; // block align
        out.write_all(&32u16.to_le_bytes())?; // 位深
        out.write_all(b"data")?;
        out.write_all(&0u32.to_le_bytes())?; // 占位：data 长度

        Ok(Self { out, data_bytes: 0 })
    }

    fn write(&mut self, samples: &[f32]) -> Result<()> {
        for &s in samples {
            self.out.write_all(&s.to_le_bytes())?;
        }
        self.data_bytes += (samples.len() * 4) as u32;
        Ok(())
    }

    /// 回填两个长度字段并落盘。
    fn finalize(mut self) -> Result<()> {
        self.out.flush()?;
        let f = self.out.get_mut();
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&(HEADER_LEN - 8 + self.data_bytes).to_le_bytes())?;
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&self.data_bytes.to_le_bytes())?;
        f.sync_all()?;
        Ok(())
    }
}

/// 生成一个带时间戳的录音文件名。
///
/// 不用序号：序号要先扫目录，而且用户删掉中间某个之后会重复。
pub fn timestamped_name(unix_secs: u64) -> String {
    // 只做到"同一秒内不重名"即可，不引 chrono
    format!("wego-{unix_secs}.wav")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("wego-rec-test-{name}"))
    }

    /// 录进去什么，读出来就得是什么 —— 逐位一致。
    ///
    /// 干声是后续所有处理的源头，这里有任何改动都会一路传下去。
    #[test]
    fn records_dry_signal_bit_exact() {
        let path = tmp("bitexact.wav");
        let _ = std::fs::remove_file(&path);

        let (mut sink, mut rec) = Recorder::new(48_000);
        rec.start(&path).unwrap();

        let input: Vec<f32> = (0..4800)
            .map(|i| (i as f32 / 4800.0 * 2.0 - 1.0) * 0.75)
            .collect();
        for chunk in input.chunks(144) {
            sink.push(chunk);
        }
        // 给写入线程时间取走
        std::thread::sleep(std::time::Duration::from_millis(120));
        let out = rec.stop().unwrap().unwrap();

        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");

        let data_len = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
        assert_eq!(data_len, input.len() * 4, "data 长度字段没回填对");
        assert_eq!(bytes.len(), HEADER_LEN as usize + data_len);

        let decoded: Vec<f32> = bytes[44..]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        assert_eq!(decoded, input, "录下来的干声和输入不一致");

        let _ = std::fs::remove_file(&out);
    }

    /// 没在录的时候 push 必须是纯粹的空操作 —— 否则停止录音之后
    /// 音频线程还在往缓冲里塞，下一次开录会接上一段旧声音。
    #[test]
    fn push_is_a_noop_when_not_recording() {
        let (mut sink, rec) = Recorder::new(48_000);
        sink.push(&[0.5; 512]);
        assert_eq!(rec.state().frames_written.load(REL), 0);
        assert_eq!(rec.state().dropped.load(REL), 0);
    }

    /// 上一轮的残留不能流进这一轮的开头。
    #[test]
    fn a_second_take_does_not_inherit_the_first() {
        let path1 = tmp("take1.wav");
        let path2 = tmp("take2.wav");
        let _ = std::fs::remove_file(&path1);
        let _ = std::fs::remove_file(&path2);

        let (mut sink, mut rec) = Recorder::new(48_000);

        rec.start(&path1).unwrap();
        sink.push(&[0.9; 1000]);
        std::thread::sleep(std::time::Duration::from_millis(80));
        rec.stop().unwrap();

        // 停止之后推的样本应当被丢掉
        sink.push(&[0.9; 1000]);

        rec.start(&path2).unwrap();
        sink.push(&[-0.25; 480]);
        std::thread::sleep(std::time::Duration::from_millis(120));
        let out = rec.stop().unwrap().unwrap();

        let bytes = std::fs::read(&out).unwrap();
        let decoded: Vec<f32> = bytes[44..]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        assert_eq!(decoded.len(), 480, "第二条录进了不属于它的样本");
        assert!(decoded.iter().all(|&s| s == -0.25));

        let _ = std::fs::remove_file(&path1);
        let _ = std::fs::remove_file(&out);
    }

    /// 空录音也必须是一个合法 WAV，不能是半截文件。
    #[test]
    fn empty_take_is_still_a_valid_wav() {
        let path = tmp("empty.wav");
        let _ = std::fs::remove_file(&path);

        let (_, mut rec) = Recorder::new(48_000);
        rec.start(&path).unwrap();
        let out = rec.stop().unwrap().unwrap();

        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(bytes.len(), HEADER_LEN as usize);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 0);

        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn rejects_double_start() {
        let path = tmp("double.wav");
        let _ = std::fs::remove_file(&path);
        let (_, mut rec) = Recorder::new(48_000);
        rec.start(&path).unwrap();
        assert!(rec.start(&path).is_err());
        let _ = rec.stop();
        let _ = std::fs::remove_file(&path);
    }
}
