/**
 * 角色管理。
 *
 * # 一个角色是什么
 *
 * 「唱出来是什么样」所需的全部参数，打成一个具名包：
 * 调、修正速度、整体移调、共振峰平移。
 *
 * 界面上原本裸露的「调」被收进来了 —— 对用户来说"我要唱成少女音"
 * 是一件事，不是四个要分别拧的旋钮。
 *
 * # 选中 = 试听 = 编辑
 *
 * 刻意不做「选一个」和「编一个」两套选中态。声线是**听**出来的，
 * 不是看参数看出来的：点中谁就立刻换成谁，拖滑杆时声音当场就变。
 * 引擎没启动时也能改，只是听不到，页脚会说明这一点。
 *
 * # 诚实边界
 *
 * 这里模拟声线靠两个维度：共振峰平移（不动音高、不加延迟）和整体移调。
 * 能做到"像另一个人"，**做不到"像某个指定的人"** —— 后者要神经声码器，
 * 而那条线的许可证问题是实施方案里唯一还没关掉的 🔴。
 * 所以这一页从头到尾不出现"克隆""换成 XX 的声音"这类说法。
 */
import { useEffect, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { api, type Character, type TakeInfo, type TimbreSuggestion } from "../ipc";
import { Field, Panel, Segmented } from "./ui";

const TONICS = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

/** 把 "F#m" 拆成 ["F#", "m"]。解析失败时退回 C 大调，不抛错。 */
function splitKey(key: string): [string, string] {
  const m = /^([A-G][#b]?)(.*)$/.exec((key ?? "").trim());
  if (!m) return ["C", ""];
  return [m[1] ?? "C", m[2] ?? ""];
}

function semis(v: number) {
  return `${v > 0 ? "+" : ""}${v.toFixed(1)}`;
}

/** 共振峰的方向对用户毫无直觉，必须翻译成听感。 */
function formantWord(v: number) {
  if (Math.abs(v) < 0.25) return "与原声相同";
  if (v > 0) return `声道更短 —— 更细、更"小只"`;
  return `声道更长 —— 更粗、更"大只"`;
}

/** 倾斜的方向同样没有直觉，翻译成听感。 */
function tiltWord(v: number) {
  if (Math.abs(v) < 0.1) return "与原声相同";
  if (v > 0) return "更亮、更薄 —— 齿音与气息更突出";
  return "更暗、更厚 —— 低频占比更高";
}

function pitchWord(v: number) {
  if (Math.abs(v) < 0.25) return "不移调";
  return `整体${v > 0 ? "升" : "降"} ${Math.abs(v).toFixed(1)} 个半音`;
}

export interface CastProps {
  characters: Character[];
  activeId: string;
  onActiveId: (id: string) => void;
  onPatch: (id: string, patch: Partial<Character>) => void;
  onCreate: () => void;
  onDuplicate: (id: string) => void;
  onDelete: (id: string) => void;
  onResetBuiltin: (id: string) => void;
  running: boolean;
  saveError: string;
}

export function CastView(p: CastProps) {
  const active = p.characters.find((c) => c.id === p.activeId);
  const [tonic, scale] = splitKey(active?.key ?? "C");
  const patch = (x: Partial<Character>) => active && p.onPatch(active.id, x);

  return (
    <div className="cast">
      <aside className="cast-list">
        <div className="cast-list-head">
          <h2>角色</h2>
          <button className="btn tiny" onClick={p.onCreate} type="button">
            新建
          </button>
        </div>

        <div className="cast-items">
          {p.characters.map((c) => (
            <button
              key={c.id}
              type="button"
              className={`cast-item ${c.id === p.activeId ? "on" : ""}`}
              onClick={() => p.onActiveId(c.id)}
            >
              <span className="cast-item-name">
                {c.name}
                {c.builtin && <em>内置</em>}
              </span>
              <span className="cast-item-sub mono">
                {c.key} · 移调 {semis(c.pitchShift)} · 共振峰{" "}
                {semis(c.formantShift)}
              </span>
            </button>
          ))}
        </div>

        <p className="hint">
          选中即<b>立刻生效</b>。引擎运行时拖滑杆声音会当场改变 ——
          声线要靠听，不是靠看参数。
        </p>
      </aside>

      <div className="cast-edit">
        {!active ? (
          <Panel title="角色">
            <p className="hint">角色库是空的。点左上角「新建」建一个。</p>
          </Panel>
        ) : (
          <>
            <Panel
              title="基本"
              right={
                <div className="cast-acts">
                  <button
                    className="btn tiny"
                    onClick={() => p.onDuplicate(active.id)}
                    type="button"
                  >
                    复制
                  </button>
                  {active.builtin ? (
                    <button
                      className="btn tiny"
                      onClick={() => p.onResetBuiltin(active.id)}
                      type="button"
                      title="恢复这个内置角色的出厂参数"
                    >
                      复位
                    </button>
                  ) : (
                    <button
                      className="btn tiny danger"
                      onClick={() => p.onDelete(active.id)}
                      type="button"
                    >
                      删除
                    </button>
                  )}
                </div>
              }
            >
              <div className="cast-grid">
                <Field label="名称">
                  <input
                    className="text"
                    value={active.name}
                    maxLength={16}
                    onChange={(e) => patch({ name: e.target.value })}
                  />
                </Field>

                <Field label="调" hint="修音把你唱的音吸附到这个调的音阶上">
                  <div className="key-pick">
                    <select
                      value={tonic}
                      onChange={(e) => patch({ key: e.target.value + scale })}
                    >
                      {TONICS.map((t) => (
                        <option key={t} value={t}>
                          {t}
                        </option>
                      ))}
                    </select>
                    <Segmented
                      value={scale}
                      onChange={(s) => patch({ key: tonic + s })}
                      options={[
                        { value: "", label: "大调" },
                        { value: "m", label: "小调" },
                        { value: "chrom", label: "半音", title: "不做调式限制" },
                      ]}
                    />
                  </div>
                </Field>
              </div>

              <Field label="一句话说明" hint="选角色时显示，帮你不试听也能想起它是什么">
                <input
                  className="text wide"
                  value={active.note}
                  maxLength={40}
                  onChange={(e) => patch({ note: e.target.value })}
                />
              </Field>
            </Panel>

            <Panel title="声线">
              <Knob
                label="共振峰平移"
                value={active.formantShift}
                min={-8}
                max={8}
                step={0.5}
                unit="半音"
                read={formantWord(active.formantShift)}
                onChange={(v) => patch({ formantShift: v })}
              />
              <p className="hint">
                声线的<b>主维度</b>：只缩放频谱包络，<b>不动音高、不增加一毫秒延迟</b>。
                失真也最小 —— 想要自然的声线变化，优先动这一根。
              </p>

              <Knob
                label="频谱倾斜"
                value={active.tiltDbPerOct}
                min={-4}
                max={4}
                step={0.1}
                unit="dB/八度"
                read={tiltWord(active.tiltDbPerOct)}
                onChange={(v) => patch({ tiltDbPerOct: v })}
              />
              <p className="hint">
                声线的<b>第二个维度</b>：共振峰管"声道多长"，倾斜管"整体明暗"。
                两级一阶滤波器，同样<b>不增加缓冲延迟</b>。
              </p>

              <Knob
                label="整体移调"
                value={active.pitchShift}
                min={-12}
                max={12}
                step={0.5}
                unit="半音"
                read={pitchWord(active.pitchShift)}
                onChange={(v) => patch({ pitchShift: v })}
                tone={Math.abs(active.pitchShift) > 5 ? "caution" : undefined}
              />
              {Math.abs(active.pitchShift) > 5 && (
                <p className="hint warn">
                  超过 5 个半音，PSOLA 会出现<b>明显金属感</b>。
                  想换性别感，先加共振峰、少加移调。
                </p>
              )}

              <Knob
                label="修正速度"
                value={active.retuneMs}
                min={0}
                max={120}
                step={5}
                unit="ms"
                read={
                  active.retuneMs === 0
                    ? "瞬间吸附 —— 电音效果"
                    : active.retuneMs <= 20
                      ? "很紧，修得听得出来"
                      : active.retuneMs <= 60
                        ? "自然，听不出修过"
                        : "很松，几乎不修"
                }
                onChange={(v) => patch({ retuneMs: v })}
              />
            </Panel>

            <FromReference onApply={patch} />

            <Panel title="这套方案能做到什么">
              <p className="hint">
                共振峰 + 移调能做出<b>可控的声线</b>（更细/更粗、更高/更低），
                也就是"像另一个人"。但它<b>做不到"像某个指定的人"</b> ——
                那需要神经声码器与说话人嵌入，延迟和许可证都还没有结论
                （实施方案定调表 #14，是目前唯一未关闭的阻塞项）。
                所以这里叫「角色」而不是「变声」：承诺的是声线塑形，不是克隆。
              </p>
            </Panel>
          </>
        )}
      </div>

      {(p.saveError || !p.running) && (
        <p className={`cast-foot ${p.saveError ? "warn" : ""}`}>
          {p.saveError
            ? `角色没能存盘：${p.saveError}`
            : "引擎未启动 —— 现在可以改参数，但听不到效果。回「调音」页启动后再来调最省事。"}
        </p>
      )}
    </div>
  );
}

/**
 * 参考音频 → 角色参数。
 *
 * # 为什么要两份素材
 *
 * 共振峰平移是**相对量**："把你的声道缩放到它那么长"。
 * 只给参考音频算不出来 —— 必须知道你自己的起点在哪。
 * 所以源素材用你自己录的干声，这也是录制功能存在的另一个理由。
 *
 * # 为什么只落一个参数
 *
 * 分析能给出音高差和频谱倾斜差，但**只有共振峰是引擎当前能施加的**。
 * 音高差故意不用（改了就不是这首歌了），倾斜差没有 EQ 环节可落。
 * 这两项如实显示、但不写进角色 —— 报出来是为了说清"还差在哪"，
 * 不是假装已经做到了。
 */
function FromReference({
  onApply,
}: {
  onApply: (patch: { formantShift: number; tiltDbPerOct: number }) => void;
}) {
  const [takes, setTakes] = useState<TakeInfo[]>([]);
  const [source, setSource] = useState("");
  const [reference, setReference] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  const [result, setResult] = useState<TimbreSuggestion | null>(null);

  useEffect(() => {
    api
      .listRecordings()
      .then((t) => {
        setTakes(t);
        setSource((s) => s || t[0]?.path || "");
      })
      .catch((e) => setErr(String(e)));
  }, []);

  // 拖拽接文件路径。
  //
  // 走 Tauri 的原生 drag-drop 事件而不是 HTML5 的：浏览器的 File 对象
  // **拿不到真实路径**，只能读出字节再传给 Rust —— 一段 2 分钟的 WAV
  // 有二十多 MB，过 IPC 传它纯属自找麻烦。
  useEffect(() => {
    const un = getCurrentWebview().onDragDropEvent((e) => {
      if (e.payload.type !== "drop") return;
      const p = e.payload.paths.find((x) => x.toLowerCase().endsWith(".wav"));
      if (p) {
        setReference(p);
        setResult(null);
        setErr("");
      } else {
        setErr("只认 WAV 文件");
      }
    });
    return () => {
      un.then((f) => f()).catch(() => {});
    };
  }, []);

  const run = async () => {
    setBusy(true);
    setErr("");
    try {
      setResult(await api.suggestCharacter(reference, source));
    } catch (e) {
      setErr(String(e));
      setResult(null);
    } finally {
      setBusy(false);
    }
  };

  const fileName = (p: string) => p.split(/[\\/]/).pop() ?? p;

  return (
    <Panel title="从参考音频生成">
      <div className="cast-grid">
        <Field label="参考音频" hint="把 WAV 拖进窗口任意位置">
          <div className={`drop ${reference ? "has" : ""}`}>
            {reference ? fileName(reference) : "拖一个 WAV 进来"}
          </div>
        </Field>

        <Field label="我的干声" hint="用你自己的录音当起点 —— 共振峰平移是相对量">
          <select value={source} onChange={(e) => setSource(e.target.value)}>
            {takes.length === 0 && <option value="">还没有录音</option>}
            {takes.map((t) => (
              <option key={t.path} value={t.path}>
                {t.name}（{t.seconds.toFixed(1)}s）
              </option>
            ))}
          </select>
        </Field>
      </div>

      <button
        className="btn primary"
        onClick={run}
        disabled={!reference || !source || busy}
        type="button"
      >
        {busy ? "分析中…" : "分析"}
      </button>

      {err && <p className="hint warn">{err}</p>}

      {result && (
        <>
          <div className="suggest">
            <div className="suggest-main">
              <span className="suggest-num mono">
                {result.formantShift > 0 ? "+" : ""}
                {result.formantShift.toFixed(1)}
              </span>
              <span className="suggest-unit">半音共振峰</span>
              <button
                className="btn tiny"
                onClick={() =>
                  onApply({
                    formantShift: result.formantShift,
                    // 倾斜差直接就是要施加的量：把我的明暗推到它那里
                    tiltDbPerOct: Math.max(-4, Math.min(4, result.tiltDelta)),
                  })
                }
                type="button"
              >
                应用到当前角色
              </button>
            </div>
            <div className="suggest-sub mono">
              倾斜 {result.tiltDelta > 0 ? "+" : ""}
              {result.tiltDelta.toFixed(1)} dB/八度 · 把握{" "}
              {(result.confidence * 100).toFixed(0)}% ·
              你 {result.sourceF0.toFixed(0)}Hz / 参考 {result.referenceF0.toFixed(0)}Hz
            </div>
          </div>

          {result.warning && <p className="hint warn">{result.warning}</p>}

          <p className="hint">
            这是个<b>起点，不是答案</b>。合成素材上实测估计偏保守约 20%
            （真值 +3.9 时给出 +3.0）—— 应用之后拿滑杆凭耳朵再推一点，
            通常会更像。声线是听出来的。
          </p>

          <p className="hint">
            <b>音高差 {result.pitchDelta > 0 ? "+" : ""}
            {result.pitchDelta.toFixed(1)} 半音，刻意不采用。</b>
            参考音源比你高不代表你该升调去唱 —— 那就不是这首歌了，
            而且 PSOLA 超过 ±5 半音会有明显金属感。
            「像另一个人」这件事主要由共振峰承担。
            <br />
            <br />
            <b>共振峰和倾斜会一起落下去。</b>共振峰管声道长短，
            倾斜管整体明暗 —— 两个维度都对上，才谈得上"像"。
          </p>
        </>
      )}
    </Panel>
  );
}

function Knob({
  label,
  value,
  min,
  max,
  step,
  unit,
  read,
  onChange,
  tone,
}: {
  label: string;
  value: number;
  min: number;
  max: number;
  step: number;
  unit: string;
  read: string;
  onChange: (v: number) => void;
  tone?: "caution";
}) {
  return (
    <div className="knob">
      <div className="knob-head">
        <span className="knob-label">{label}</span>
        <span className={`knob-val mono ${tone ? `tone-${tone}` : ""}`}>
          {step < 1 && value > 0 ? "+" : ""}
          {step < 1 ? value.toFixed(1) : value}
          <em>{unit}</em>
        </span>
      </div>
      <input
        type="range"
        min={min}
        max={max}
        step={step}
        value={value}
        onChange={(e) => onChange(Number(e.target.value))}
      />
      <span className="knob-read">{read}</span>
    </div>
  );
}
