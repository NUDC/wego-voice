/**
 * 官网内容 —— 集中在这里，类型化。
 *
 * # 为什么不直接写进 JSX
 *
 * 官网上的技术数字（延迟拆解、块大小、判定阈值）**都来自实测**，
 * 散落在各个组件里迟早会和代码里的实测值脱节 ——
 * 改了结论却漏改官网，是最容易发生也最难发现的错误。
 *
 * 集中一处之后：改数字只改这一个文件，而且类型能挡住结构性笔误。
 *
 * ⚠️ 这些数字来自 `wego-bench` 的实测输出。改之前先重跑一遍。
 */

/** 功能完成状态。官网不宣传不存在的功能。 */
export type Status = "done" | "wip";

export interface Feature {
  title: string;
  body: string;
  points: string[];
  status: Status;
}

export interface LatencyRow {
  label: string;
  frames: string;
  /** 毫秒，**数值**。要画成堆叠条，光有字符串不够。 */
  ms: number;
  /** 条形的颜色档。设备 I/O 与 DSP 是两类东西，视觉上要分开。 */
  kind: "io" | "dsp";
}

/** 站点级事实。全部来自 `wego-bench` 实测，改之前先重跑。 */
export const FACTS = {
  /** 端到端延迟，生产配置实测值。 */
  latencyMs: "29.92",
  /** 人耳容忍上限，超过即触发 DAF 效应。 */
  dafThresholdMs: 50,
  /** 我们给自己定的硬线。 */
  budgetMs: 30,
  /** 共享模式（不可用）的对照值。 */
  sharedModeMs: "55",
  platform: "Windows 10 / 11",
} as const;

export const FEATURES: Feature[] = [
  {
    title: "① 实时音高校准",
    body: "麦克风进来的声音经过音高检测与修正后立刻送回耳返。修正力度可调 —— 从「修得不露痕迹」到「彻底电音」之间连续可选。",
    points: [
      "颤音会保留，不会把人唱成 MIDI",
      "跑调太离谱时不硬拉，避免刺耳失真",
      "按调式量化，不是简单吸附到最近半音",
    ],
    status: "done",
  },
  {
    title: "② 实时声线塑形",
    body: "共振峰平移改变「声道长短」，频谱倾斜改变「整体明暗」—— 两者都不动音高、不增加一毫秒延迟。打包成具名「角色」，选中即生效。",
    points: [
      "放一段参考音频，自动算出该拧多少",
      "纯 DSP，不需要下载任何模型",
      "做得到「像另一个人」，做不到「像某个指定的人」",
    ],
    status: "done",
  },
  {
    title: "③ 录完再换声（开发中）",
    body: "干声已经能录（落盘的是原始干声，不是修正过的）。基于神经网络的离线声线转换还在做 —— 它做不进 30 毫秒，只能放在录制之后。",
    points: [
      "全程在你自己的电脑上跑，声音不上传",
      "干声 / 修音 / 换声 分轨导出",
      "输出带 AI 生成标识",
    ],
    status: "wip",
  },
];

/** 延迟容忍度的三档。数值边界来自听觉研究，不是我们拍的。 */
export const LATENCY_SCALE = [
  {
    range: "< 20ms",
    desc: "感知为「自己的声音」",
    tone: "good" as const,
  },
  {
    range: "20–50ms",
    desc: "能察觉发飘，但不影响发声",
    tone: "ok" as const,
  },
  {
    range: "> 50ms",
    desc: "触发延迟听觉反馈效应，人会不自觉结巴、跑调",
    tone: "bad" as const,
  },
];

/** 生产配置的延迟拆解。48kHz，WASAPI 独占。 */
export const LATENCY_BUDGET: LatencyRow[] = [
  { label: "输入块", frames: "96 帧", ms: 2.0, kind: "io" },
  { label: "环形缓冲", frames: "456 帧", ms: 9.5, kind: "io" },
  { label: "输出块", frames: "144 帧", ms: 3.0, kind: "io" },
  { label: "音高修正算法", frames: "PSOLA", ms: 15.42, kind: "dsp" },
];

/** 画堆叠条时的横轴满量程。取 No-Go 线，好让"贴着线"这件事看得见。 */
export const BUDGET_SCALE_MS = FACTS.budgetMs;

export const REQUIREMENTS = {
  yes: [
    "**有线耳机**，或外置声卡 / USB 麦克风",
    FACTS.platform,
    "录音时能让本程序独占声卡 —— 运行期间系统其他声音会静音",
    "**Microsoft Edge WebView2 运行时**（微软官方免费组件，Win11 与较新的 Win10 自带）",
  ],
  no: [
    "**蓝牙耳机** —— 编解码本身就有 100~250 毫秒延迟，是无线协议的物理限制，任何软件都绕不过",
    "macOS / Linux —— 暂不支持",
    "手机 —— 不做移动端",
  ],
};

/**
 * 仓库地址。下载、「所有版本」、版本探针的 API 地址都从这里派生，不写死多份。
 *
 * ⚠️ `scripts/prerender.mjs` 用正则从本文件里抠这一行来拼 API 地址。
 * 改格式（比如换成模板串）会让构建直接报错 —— 那是故意的，
 * 总比悄悄发一个探针指着空地址的页面好。
 */
export const REPO = "https://github.com/NUDC/wego-voice";

/** Release 列表页。没有可下载版本时，按钮退到这里，不会 404。 */
export const RELEASES = `${REPO}/releases`;

export interface Release {
  tag: string;
  date: string;
  /** **免安装单文件**直链。空串表示还没有可下载的版本。 */
  url: string;
  file: string;
  /** 人类可读的大小，如 "8.4 MB"。 */
  size: string;
}

function humanSize(bytes: string | undefined): string {
  const n = Number(bytes);
  if (!n || !Number.isFinite(n)) return "";
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

/**
 * 当前发布版本 —— **构建期烤进 HTML 的那一份，是兜底不是真相**。
 *
 * 真正的版本信息由 `scripts/release-probe.js` 在页面加载后直接问
 * GitHub Release API 要（见那个文件的说明）。这里烤进去的值负责三件事：
 * 首屏不闪、禁用 JS 也能下载、API 限流或挂掉时页面仍然可用。
 *
 * 为什么两层都要：烤进去的那份会过期（在网页上手改 Release 不会触发重新部署），
 * 探针那份会失败（离线、限流）。两者失败的场景不重叠，叠起来才不留缺口。
 *
 * ⚠️ **`url` 为空是正常状态**，不是故障 —— 项目还没发版。
 * 页面必须如实显示「尚未发布」，而不是给一个点了会 404 的按钮。
 */
export const RELEASE: Release = {
  tag: import.meta.env.VITE_RELEASE_TAG ?? "",
  date: import.meta.env.VITE_RELEASE_DATE ?? "",
  url: import.meta.env.VITE_RELEASE_URL ?? "",
  file: import.meta.env.VITE_RELEASE_FILE ?? "",
  size: humanSize(import.meta.env.VITE_RELEASE_SIZE),
};

export const NAV = [
  { href: "#how", label: "怎么做到的" },
  { href: "#require", label: "运行要求" },
];
