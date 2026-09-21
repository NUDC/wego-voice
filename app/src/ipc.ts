/**
 * Rust ↔ 前端的类型化边界。
 *
 * 这些类型必须与 `src-tauri` 里的 serde 结构保持一致。
 * 字段名用 camelCase —— Rust 侧统一加了 `#[serde(rename_all = "camelCase")]`。
 *
 * ⚠️ **架构红线**：这条通道只传控制指令（低频）和标量快照（20Hz）。
 * 音频采样点永远不经过这里 —— 48kHz / 144 帧的块意味着每秒 333 次、
 * 每次 3ms 预算，而 IPC 是 JSON 过 WebView bridge，扛不住。
 * 详见 docs/实施方案.md §3.2 红线 1。
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface DeviceList {
  host: string;
  defaultInput: string | null;
  defaultOutput: string | null;
  inputs: string[];
  outputs: string[];
}

export interface BackendInfo {
  backend: string;
  /** 独占设备时系统其他声音会静音，UI 必须明确告知。 */
  exclusive: boolean;
  host: string;
  inputDevice: string;
  outputDevice: string;
  sampleRate: number;
  inputChannels: number;
  outputChannels: number;
  requestedBufferFrames: number;
  inputBlockFrames: number;
  outputBlockFrames: number;
  /** 独占模式下常被迫退让到整数格式（实测输入 int16、输出 int24-in-32）。 */
  inputFormat: string;
  outputFormat: string;
  targetFillBlocks: number;
  algorithmicMs: number;
  realtimePriority: boolean;
  /**
   * 若从首选后端退回了兜底后端，这里是原因。
   * 退回意味着延迟差 2.5 倍且越过 DAF 阈值 —— UI 必须显眼告知。
   */
  fallbackReason: string | null;
}

export interface MetricsSnapshot {
  inputFrames: number;
  outputFrames: number;
  inputCallbacks: number;
  outputCallbacks: number;
  xruns: number;
  overflows: number;
  dspUnderruns: number;
  ringFill: number;
  ringFillMin: number;
  ringFillMax: number;
  smoothedFill: number;
  driftDrops: number;
  driftInserts: number;
  netDrift: number;
  actualInputBlock: number;
  actualOutputBlock: number;
  maxOutputBlock: number;
  budgetUs: number;
  rtPromotions: number;
  rtFailures: number;
  captureGapMaxUs: number;
  renderGapMaxUs: number;
  captureStalls: number;
  renderStalls: number;
  callbackMaxUs: number;
  callbackOverHalfBudget: number;
  callbackAvgUs: number;
  measuredRtUs: number;
  algorithmicUs: number;
  f0Hz: number;
  centsOff: number;
  rms: number;
  targetMidi: number;
  voiced: boolean;
  clipping: boolean;
}

export interface Tick {
  running: boolean;
  metrics: MetricsSnapshot;
  driftPpm: number;
  uncompensated10minMs: number;
  latencyMs: number;
  latencyVerdict: string;
  targetFill: number;
}

export interface LatencyStats {
  samples: number;
  minMs: number;
  medianMs: number;
  p90Ms: number;
  maxMs: number;
  spreadMs: number;
}

export interface LatencyResult {
  stats: LatencyStats;
  detected: number;
  requested: number;
  peakSeen: number;
  noiseFloor: number;
  threshold: number;
  /** 测不到时的人话原因；成功时为空串。 */
  diagnosis: string;
}

export interface StartRequest {
  backend?: string;
  inputDevice?: string;
  outputDevice?: string;
  bufferFrames?: number;
  sampleRate?: number;
  targetFillBlocks?: number;
  f0Floor?: number;
  realtimePriority?: boolean;
}

export interface ParamUpdate {
  retuneMs?: number;
  key?: string;
  bypass?: boolean;
  monitorMuted?: boolean;
  monitorGain?: number;
  pitchShift?: number;
  formantShift?: number;
}

/**
 * 角色 —— 一个具名的声线预设。
 *
 * 「调」不再是界面上的裸参数，而是角色的一个字段：
 * 对用户来说"唱成少女音"是一件事，不是四个要分别拧的旋钮。
 *
 * ⚠️ 声线靠**共振峰平移 + 整体移调**模拟，做得到"像另一个人"，
 * 做不到"像某个指定的人" —— 后者需要神经声码器，那条线的许可证还没落地。
 */
export interface Character {
  id: string;
  name: string;
  /** 调名，如 "C"、"Am"、"F#chrom"。 */
  key: string;
  retuneMs: number;
  /** 整体移调（半音）。超过 ±5 会有明显金属感。 */
  pitchShift: number;
  /** 共振峰平移（半音）。声线的主维度，不动音高、不增加延迟。 */
  formantShift: number;
  note: string;
  /** 内置角色可改可复位，但不能删。 */
  builtin: boolean;
}

export interface CharacterStore {
  characters: Character[];
  activeId: string;
}

export const api = {
  devices: () => invoke<DeviceList>("devices"),
  start: (req: StartRequest) => invoke<BackendInfo>("start", { req }),
  stop: () => invoke<void>("stop"),
  engineInfo: () => invoke<BackendInfo | null>("engine_info"),
  tick: () => invoke<Tick>("tick"),
  resetMetrics: () => invoke<void>("reset_metrics"),
  setParams: (upd: ParamUpdate) => invoke<void>("set_params", { upd }),
  measureLatency: (rounds: number) =>
    invoke<LatencyResult>("measure_latency", { rounds }),

  charactersLoad: () => invoke<CharacterStore>("characters_load"),
  charactersSave: (store: CharacterStore) =>
    invoke<void>("characters_save", { store }),
  charactersBuiltins: () => invoke<Character[]>("characters_builtins"),
  /** 整体下发一个角色。四个参数一起生效，不会出现中间态。 */
  applyCharacter: (character: Character) =>
    invoke<void>("apply_character", { character }),
};

/** 订阅 20Hz 的指标推送。返回取消订阅函数。 */
export function onTick(cb: (t: Tick) => void): Promise<UnlistenFn> {
  return listen<Tick>("tick", (e) => cb(e.payload));
}
