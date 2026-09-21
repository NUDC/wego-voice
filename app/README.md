# wego-voice

**桌面实时修音与音色转换工具。**

唱的时候耳返里听到修准的自己；唱完把干声换成目标音色，分轨导出可用的音频文件。

**Windows** 单机运行，无账号、无服务端、运行时离线。

## 定位

这是一个**工具**，不是面向小白的消费级 App：

- 参数全暴露、可存预设，不藏在"简单模式"后面
- 输出就是本地文件，可分轨，能直接进 DAW
- 不做打分、成就、社交分享
- **默认你戴有线耳机或接了声卡** —— 蓝牙不支持实时耳返（物理限制，见下）

给**要把唱的东西做成成品**的人：翻唱创作者、内容/播客作者、自弹自唱、想练音准的人。

---

## 文档

| 文档 | 内容 |
|---|---|
| [实施方案.md](docs/实施方案.md) | **单一权威文档**：定调表、架构、选型、四阶段计划、风险登记册 |
| [Phase0-实测记录.md](docs/Phase0-实测记录.md) | Phase 0 线 A 的实测数字与踩坑记录 |
| [可行性分析.md](docs/可行性分析.md) | 背景资料（已被实施方案取代） |
| [技术架构选型.md](docs/技术架构选型.md) | 背景资料（已被实施方案取代） |

---

## 当前状态

**Phase 0 线 A 进行中。** DSP 核心 + 双后端实时引擎已完成，55 个测试全绿。

**生产配置**：WASAPI 独占，输入 96 帧 / 输出 144 帧，环形水位 2.5，`f0_floor` 130Hz。

| 指标 | 结果 | |
|---|---|---|
| 端到端延迟 | **29.92 ms** | ✅ < 30 ms |
| CPU 余量（平均/峰值） | 4% / 32% | ✅ |
| 时钟漂移 | 实测 -56 ppm，补偿后残余 < 2 ms | ✅ |
| 10 分钟连测 xrun | 42 次 | 🔴 归因：**测试机整机 44ms 冻结**，非代码 |

两项阻塞，都因缺设备：**换普通桌面机复测稳定性**、
**脉冲往返实测**（需回环线）。工具与归因指标均已就位，拿到设备即可直接跑。

详见 [Phase0 实测记录](docs/Phase0-实测记录.md)。

---

## 工程结构

```
crates/voice-core/    纯 DSP：FFT + YIN 音高检测 + TD-PSOLA 修正 + 调式量化
                      零依赖、无 I/O、无 GUI —— 将来可直接复用为 CLAP 插件核心
crates/voice-audio/   实时引擎
  duplex.rs           与后端无关的核心：环形缓冲、漂移补偿、块大小自适应
  backend/            WASAPI 独占（主力）+ cpal 共享（兜底）
src-tauri/            Tauri 外壳：引擎持有、command 层、20Hz 指标推送
src/                  React 诊断页 + canvas 音高条
```

### 三种入口，同一个可执行文件

```bash
wego-voice-app.exe               # 诊断页 GUI
wego-voice-app.exe --autostart   # 开窗即启动引擎
wego-voice-app.exe --bench       # headless 压测（不创建 WebView）
wego-voice-app.exe --selftest    # 自检：走一遍 UI 用的那条路并断言
```

`--bench` 与独立的 `wego-bench` 跑的是**完全相同**的引擎代码，
两者数字相减即 Tauri 外壳的净开销 —— 同源对比，没有框架差异混在里面。

### 两个后端

| 后端 | 角色 | 延迟 | 代价 |
|---|---|---|---|
| WASAPI 独占 | **主力** | **29.92 ms** | 运行期间独占声卡 |
| cpal 共享 | **兜底** | 55 ms | 超 No-Go 线；仅在独占开不成（设备被占用）时使用 |

cpal 在 Windows 上硬编码共享模式，而共享模式的块周期由音频引擎决定、
改不了（0.15 与 0.18 都查过源码）。所以低延迟只能绕开它。

> 平台已收窄为**仅 Windows**，cpal 的跨平台价值随之消失 ——
> 将来可用 `wasapi` crate 自带的共享模式替掉它，彻底去掉这个依赖。

### 两个纪律区

Code review 时单独看：

- **`voice-core/src/psola.rs` 与 `lib.rs` 的 `process` 路径** —— 实时安全：
  无堆分配、无锁、无日志、无 panic。所有缓冲在构造时预分配。
- **将来的 `infer/preprocess.rs`** —— 推理预处理必须与训练侧逐位对齐，
  对不齐的表现是"能出声但音色不像"，极难排查。

---

## 快速开始

需要 Rust（当前用 nightly 1.100，稳定版亦可）+ Node 20+。

```bash
# 首次：装前端依赖
npm install

# 构建并运行诊断页
npx tauri build --no-bundle
./target/release/wego-voice-app.exe --autostart

# 开发模式（热重载）
npm run tauri dev

# 跑全部测试
cargo test --release --workspace
npx tsc --noEmit

# 列出音频设备
cargo run -p voice-audio --release --bin wego-bench -- --devices

# 查本机声卡的硬件周期能力，算出各条低延迟路线可达的延迟（Windows）
cargo run -p voice-audio --release --bin wasapi-probe

# 真的去打开独占流，验证上面算出的延迟能否兑现（Windows）
cargo run -p voice-audio --release --bin wasapi-exclusive

# 压测并给出 Phase 0 判定（⚠️ 戴耳机，或加 --mute 避免啸叫）
# Windows 上默认 auto = 优先独占，失败退回 cpal
cargo run -p voice-audio --release --bin wego-bench -- --duration 60 --mute

# 指定后端对比：wasapi=独占低延迟 / cpal=共享高延迟
cargo run -p voice-audio --release --bin wego-bench -- --backend wasapi --mute
cargo run -p voice-audio --release --bin wego-bench -- --backend cpal --mute

# 实测往返延迟（需回环线，或把耳机贴住麦克风）
cargo run -p voice-audio --release --bin wego-bench -- --latency

# 10 分钟连测，查时钟漂移
cargo run -p voice-audio --release --bin wego-bench -- --duration 600 --mute

# 调 PSOLA 延迟/音质权衡（100Hz→20ms，130→15.4ms，160→12.5ms）
cargo run -p voice-audio --release --bin wego-bench -- --f0-floor 130

# 完整用法
cargo run -p voice-audio --release --bin wego-bench -- --help
```

> **务必戴耳机。** 外放会让麦克风拾到自己的输出，形成啸叫回路。
> 不想出声就加 `--mute`：DSP 照常跑，只是不送耳返。

---

## 设计要点

**三条架构红线**（违反即返工，详见实施方案 §3.2）：

1. 实时音频路径 100% 在 Rust 侧，采样点绝不经过 WebView 或 IPC
2. 落盘保存的必须是原始干声，修正音只用于耳返监听
3. 推理线程绝不与实时音频线程抢 CPU

**延迟预算**：端到端 < 30ms 是硬线。超过 50ms 会触发延迟听觉反馈（DAF）效应，
用户会不自觉结巴跑调 —— 那时产品比不开更糟。
