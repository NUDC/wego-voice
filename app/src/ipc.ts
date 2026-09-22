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
  /** 房间噪声本底估计（线性 RMS）。 */
  noiseFloor: number;
  /** 本帧是否越过噪声门。false 说明 YIN 根本没跑。 */
  gateOpen: boolean;
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
  /** 噪声门余量（dB）。人声要高出实测本底这么多才进入音高检测。 */
  noiseGateDb?: number;
  /** 角色：频谱倾斜（dB/八度）。 */
  tiltDbPerOct?: number;
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
  /** 频谱倾斜（dB/八度）。正 = 更亮。声线的第二个维度。 */
  tiltDbPerOct: number;
  note: string;
  /** 内置角色可改可复位，但不能删。 */
  builtin: boolean;
}

/**
 * 录音状态。
 *
 * 录的是**干声**（采集侧原始输入），不是耳返里那个修正过的声音 ——
 * 架构红线 2。修正音有损且不可逆，只存它等于永久放弃了换角色重来、
 * 离线重新校准、以及送进声线转换的机会。
 */
export interface RecordingStatus {
  recording: boolean;
  seconds: number;
  /** 因缓冲满而丢弃的样本数。**必须显示** —— 悄悄丢帧比录不上更糟。 */
  dropped: number;
  path: string | null;
}

/** 录音目录里的一条 take。 */
export interface TakeInfo {
  name: string;
  path: string;
  seconds: number;
}

/**
 * 参考音频分析结果。
 *
 * ⚠️ 共振峰平移是**相对量** —— "把你的声道缩放到它那么长"。
 * 所以必须同时有参考音频和你自己的干声，只给一边算不出来。
 */
export interface TimbreSuggestion {
  /** 建议的共振峰平移（半音）。这是声线的主维度。 */
  formantShift: number;
  /** 恒为 0 —— 改了音高就不是这首歌了。 */
  pitchShift: number;
  /** 实测音高差（半音），仅供参考。 */
  pitchDelta: number;
  /** 0~1。低于 0.4 必须明说"没把握"。 */
  confidence: number;
  /** 频谱倾斜差（dB/八度）。现在能施加了（tilt.rs）。 */
  tiltDelta: number;
  sourceF0: number;
  referenceF0: number;
  sourceVoicedSecs: number;
  referenceVoicedSecs: number;
  /** 素材不合格时的人话说明；合格时为空串。 */
  warning: string;
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

  startRecording: () => invoke<RecordingStatus>("start_recording"),
  stopRecording: () => invoke<RecordingStatus>("stop_recording"),
  recordingStatus: () => invoke<RecordingStatus>("recording_status"),
  /** 在资源管理器里打开录音目录，返回路径。 */
  revealRecordings: () => invoke<string>("reveal_recordings"),

  listRecordings: () => invoke<TakeInfo[]>("list_recordings"),
  suggestCharacter: (referencePath: string, sourcePath: string) =>
    invoke<TimbreSuggestion>("suggest_character", { referencePath, sourcePath }),

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
