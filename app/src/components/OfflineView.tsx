/**
 * 离线处理视图。
 *
 * # 它凭什么存在
 *
 * 实时链路被 30 ms 预算捆着手脚：不能回看、f0 只能用因果算法、
 * PSOLA 窗口被 `f0_floor` 截断（低音区拿音质换延迟）。
 *
 * **离线重跑这三条限制一条都没有。** 同一段干声，离线版能做八度纠错、
 * 零相位平滑（修正不再慢半拍）、窗口按真实周期走。
 *
 * 所以这一页不是"另一种修音"，而是**同一段素材更好的那一版**。
 *
 * # 为什么要挡住"引擎在跑"
 *
 * 架构红线 3：推理绝不与实时音频线程抢 CPU。离线处理满载单核，
 * 而实时链路每 3 ms 就要交一次货 —— 同时跑必然爆音。
 *
 * 后端会拒绝，但 UI 必须**提前把按钮禁掉并说清原因**：
 * 让用户点下去再收到报错，是把本可以避免的挫败塞给他。
 */
import { useEffect, useState } from "react";
import { api, type Character, type OfflineStatus, type TakeInfo } from "../ipc";
import { Field, Panel } from "./ui";

export interface OfflineProps {
  /** 引擎是否在跑。跑着就不能启动离线任务。 */
  running: boolean;
  characters: Character[];
  activeId: string;
}

const EMPTY: OfflineStatus = {
  running: false,
  progress: 0,
  stage: "空闲",
  output: null,
  error: null,
  octaveFixes: 0,
  gapFills: 0,
};

export function OfflineView(p: OfflineProps) {
  const [takes, setTakes] = useState<TakeInfo[]>([]);
  const [input, setInput] = useState("");
  const [charId, setCharId] = useState(p.activeId);
  const [st, setSt] = useState<OfflineStatus>(EMPTY);
  const [err, setErr] = useState("");

  useEffect(() => {
    api
      .listRecordings()
      .then((t) => {
        setTakes(t);
        setInput((v) => v || t[0]?.path || "");
      })
      .catch((e) => setErr(String(e)));
    api.offlineStatus().then(setSt).catch(() => {});
  }, []);

  // 只在跑的时候轮询。常驻定时器是白烧 CPU —— 而这个应用的 CPU
  // 本来就要留给音频线程。
  useEffect(() => {
    const sync = () => api.offlineStatus().then(setSt).catch(() => {});
    sync();
    if (!st.running) return;
    const id = window.setInterval(sync, 300);
    return () => window.clearInterval(id);
  }, [st.running]);

  const character = p.characters.find((c) => c.id === charId);
  const take = takes.find((t) => t.path === input);
  const blocked = p.running;
  const canRun = !!input && !!character && !st.running && !blocked;

  const start = async () => {
    if (!character) return;
    setErr("");
    try {
      await api.offlineStart(input, character);
      setSt((s) => ({ ...s, running: true, output: null, error: null }));
    } catch (e) {
      setErr(String(e));
    }
  };

  return (
    <div className="offline">
      <Panel title="离线重新校准">
        <p className="knob-intro">
          实时修音被 30 ms 预算捆着：<b>不能回看</b>、f0 只能用因果算法、
          PSOLA 窗口被基频下限截断。离线重跑这三条限制都没有 ——
          八度纠错、零相位平滑（修正不再慢半拍）、窗口按真实周期走。
          <br />
          输入是<b>干声</b>，输出与它等长，可以直接叠在一起 A/B。
        </p>

        <div className="cast-grid">
          <Field label="选一条录音" hint="录的是原始干声，不是耳返里那个修正过的声音">
            <select value={input} onChange={(e) => setInput(e.target.value)}>
              {takes.length === 0 && <option value="">还没有录音</option>}
              {takes.map((t) => (
                <option key={t.path} value={t.path}>
                  {t.name}（{t.seconds.toFixed(1)}s）
                </option>
              ))}
            </select>
          </Field>

          <Field label="用哪个角色" hint="调、修正速度、移调、共振峰都取自它">
            <select value={charId} onChange={(e) => setCharId(e.target.value)}>
              {p.characters.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.name}
                </option>
              ))}
            </select>
          </Field>
        </div>

        {/* 提前挡住，而不是让用户点下去再收到报错 */}
        {blocked && (
          <p className="hint warn">
            <b>引擎正在运行，离线处理已禁用。</b>
            它会占满一个核，和实时链路抢 CPU 会直接导致爆音。
            请先回「调音」页停止引擎。
          </p>
        )}

        <div className="offline-act">
          <button className="btn primary" onClick={start} disabled={!canRun} type="button">
            开始处理
          </button>
          {st.running && (
            <button className="btn" onClick={() => api.offlineCancel()} type="button">
              取消
            </button>
          )}
          {take && !st.running && (
            <span className="offline-est mono">
              约 {(take.seconds / 26).toFixed(1)} 秒（实测 ~26× 实时）
            </span>
          )}
        </div>

        {st.running && (
          <div className="offline-bar">
            <div className="offline-bar-head">
              <span>{st.stage}</span>
              <span className="mono">{(st.progress * 100).toFixed(0)}%</span>
            </div>
            <div className="offline-track">
              <span style={{ width: `${st.progress * 100}%` }} />
            </div>
          </div>
        )}

        {err && <p className="hint warn">{err}</p>}
        {st.error && <p className="hint warn">处理失败：{st.error}</p>}

        {st.output && !st.running && (
          <div className="offline-done">
            <p className="offline-file mono">{st.output.split(/[\\/]/).pop()}</p>
            {/* 把"离线到底多做了什么"摊开说 —— 否则用户没法判断这一步值不值 */}
            <p className="hint">
              音高轨修了 <b>{st.octaveFixes}</b> 帧八度错误、补了{" "}
              <b>{st.gapFills}</b> 帧空洞 —— 这两样实时链路都做不到
              （看不到邻域）。
              <br />
              文件就在录音旁边。
              <button className="link" onClick={() => api.revealRecordings()} type="button">
                打开文件夹
              </button>
            </p>
          </div>
        )}
      </Panel>

      <Panel title="还没有的">
        <p className="hint">
          <b>神经网络声线转换</b>（「像某个指定的人」那一档）还在做。
          现在这一页只做音高校准 —— 声线塑形仍然是实时链路上的纯 DSP，
          在「角色」页调。
          <br />
          <br />
          这条离线管线产出的音高轨，正是将来接声线转换模型时的必需输入 ——
          所以它不是临时方案，是同一条路的第一段。
        </p>
      </Panel>
    </div>
  );
}
