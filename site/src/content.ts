/**
 * 官网内容 —— 集中在这里，类型化。
 *
 * # 为什么不直接写进 JSX
 *
 * 官网上的技术数字（延迟拆解、块大小、判定阈值）**都来自实测**，
 * 散落在各个组件里迟早会和 `docs/Phase0-实测记录.md` 脱节 ——
 * 改了实测结论却漏改官网，是最容易发生也最难发现的错误。
 *
 * 集中一处之后：改数字只改这一个文件，而且类型能挡住结构性笔误。
 *
 * ⚠️ **任何数字变动，必须同步 `docs/Phase0-实测记录.md`。**
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
  ms: string;
  total?: boolean;
}

export interface FaqItem {
  q: string;
  a: string;
}

/** 站点级事实。改之前先看 docs/Phase0-实测记录.md。 */
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
    title: "① 唱的时候：实时修音耳返",
    body: "麦克风进来的声音经过音高检测与修正后立刻送回耳返。修正力度可调 —— 从「修得不露痕迹」到「彻底电音」之间连续可选。",
    points: [
      "颤音会保留，不会把人唱成 MIDI",
      "跑调太离谱时不硬拉，避免刺耳失真",
      "按调式量化，不是简单吸附到最近半音",
    ],
    status: "done",
  },
  {
    title: "② 唱完：换成想要的音色",
    body: "把录下来的干声转换成目标音色。全程在你自己的电脑上跑，声音不上传、不出本机。",
    points: [
      "干声 / 修音 / 换声 / 混音，四条轨分开导出",
      "输出带 AI 生成标识",
      "可批量排队处理",
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
  { label: "输入块", frames: "96 帧", ms: "2.00 ms" },
  { label: "环形缓冲", frames: "456 帧", ms: "9.50 ms" },
  { label: "输出块", frames: "144 帧", ms: "3.00 ms" },
  { label: "音高修正算法", frames: "—", ms: "15.42 ms" },
  { label: "端到端", frames: "", ms: `${FACTS.latencyMs} ms`, total: true },
];

export const REQUIREMENTS = {
  yes: [
    "**有线耳机**，或外置声卡 / USB 麦克风",
    FACTS.platform,
    "录音时能让本程序独占声卡",
  ],
  no: [
    "**蓝牙耳机** —— 编解码本身就有 100~250 毫秒延迟，是无线协议的物理限制，任何软件都绕不过",
    "macOS / Linux —— 暂不支持",
    "手机 —— 不做移动端",
  ],
};

export const POSITIONING = {
  is: [
    "参数全暴露，可调可存预设",
    "输出就是本地文件，能直接进 DAW",
    "拖拽导入、键盘快捷键、批量处理",
    "一页说明 + 合理默认值",
  ],
  isnt: [
    "打分、成就、进步曲线",
    "社交、分享、排行榜",
    "内置曲库（用你自己的伴奏）",
    "新手引导流程",
  ],
};

/** 仓库地址。下载与「所有版本」都从这里派生，不写死多份。 */
export const REPO = "https://github.com/NUDC/wego-voice";

export interface Release {
  tag: string;
  date: string;
  /** 安装包直链。空串表示还没有可下载的版本。 */
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
 * 当前发布版本。构建期由 CI 注入（见 .github/workflows/pages.yml）。
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

export const FAQ: FaqItem[] = [
  {
    q: "我的声音会被上传吗？",
    a: "不会。所有处理都在你自己的电脑上完成，程序运行时不需要联网。唯一的一次网络请求是首次下载音色模型（几百 MB），下载完之后可以断网使用。也会提供完整离线安装包。",
  },
  {
    q: "为什么要独占声卡？能不能不独占？",
    a: `能，但延迟会从 ${FACTS.latencyMs} 毫秒变成 ${FACTS.sharedModeMs} 毫秒以上，超过人耳的容忍线，实时耳返就失去意义了。软件里可以切换成共享模式，诊断页会照实显示当时的延迟，不隐瞒。`,
  },
  {
    q: "它能帮我唱得更准吗，还是只是修音？",
    a: "两者都有，但要诚实说明：实时听到修正后的自己，对训练音准是有帮助的，因为你能立刻听出偏差方向。不过约有百分之几的人属于先天性失歌症（感知层面分辨不出音高），实时反馈对这部分人无效 —— 但「录完直接修好」这个功能依然有用。",
  },
  {
    q: "声线模拟会被用来伪造别人的声音吗？",
    a: "我们不提供任何声线来源 —— 不上架、不托管任何人的声纹，参考素材完全由你自己提供。相应地，取得被模拟者同意是使用者的责任，软件在导入时会明确提示。我们这边的责任是：所有输出文件都带 AI 生成标识与不可听水印，这一条不可关闭。",
  },
  {
    q: "声线模拟是实时的吗？",
    a: "不是，是录完之后转换。实时修音和声线模拟是两个功能、两套延迟预算：修音必须压进 30 毫秒（否则触发延迟听觉反馈），而神经网络声线转换在这个预算内做不到 —— 零样本、流式、30 毫秒以内，这三条目前无法同时满足。硬做的结果是两个功能一起废掉，所以声线模拟放在录制之后。耳返里你听到的始终是校准过的自己的声音，这本来也是歌手需要的。",
  },
  {
    q: "为什么不做 macOS？",
    a: "低延迟这条路线是 Windows 特有的（WASAPI 独占模式），换平台等于把整套验证重做一遍。与其两个平台都做得半吊子，不如先在一个平台上做透。底层架构已经为将来留了位置。",
  },
  {
    q: "现在能用了吗？",
    a: RELEASE.url
      ? `实时修音可以用了（${RELEASE.tag}），声线模拟的离线转换还在做。下载在页面底部。`
      : "还不能。实时修音部分已经完成并通过实测，声线模拟的离线转换还在做。目前没有可下载的版本。",
  },
];

export const NAV = [
  { href: "#how", label: "怎么做到的" },
  { href: "#require", label: "硬件要求" },
  { href: "#faq", label: "常见问题" },
];
