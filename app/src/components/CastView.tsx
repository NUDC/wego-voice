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
import type { Character } from "../ipc";
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
