/**
 * 调音视图 —— 应用的主界面，**只有一个页面、一套布局**。
 *
 * # 为什么不分「开始页」和「运行页」
 *
 * 最早是两整屏：未启动一屏空状态，点启动后整屏换成调音界面。
 * 后来合成一页，但底部控制台仍然按状态整块换内容 ——
 * 那还是「两套界面挤在同一个位置」，切换瞬间所有控件都在跳。
 *
 * 现在是同一套布局从头到尾不变：
 *
 * - 音高显示**一直在**，未启动时是安静的占位态
 * - 控制台的槽位**固定**，启动前后位置一一对应，只有可用状态和数值在变
 * - 主按钮占同一个位置，文案在「启动引擎 / 停止」之间切换
 * - 底部说明行**永远存在**（内容随状态换），所以不会有行高突变把界面顶动
 *
 * # 哪些参数能在启动前设
 *
 * 调式、修正速度、耳返开关都能 —— `App.start()` 在引擎起来之后会立刻
 * `setParams` 把它们下发下去。所以没有理由启动前把它们藏起来：
 * 藏起来只会让用户以为「得先启动才能配」。
 *
 * 真正只能停止时改的是**设备与音频模式**（要重新协商独占模式），
 * 它们运行时原地禁用，而不是消失。
 */
import type { PitchSample } from "./PitchDisplay";
import { PitchDisplay } from "./PitchDisplay";
import { Field, Meter, Segmented } from "./ui";
import type { Character, DeviceList, RecordingStatus } from "../ipc";

export interface TunerProps {
  running: boolean;
  busy: boolean;
  sample: PitchSample;
  devices: DeviceList | null;

  backend: string;
  onBackend: (v: string) => void;
  inputDevice: string;
  onInputDevice: (v: string) => void;
  outputDevice: string;
  onOutputDevice: (v: string) => void;

  muted: boolean;
  onMuted: (v: boolean) => void;

  /** 角色库与当前角色。「调」已经是角色的一个字段，不再单独出现在这里。 */
  characters: Character[];
  activeId: string;
  onActiveId: (id: string) => void;
  /** 修正速度直接改在当前角色上 —— 角色是唯一的真相来源。 */
  onRetuneMs: (v: number) => void;
  /** 跳到角色管理页。 */
  onManage: () => void;

  /** 录音。录的是干声 —— 见 ipc.ts 的 RecordingStatus 说明。 */
  rec: RecordingStatus;
  onRecord: () => void;
  /** 跳到录音页。录完之后"它去哪了"必须有一个当场能点的答案。 */
  onTakes: () => void;

  onStart: () => void;
  onStop: () => void;
}

function mmss(secs: number) {
  const s = Math.max(0, Math.floor(secs));
  return `${String(Math.floor(s / 60)).padStart(2, "0")}:${String(s % 60).padStart(2, "0")}`;
}

export function TunerView(p: TunerProps) {
  const locked = p.running || p.busy;
  const active = p.characters.find((c) => c.id === p.activeId);

  return (
    <div className="tuner">
      <div className="stage">
        <PitchDisplay sample={p.sample} running={p.running} />
        {!p.running && (
          <div className="stage-veil">
            <div className="veil-card">
              <span className="veil-tag">引擎未启动</span>
              <p>
                启动后这里显示你唱的<b>音名</b>、偏差<b>音分</b>，
                以及最近 8 秒的音准走势。
              </p>
            </div>
          </div>
        )}
      </div>

      <div className="console">
        <div className="console-rows">
          {/* 第一行：设备链路。运行中只能看，不能改。 */}
          <div className={`console-row ${locked ? "locked" : ""}`}>
            <Field label="输入设备">
              <select
                value={p.inputDevice}
                onChange={(e) => p.onInputDevice(e.target.value)}
                disabled={locked}
              >
                <option value="">默认（{p.devices?.defaultInput ?? "—"}）</option>
                {p.devices?.inputs.map((d) => (
                  <option key={d} value={d}>
                    {d}
                  </option>
                ))}
              </select>
            </Field>

            <Field label="输出设备">
              <select
                value={p.outputDevice}
                onChange={(e) => p.onOutputDevice(e.target.value)}
                disabled={locked}
              >
                <option value="">默认（{p.devices?.defaultOutput ?? "—"}）</option>
                {p.devices?.outputs.map((d) => (
                  <option key={d} value={d}>
                    {d}
                  </option>
                ))}
              </select>
            </Field>

            <Field label="音频模式" className="f-mode">
              <Segmented
                value={p.backend}
                onChange={p.onBackend}
                disabled={locked}
                options={[
                  { value: "auto", label: "自动", title: "优先独占，失败回退共享" },
                  { value: "wasapi", label: "低延迟", title: "WASAPI 独占，约 30 ms" },
                  { value: "cpal", label: "兼容", title: "共享模式，约 55 ms —— 耳返不可用" },
                ]}
              />
            </Field>
          </div>

          {/* 第二行：演唱参数。启动前后都能改，改完立刻生效。 */}
          <div className="console-row">
            <Field label="角色" className="f-cast">
              <div className="cast-pick">
                <select
                  value={p.activeId}
                  onChange={(e) => p.onActiveId(e.target.value)}
                >
                  {p.characters.map((c) => (
                    <option key={c.id} value={c.id}>
                      {c.name}
                    </option>
                  ))}
                </select>
                <button className="btn tiny" onClick={p.onManage} type="button">
                  管理
                </button>
              </div>
            </Field>

            <Field
              label={`修正速度 ${retuneWord(active?.retuneMs ?? 40)}`}
              className="f-retune"
            >
              <div className="slider-row">
                <input
                  type="range"
                  min={0}
                  max={120}
                  step={5}
                  value={active?.retuneMs ?? 40}
                  onChange={(e) => p.onRetuneMs(Number(e.target.value))}
                />
                <span className="slider-val mono">
                  {active?.retuneMs ?? 40} ms
                </span>
              </div>
            </Field>

            <Field label="输入电平" className="f-level">
              <div className="level">
                <Meter
                  value={p.running ? Math.min(1, (p.sample.rms ?? 0) * 6) : 0}
                  tone={p.sample.clipping ? "alert" : "good"}
                />
                <span className="clip">{p.sample.clipping ? "削顶" : ""}</span>
              </div>
            </Field>
          </div>
        </div>

        {/* 动作列：跨两行，位置固定。 */}
        <div className="console-act">
          <button
            className={`btn mon ${p.muted ? "" : "hot"}`}
            onClick={() => p.onMuted(!p.muted)}
            title={
              p.muted
                ? "打开耳返 —— 需要戴耳机"
                : "耳返已开 —— 外放会啸叫"
            }
          >
            {p.muted ? <IconMute /> : <IconSound />}
            耳返{p.muted ? "静音" : "开启"}
          </button>

          <button
            className={`btn wide ${p.running ? "danger" : "primary"}`}
            onClick={p.running ? p.onStop : p.onStart}
            disabled={p.busy}
          >
            {p.busy ? "启动中…" : p.running ? "停止" : "启动引擎"}
          </button>
        </div>

        {/* ── 录音条 ──

            录音是通往「录完再换声线」那条路的入口，也是这个应用唯一会
            留下东西的动作 —— 它需要一块自己的地方，而不是动作列里一个
            和「耳返」并排的小按钮。

            每个槽位**常驻**：计时器空闲时显示 00:00、丢帧计数为 0 时也占位。
            出现/消失会让这一行的高度跳变，进而把整个控制台顶动。 */}
        <div className={`rec-strip ${p.rec.recording ? "on" : ""}`}>
          <button
            className={`btn rec ${p.rec.recording ? "on" : ""}`}
            onClick={p.onRecord}
            disabled={!p.running || p.busy}
            title={
              p.running
                ? p.rec.recording
                  ? "停止并保存"
                  : "录制干声 —— 声线转换与离线校准都用它"
                : "采集链路在引擎里，引擎没起来就没有声音可录"
            }
            type="button"
          >
            <span className="rec-dot" />
            {p.rec.recording ? "停止并保存" : "录制"}
          </button>

          <span className="rec-time mono">{mmss(p.rec.seconds)}</span>

          <span className="rec-say">
            {!p.running ? (
              <>录音需要先<b>启动引擎</b> —— 采集链路在引擎里。</>
            ) : p.rec.recording ? (
              <>
                正在录<b>干声</b>，不是耳返里那个修正过的声音。
              </>
            ) : (
              <>录<b>干声</b>：换角色重来、离线校准、将来送进声线转换，都从它出发。</>
            )}
          </span>

          {/* 丢帧必须报出来，而且要**常驻**一个槽位：
              拿着有断裂的素材去做后续处理，事后根本查不出原因。 */}
          <span className={`rec-drop mono ${p.rec.dropped > 0 ? "bad" : ""}`}>
            {p.rec.dropped > 0 ? `丢帧 ${p.rec.dropped}` : "丢帧 0"}
          </span>

          <span className="rec-last">
            {p.rec.path ? (
              <>
                {p.rec.recording ? "正在写入：" : "已保存："}
                <b title={p.rec.path}>{p.rec.path.split(/[\\/]/).pop()}</b>
              </>
            ) : (
              <span className="dim">本次还没有录音</span>
            )}
          </span>

          <button className="btn tiny" onClick={p.onTakes} type="button">
            录音页
          </button>
        </div>

        {/* 说明行永远存在，只换内容 —— 否则出现/消失会把整个控制台顶动。
            录音相关的状态已经归上面那条，这里只管引擎与耳返。 */}
        <p className={`console-note ${p.rec.dropped > 0 || (p.running && !p.muted) ? "warn" : ""}`}>
          {p.rec.dropped > 0 ? (
            <>
              ⚠️ 录音<b>丢了 {p.rec.dropped} 个样本</b> ——
              文件里会有细微断裂。多半是磁盘被占住了（杀毒扫描、同步盘）。
            </>
          ) : !p.running ? (
            <>
              <b>请戴有线耳机</b>：蓝牙做不了实时耳返，延迟是无线协议的物理限制。
              设备与模式会在启动后锁定，角色和修正速度则随时可调。
            </>
          ) : p.muted ? (
            <>
              {active && (
                <>
                  角色<b>「{active.name}」</b>：{active.note}
                  {" · "}
                </>
              )}
              耳返<b>静音中</b> —— 打开后才能边唱边听到。
            </>
          ) : (
            <>
              耳返已开启，<b>务必戴耳机</b> —— 外放会让麦克风拾到自己的输出，形成啸叫回路。
            </>
          )}
        </p>
      </div>
    </div>
  );
}

/** 把毫秒翻译成听感。数字本身对用户没有意义。 */
function retuneWord(ms: number) {
  if (ms === 0) return "· 电音";
  if (ms <= 20) return "· 很紧";
  if (ms <= 60) return "· 自然";
  return "· 几乎不修";
}

function IconMute() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden>
      <path
        d="M7 3L4 6H2v4h2l3 3V3z"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinejoin="round"
      />
      <path d="M10 6l4 4M14 6l-4 4" stroke="currentColor" strokeWidth="1.4" />
    </svg>
  );
}

function IconSound() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden>
      <path
        d="M7 3L4 6H2v4h2l3 3V3z"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinejoin="round"
      />
      <path
        d="M10.5 5.5a3.5 3.5 0 010 5M12.8 3.5a6.5 6.5 0 010 9"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
      />
    </svg>
  );
}
