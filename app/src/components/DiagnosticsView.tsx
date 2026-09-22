/**
 * 诊断视图。
 *
 * 单机 App 没有遥测，用户机器上出了问题我们看不见 ——
 * 这一页就是唯一的可观测性来源。工具定位下它也是用户真正要用的功能：
 * 判断自己的声卡行不行、延迟卡在哪一段、要不要换设备。
 *
 * 它是**第二视图**，不是首页（见 TunerView 的说明）。
 */
import type { BackendInfo, LatencyResult, Tick } from "../ipc";
import { Field, Meter, Panel, Row, Segmented, Stat, type Tone } from "./ui";

function fmt(n: number | undefined, digits = 0) {
  return n === undefined || Number.isNaN(n) ? "—" : n.toFixed(digits);
}

/** 线性幅度转 dBFS。人对噪声的直觉是 dB，不是 0.0017。 */
function dbfs(linear: number | undefined) {
  if (!linear || linear <= 0) return -100;
  return 20 * Math.log10(linear);
}

/**
 * 本底高于这个值就认为估出来的不是噪声，噪声门自动停用。
 * 必须与 `voice-core/src/noise.rs` 的 `MAX_PLAUSIBLE_FLOOR` 保持一致。
 */
const MAX_PLAUSIBLE_FLOOR = 0.02;

/** -50 dBFS 以下是安静房间；-40 起弱起音会开始被门吃掉。 */
function noiseTone(floor: number): Tone {
  if (floor <= 0) return "neutral";
  if (floor >= MAX_PLAUSIBLE_FLOOR) return "alert";
  if (floor > 0.005) return "caution";
  return "good";
}

/** 30ms 是 No-Go 线；50ms 起触发延迟听觉反馈效应，产品直接失效。 */
function latencyTone(ms: number): Tone {
  if (ms <= 0) return "neutral";
  if (ms <= 25) return "good";
  if (ms <= 30) return "caution";
  return "alert";
}

export interface DiagProps {
  running: boolean;
  busy: boolean;
  tick: Tick;
  info: BackendInfo | null;
  latency: LatencyResult | null;
  onMeasure: () => void;
  onReset: () => void;
  f0Floor: number;
  onF0Floor: (v: number) => void;
  targetFill: number;
  onTargetFill: (v: number) => void;
  noiseGateDb: number;
  onNoiseGateDb: (v: number) => void;
  /** 视口尺寸与内容溢出量。用户报 bug 时要用。 */
  viewport: { w: number; h: number; overflow: number };
}

export function DiagnosticsView(p: DiagProps) {
  const { tick, info, running } = p;
  const m = tick.metrics ?? ({} as Tick["metrics"]);
  const healthy = running && !m.xruns && !m.overflows && !m.dspUnderruns;
  const stalls = (m.captureStalls ?? 0) + (m.renderStalls ?? 0);

  const cpuAvg = m.budgetUs ? (m.callbackAvgUs / m.budgetUs) * 100 : 0;
  const cpuPeak = m.budgetUs ? (m.callbackMaxUs / m.budgetUs) * 100 : 0;
  const cpuTone: Tone = !running
    ? "neutral"
    : cpuPeak > 80
      ? "alert"
      : cpuPeak > 50
        ? "caution"
        : "good";

  return (
    <div className="diag">
      <div className="stats">
        <Stat
          label="端到端延迟"
          value={running ? fmt(tick.latencyMs, 2) : "—"}
          unit={running ? "ms" : ""}
          tone={latencyTone(tick.latencyMs)}
          hint={tick.latencyVerdict || "引擎未启动"}
        />
        <Stat
          label="实时健康度"
          value={running ? (healthy ? "正常" : `${m.xruns}`) : "—"}
          unit={running && !healthy ? "次 xrun" : ""}
          tone={!running ? "neutral" : healthy ? "good" : "alert"}
          hint={
            running
              ? stalls
                ? `整机停顿 ${stalls} 次`
                : "无欠载、无溢出"
              : "引擎未启动"
          }
        />
        <Stat
          label="时钟漂移"
          value={
            running
              ? `${tick.driftPpm >= 0 ? "+" : ""}${fmt(tick.driftPpm, 1)}`
              : "—"
          }
          unit={running ? "ppm" : ""}
          tone={running ? "good" : "neutral"}
          hint={
            running
              ? `不补偿 10 分钟累积 ${fmt(Math.abs(tick.uncompensated10minMs), 1)} ms`
              : "引擎未启动"
          }
        />
        <Stat
          label="CPU 占回调预算"
          value={running ? `${fmt(cpuAvg)}/${fmt(cpuPeak)}` : "—"}
          unit={running ? "%" : ""}
          tone={cpuTone}
          hint={running ? "平均 / 峰值" : "引擎未启动"}
        />
      </div>

      <div className="panels">
        <Panel title="延迟构成">
          <Row
            label="设备 I/O"
            value={`${fmt(tick.latencyMs - (info?.algorithmicMs ?? 0), 2)} ms`}
          />
          <Row label="DSP 算法（PSOLA）" value={`${fmt(info?.algorithmicMs, 2)} ms`} />
          <Row
            label="块大小 输入 / 输出"
            value={
              info ? `${info.inputBlockFrames} / ${info.outputBlockFrames} 帧` : "—"
            }
          />
          <Row
            label="采样格式"
            value={
              info
                ? `${info.inputFormat.split(" ")[0]} / ${info.outputFormat.split(" ")[0]}`
                : "—"
            }
            muted
          />
          <p className="hint">
            这是<b>理论下界</b>，不含驱动与硬件固有延迟。真实数字用下方脉冲实测。
          </p>
        </Panel>

        <Panel title="实时健康度" right={
          <button className="btn tiny" onClick={p.onReset} disabled={!running}>
            清零
          </button>
        }>
          <Row
            label="xrun（输出欠载）"
            value={m.xruns ?? 0}
            tone={m.xruns ? "alert" : "good"}
          />
          <Row
            label="overflow（输入溢出）"
            value={m.overflows ?? 0}
            tone={m.overflows ? "alert" : "good"}
          />
          <Row
            label="DSP 内部欠载"
            value={m.dspUnderruns ?? 0}
            tone={m.dspUnderruns ? "alert" : "good"}
          />
          <Row
            label="实时优先级"
            value={running ? `${m.rtPromotions ?? 0} / 2 线程` : "—"}
            tone={m.rtFailures ? "alert" : "good"}
          />
          <Row
            label="整机停顿 采集/渲染"
            value={`${m.captureStalls ?? 0} / ${m.renderStalls ?? 0}`}
            tone={stalls ? "caution" : "good"}
          />
          <Row
            label="最长间隔 采集/渲染"
            value={`${fmt(m.captureGapMaxUs)} / ${fmt(m.renderGapMaxUs)} µs`}
            muted
          />
          {running && stalls > 0 && (
            <p className="hint">
              两侧<b>同时</b>出现远超正常值的间隔，说明是整机冻结
              （电源管理、虚拟机暂停、杀毒扫描等），不是本程序算不过来。
            </p>
          )}
        </Panel>

        <Panel title="输入噪声">
          {(() => {
            const floor = m.noiseFloor ?? 0;
            const disabled = floor >= MAX_PLAUSIBLE_FLOOR;
            return (
              <>
                <Row
                  label="房间本底"
                  value={running ? `${fmt(dbfs(floor), 1)} dBFS` : "—"}
                  tone={running ? noiseTone(floor) : "neutral"}
                />
                <Row
                  label="噪声门限"
                  value={
                    running && !disabled
                      ? `${fmt(dbfs(floor) + p.noiseGateDb, 1)} dBFS`
                      : running
                        ? "已停用"
                        : "—"
                  }
                  tone={disabled && running ? "caution" : "neutral"}
                />
                <Row
                  label="当前状态"
                  value={
                    !running
                      ? "—"
                      : m.gateOpen
                        ? m.voiced
                          ? "放行 · 检测到人声"
                          : "放行 · 未判定为人声"
                        : "关闭 · 已跳过音高检测"
                  }
                  muted
                />
                {running && disabled && (
                  <p className="hint">
                    本底高过合理上限，<b>噪声门已自动停用</b> ——
                    估出来的显然不是噪声（多半是启动时你已经在唱，
                    或者环境实在太吵）。停一下不出声，一两秒就能重新学到。
                    <br />
                    这是刻意的：<b>门宁可失效，也不能把人声吞掉。</b>
                  </p>
                )}
                {running && !disabled && floor > 0.005 && (
                  <p className="hint">
                    环境偏吵，弱起音和收尾气声可能被门挡掉。
                    换个安静点的地方，或把下面的余量调小。
                  </p>
                )}
                {running && !disabled && floor <= 0.005 && (
                  <p className="hint">
                    本底越低越好。门的作用是<b>不让风扇、电流声这类
                    有周期成分的噪声被当成人声修音</b>，
                    同时省掉静音段的音高检测开销。
                  </p>
                )}

                {/* 这一项运行中可改 —— 和下面「引擎参数」那两个不同，
                    它不需要重建音频链路，调完立刻听得出来。 */}
                <Field
                  label="门限余量"
                  hint="人声要高出本底多少 dB 才放行。调高更不容易被噪声误触发，但会吃掉弱起音。"
                >
                  <div className="slider-row">
                    <input
                      type="range"
                      min={0}
                      max={30}
                      step={1}
                      value={p.noiseGateDb}
                      onChange={(e) => p.onNoiseGateDb(Number(e.target.value))}
                    />
                    <span className="slider-val mono">
                      {p.noiseGateDb === 0 ? "关" : `${p.noiseGateDb} dB`}
                    </span>
                  </div>
                </Field>
              </>
            );
          })()}
        </Panel>

        <Panel title="时钟与缓冲">
          <Row
            label="环形缓冲水位"
            value={running ? `${m.ringFill ?? 0} / ${tick.targetFill}` : "—"}
          />
          {running && (
            <div className="fill-meter">
              <Meter
                value={tick.targetFill ? m.ringFill / (tick.targetFill * 2) : 0}
                tone="neutral"
                marker={0.5}
              />
            </div>
          )}
          <Row
            label="补偿 丢帧 / 插帧"
            value={`${m.driftDrops ?? 0} / ${m.driftInserts ?? 0}`}
          />
          <p className="hint">
            独占模式下音频不经过系统混音器，两台设备的时钟各走各的 ——
            漂移是<b>正常现象</b>，补偿器会持续拉回。水位贴着左边才是危险信号。
          </p>
        </Panel>

        <Panel
          title="往返延迟实测"
          right={
            <button
              className="btn tiny"
              onClick={p.onMeasure}
              disabled={!running || p.busy}
            >
              {p.busy ? "测量中…" : "测量 25 轮"}
            </button>
          }
        >
          {p.latency && p.latency.detected > 0 ? (
            <>
              <div className={`big-num num tone-${latencyTone(p.latency.stats.medianMs)}`}>
                {fmt(p.latency.stats.medianMs, 2)}
                <span className="big-unit">ms 中位数</span>
              </div>
              <Row
                label="最小 / P90 / 最大"
                value={`${fmt(p.latency.stats.minMs, 1)} / ${fmt(p.latency.stats.p90Ms, 1)} / ${fmt(p.latency.stats.maxMs, 1)} ms`}
              />
              <Row
                label="测得样本"
                value={`${p.latency.detected} / ${p.latency.requested}`}
              />
              {p.latency.stats.spreadMs > 5 && (
                <p className="hint">
                  离散度 {fmt(p.latency.stats.spreadMs, 1)} ms 偏大，
                  说明系统调度不稳定，本身就值得追查。
                </p>
              )}
            </>
          ) : p.latency ? (
            <div className="notice alert">
              <b>一次也没测到。</b>
              <br />
              {p.latency.diagnosis}
              <br />
              <span className="mono faint">
                峰值 {p.latency.peakSeen.toFixed(4)} ／ 本底{" "}
                {p.latency.noiseFloor.toFixed(4)} ／ 阈值{" "}
                {p.latency.threshold.toFixed(4)}
              </span>
            </div>
          ) : (
            <p className="hint">
              需要回环装置：<b>回环线</b>（输出口→输入口）最准；
              也可把耳机贴住麦克风，但要从结果里扣掉声程（约 3 ms/米）。
              保持耳返静音即可 —— 脉冲照发，同时避免啸叫回路。
            </p>
          )}
        </Panel>

        <Panel title="引擎参数" className="span-2">
          <div className="param-grid">
            <Field
              label="PSOLA 基频下限"
              hint="决定 DSP 延迟。调高则延迟降低，但低于该频率的男声音质下降。"
            >
              <Segmented
                value={p.f0Floor}
                onChange={p.onF0Floor}
                disabled={running}
                options={[
                  { value: 100, label: "100 Hz", title: "DSP 20.0 ms，男低音最佳" },
                  { value: 115, label: "115 Hz", title: "DSP 17.4 ms" },
                  { value: 130, label: "130 Hz", title: "DSP 15.4 ms（推荐）" },
                  { value: 160, label: "160 Hz", title: "DSP 12.5 ms，仅女声/童声" },
                ]}
              />
            </Field>
            <Field
              label="环形缓冲水位"
              hint="决定抗抖动能力。每加一档增加约一个输出块（3 ms）的延迟。"
            >
              <Segmented
                value={p.targetFill}
                onChange={p.onTargetFill}
                disabled={running}
                options={[
                  { value: 1.5, label: "1.5", title: "延迟最低，但实测会 xrun" },
                  { value: 2.5, label: "2.5", title: "实测安全下限（推荐）" },
                  { value: 3.5, label: "3.5", title: "更稳，多 3 ms 延迟" },
                  { value: 4.5, label: "4.5", title: "机器不稳定时用" },
                ]}
              />
            </Field>
          </div>
          <p className="hint">
            两个参数都要<b>停止引擎后</b>才能改。
          </p>
        </Panel>

        <Panel title="环境" className="span-2">
          {info && (
            <>
              <Row label="后端" value={info.backend} />
              <Row
                label="输入"
                value={`${info.inputDevice}（${info.inputFormat}）`}
                muted
              />
              <Row
                label="输出"
                value={`${info.outputDevice}（${info.outputFormat}）`}
                muted
              />
              <Row label="采样率" value={`${info.sampleRate} Hz`} />
            </>
          )}
          <Row
            label="窗口视口"
            value={`${p.viewport.w} × ${p.viewport.h}`}
            muted
          />
          <Row
            label="界面溢出"
            value={p.viewport.overflow > 0 ? `${p.viewport.overflow} px` : "无"}
            tone={p.viewport.overflow > 0 ? "caution" : "good"}
          />
        </Panel>
      </div>
    </div>
  );
}
