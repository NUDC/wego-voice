# wego-voice

**桌面实时修音与音色转换工具。**

唱的时候耳返里听到修准的自己；唱完把干声换成目标音色，分轨导出可用的音频文件。

**Windows** 单机运行，无账号、无服务端、运行时离线。

官网：<https://nudc.github.io/wego-voice/>

---

## 仓库结构

```
wego-voice/
├── app/      桌面应用（Tauri + React + Vite + TS + Rust workspace）
├── site/     官网（React + Vite + TS）
└── docs/     项目文档（两者共用，不参与任何构建）
```

**为什么分开**：

| | `app/` | `site/` |
|---|---|---|
| 受众 | 已经装了工具的用户 | 还没听说过这东西的人 |
| 构建 | Tauri + cargo + vite → exe | vite → 静态站 → CDN |
| 发布节奏 | 跟版本走 | 随时改文案 |
| 依赖 | Rust + Node | 只有 Node |

两边都用 React + Vite + TS，但**各自独立的 `node_modules` 与构建**。
同栈是为了降低维护心智成本，分目录是为了让官网改文案不必碰 cargo。

混在一起的代价：官网改个文案要跑 cargo、两边的 `node_modules` 搅在一起、
CI 每次都全量构建。分开之后互不影响。

`docs/` 放在根：它既不属于 app 也不属于 site，
而且官网的内容（延迟原理、硬件要求、FAQ）直接引用这里的实测数字。

---

## 文档

| 文档 | 内容 |
|---|---|
| [实施方案.md](docs/实施方案.md) | **单一权威文档**：定调表、架构、选型、计划、风险登记册 |
| [Phase0-实测记录.md](docs/Phase0-实测记录.md) | 实测数字与踩坑记录（10 条教训） |
| [可行性分析.md](docs/可行性分析.md) | 背景资料（已被实施方案取代） |
| [技术架构选型.md](docs/技术架构选型.md) | 背景资料（已被实施方案取代） |

---

## 当前状态

**动作 A（实时修音）已完成并通过实测；动作 B（音色转换）未开始。**

| 指标 | 结果 | |
|---|---|---|
| 端到端延迟 | **29.92 ms** | ✅ < 30 ms |
| CPU 余量（平均/峰值） | 4% / 32% | ✅ |
| 时钟漂移 | 实测 -56 ppm，补偿后残余 < 2 ms | ✅ |
| 10 分钟连测 xrun | 42 次 | 🔴 归因：**测试机整机 44ms 冻结**，非代码 |

**生产配置**：WASAPI 独占，输入 96 帧 / 输出 144 帧，环形水位 2.5，`f0_floor` 130Hz。

两项阻塞，都因缺设备：**换普通桌面机复测稳定性**、**脉冲往返实测**（需回环线）。
工具与归因指标均已就位，拿到设备即可直接跑。

---

## 快速开始

### 应用

需要 Rust（nightly 1.100，稳定版亦可）+ Node 20+。

```bash
cd app
npm install

# 构建并运行
npm run tauri build -- --no-bundle
./target/release/wego-voice-app.exe --autostart

# 开发模式（热重载）
npm run tauri dev

# 测试
cargo test --release --workspace
npx tsc --noEmit
```

**三种入口，同一个可执行文件**：

```bash
wego-voice-app.exe               # 诊断页 GUI
wego-voice-app.exe --autostart   # 开窗即启动引擎
wego-voice-app.exe --bench       # headless 压测（不创建 WebView）
wego-voice-app.exe --selftest    # 自检：走一遍 UI 用的那条路并断言
```

`--bench` 与独立的 `wego-bench` 跑的是**完全相同**的引擎代码，
两者数字相减即 Tauri 外壳的净开销 —— 同源对比，没有框架差异混在里面。

> 🔴 **打包必须走 `npm run tauri build`，不能直接 `cargo build`。**
>
> 直接 `cargo build` 出来的 exe **前端是死的**：开窗即空白，
> 零 JS 执行、零 IPC，看起来像应用崩了。
> 原因是它不保证把最新的 `dist/` 重新嵌进二进制。
> 排查这个坑花了很久，因为所有征兆都指向前端代码本身。

### 音频测量工具

`wego-bench` 用 clap 子命令：

```bash
cd app

wego-bench devices                    # 列出音频设备
wego-bench                            # 默认 30 秒压测
wego-bench soak -d 600 --mute         # 10 分钟连测，查时钟漂移
wego-bench soak --backend cpal        # 用共享模式做对照
wego-bench soak --no-rt               # 关掉实时优先级，做对照实验
wego-bench latency --rounds 50 --mute # 脉冲实测往返延迟
wego-bench sweep                      # 扫描缓冲大小，找不爆音的最小值
wego-bench dsp -d 30                  # 离线量 DSP 成本（不碰声卡）
wego-bench --help                     # 全部参数（带实测数字）
```

⚠️ **量 DSP 成本必须用 `dsp` 子命令，不能用 `soak`。**
`soak` 喂的是真实麦克风：没人唱歌时信号判为清音，PSOLA 直接透传，
`overlap_add` 一次都不执行 —— 于是它测出来的"CPU 占用"跟 DSP 真实成本无关。
`dsp` 喂合成浊音（冲激串过谐振器），逼着走完整的分析＋合成路径。

```bash
wego-bench dsp -d 30                          # 原声基线
wego-bench dsp -d 30 --formant-shift 4        # 声线路径（逐样本插值）
wego-bench dsp -d 30 --pitch-shift 5 --formant-shift 6
```

共用参数（`--backend` / `--mute` / `--f0-floor` / `--target-fill` / `--key` …）
是 global 的，写在子命令前后都行。

另有两个探针：

```bash
cargo run --release --bin wasapi-probe      # 查声卡硬件周期能力
cargo run --release --bin wasapi-exclusive  # 实开独占流验证
```

> **务必戴耳机。** 外放会让麦克风拾到自己的输出，形成啸叫回路。
> 不想出声就加 `--mute`：DSP 照常跑，只是不送耳返。

### 日志

用 `log` + `env_logger`。**日志和输出是两回事**：

- 报告表格走 `println!` —— 那是程序的**输出**，用户要看的结果
- 警告与错误走 `log::warn!` / `log::error!` —— 那是**诊断信息**

混在一起的后果：结果被日志级别过滤掉，或者日志混进了要被 grep 解析的输出里。

```bash
RUST_LOG=debug wego-bench soak    # 调日志级别
```

默认过滤为 `warn` + 本项目 crate `info`。压低第三方是刻意的：
`audio_thread_priority` 每次提权都打一条 INFO（而且是从音频线程打的），
不压下去会淹掉真正要看的警告。

⚠️ **我们自己的代码绝不在音频线程里打日志** —— `log::warn!` 会分配、取锁、写 IO，
全都违反实时纪律。提权失败之类的信息通过原子量上报，由控制线程/UI 呈现
（见 `priority.rs` 的注释）。

### 官网

React + Vite + TS，与 `app/` 同栈（一个人维护两边心智成本更低）。
部署在 GitHub Pages：<https://nudc.github.io/wego-voice/>

```bash
cd site
npm install
npm run dev     # http://localhost:5174（避开 app 的 5173，可同时开）
npm run build   # tsc → vite build → SSR 构建 → 预渲染 → site/dist/
npm run preview # 本地验收构建产物
```

**产物是零 JavaScript 的静态页。**

这个站一个交互都没有：没有 `useState`、没有 `useEffect`，FAQ 折叠用原生
`<details>`，平滑滚动是 CSS 的 `scroll-behavior`。既然如此，把 React 运行时
发给访客纯属浪费 —— 226 kB 的 JS 只为画一段构建后就不再变的 HTML。

所以：**用 React 写，但不发 React**。构建期 `renderToStaticMarkup` 成字符串
塞进 HTML，再把 script 标签和 JS 文件一起删掉。

| | 之前 | 现在 |
|---|---|---|
| JS | 226 kB | **0** |
| HTML | 2.3 kB（空壳） | 10.4 kB（含全部正文） |
| 禁用 JS | 白屏 | 完整可读 |

> ⚠️ **想加交互之前先读 `src/entry-server.tsx` 的注释。**
> 一旦有组件需要 `useState`／事件处理，零 JS 就不成立了，
> 必须改成 hydrate（保留 script 标签 + `createRoot` 换 `hydrateRoot`）。
> 在那之前优先找原生 HTML 解法 —— 落地页几乎总是够用。

`base` 设为 `"./"`（相对路径）：GitHub Pages 项目站点挂在 `/wego-voice/`
子路径下，绝对路径会 404；写死子路径又会在换域名或改仓库名时再坏一次。

**首次部署需要在仓库里手动开一次**：Settings → Pages → Source 选
**GitHub Actions**。之后推送 `site/**` 就会自动构建发布
（见 `.github/workflows/pages.yml`）。

**内容集中在 `src/content.ts`，类型化。**

官网上的技术数字（延迟拆解、块大小、判定阈值）**都来自实测**，
散落在组件里迟早会和 `docs/Phase0-实测记录.md` 脱节 ——
改了实测结论却漏改官网，是最容易发生也最难发现的错误。

> ⚠️ **改这些数字时，必须同步 `docs/Phase0-实测记录.md`。**

---

## 应用内部结构

```
app/crates/voice-core/    纯 DSP：FFT + YIN 音高检测 + TD-PSOLA 修正 + 调式量化
                          零依赖、无 I/O、无 GUI —— 可直接复用为 CLAP 插件核心
app/crates/voice-audio/   实时引擎
  duplex.rs               与后端无关的核心：环形缓冲、漂移补偿、块大小自适应
  backend/                WASAPI 独占（主力）+ cpal 共享（兜底）
app/src-tauri/            Tauri 外壳：引擎持有、command 层、20Hz 指标推送
  characters.rs           角色（声线预设）定义与持久化
app/src/                  React 界面：调音 / 角色 / 诊断 三视图
```

### 噪声门：抄的是实时通话那套

`voice-core/src/noise.rs`。**非人声帧更新噪声本底，人声帧冻结** ——
这是实时通话产品的公版做法。冻结是关键：唱歌是持续信号，
无脑跟随的估计会被一个长音一路抬到人声电平，门就再也开不了了。

它解决的不是"听得到底噪"，而是两个真症状：

- 风扇、电流声有周期成分 → YIN 给出自信的基频 → **PSOLA 对着风扇修音**
- 噪声抬高 aperiodicity → 浊音误判清音 → **修音时断时续**

顺带：门关着就**根本不跑 YIN**。安静时平均回调耗时从 70~75 µs 降到
**9~10 µs**（唱起来门开了就回到原样）。

> **失败要朝开的方向倒。** 本底估到 -34 dBFS 以上就判定"估不准"，
> 门直接停用、一律放行。漏一次误触发只是瑕疵，把人声吞掉是产品事故。

**做不到实时神经降噪**：监听路径只剩 0.08 ms 预算（29.92 / 30），
而任何带分析窗的降噪最少要 +10 ms。重型降噪只能放在
**分析路径**（零音频延迟）和**离线路径**上。

### 角色：声线是怎么做的

一个「角色」把调、修正速度、整体移调、共振峰平移打成一个具名预设 ——
用户想的是"唱成少女音"这**一件**事，不是四个分别要拧的旋钮。
角色库存在 `%APPDATA%/com.wego.voice/characters.json`。

声线靠**共振峰平移**塑造：把 PSOLA 的颗粒按 α 重采样。因为输出音高只由
合成标记间距决定、与颗粒内容无关，所以这缩放的是频谱包络（≈声道长度），
**音高原封不动、延迟一毫秒不加**。实测 CPU 仅 +7% 平均 / +19% P99
（占回调预算 5%）。

> 这套能做出"像另一个人"，**做不到"像某个指定的人"** ——
> 后者需要神经声码器与说话人嵌入，受实施方案 #14 许可证审计阻塞。
> 所以功能叫「角色」不叫「变声」，界面上没有任何"克隆"字样。

### 两个后端

| 后端 | 角色 | 延迟 | 代价 |
|---|---|---|---|
| WASAPI 独占 | **主力** | **29.92 ms** | 运行期间独占声卡 |
| cpal 共享 | **兜底** | 55 ms | 超 No-Go 线；仅在独占开不成（设备被占用）时使用 |

cpal 在 Windows 上硬编码共享模式，而共享模式的块周期由音频引擎决定、
改不了（0.15 与 0.18 都查过源码）。所以低延迟只能绕开它。

> 平台已收窄为仅 Windows，cpal 的跨平台价值随之消失 ——
> 将来可用 `wasapi` crate 自带的共享模式替掉它，彻底去掉这个依赖。

---

## 设计要点

**三条架构红线**（违反即返工，详见实施方案 §3.2）：

1. 实时音频路径 100% 在 Rust 侧，采样点绝不经过 WebView 或 IPC
2. 落盘保存的必须是原始干声，修正音只用于耳返监听
3. 推理线程绝不与实时音频线程抢 CPU

**两个纪律区**（Code review 时单独看）：

- `voice-core` 的 `process` 路径 —— 实时安全：无堆分配、无锁、无日志、无 panic
- 将来的 `infer/preprocess.rs` —— 推理预处理必须与训练侧逐位对齐，
  对不齐的表现是「能出声但音色不像」，极难排查

**延迟预算**：端到端 < 30ms 是硬线。超过 50ms 会触发延迟听觉反馈（DAF）效应，
用户会不自觉结巴跑调 —— 那时产品比不开更糟。
