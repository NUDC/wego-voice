/**
 * 录音页 —— 录下来的素材都在这里，兼管离线重新校准。
 *
 * # 它凭什么存在
 *
 * 录音是这个应用唯一会留下的东西。以前它们只是磁盘上一串时间戳文件名，
 * 界面上能做的只有"打开文件夹"，剩下的全靠用户自己在资源管理器里翻。
 *
 * 一条录音需要能被：**听**（还有没有用）、**改名**（一周后还认得出是哪条）、
 * **删**（唱废的占地方）、**处理**（离线重新校准）。这四件事都在这一页。
 *
 * # 离线重新校准
 *
 * 实时链路被 30 ms 预算捆着手脚：不能回看、f0 只能用因果算法、
 * PSOLA 窗口被 `f0_floor` 截断（低音区拿音质换延迟）。
 *
 * **离线重跑这三条限制一条都没有。** 同一段干声，离线版能做八度纠错、
 * 零相位平滑（修正不再慢半拍）、窗口按真实周期走。
 *
 * 所以产物不是"另一种修音"，而是**同一段素材更好的那一版** ——
 * 它挂在干声下面，可以直接 A/B。
 *
 * # 为什么要挡住"引擎在跑"
 *
 * 架构红线 3：推理绝不与实时音频线程抢 CPU。离线处理满载单核，
 * 而实时链路每 3 ms 就要交一次货 —— 同时跑必然爆音。
 *
 * 后端会拒绝，但 UI 必须**提前把按钮禁掉并说清原因**：
 * 让用户点下去再收到报错，是把本可以避免的挫败塞给他。
 *
 * # 播放为什么可能失手
 *
 * 用的是 WebView 的 `<audio>` + `convertFileSrc`（asset 协议，
 * 作用域锁在 `$AUDIO/wego-voice/*`，见 tauri.conf.json）。两种失手：
 *
 * 1. 引擎**独占**着声卡 —— 这时候系统里其他声音全都出不来
 * 2. WebView 解不了 32-bit float WAV（我们自己写的就是这个格式）
 *
 * 所以播放出错时不吞掉，而是露出「用系统播放器打开」。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { api, type Character, type OfflineStatus, type TakeInfo } from "../ipc";
import { Field, Panel } from "./ui";

export interface TakesProps {
  /** 引擎是否在跑。跑着就不能启动离线任务。 */
  running: boolean;
  characters: Character[];
  activeId: string;
  /** 回调：跳到调音页去录。空列表时是唯一有意义的下一步。 */
  onGoTune: () => void;
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

function mmss(secs: number) {
  const s = Math.max(0, Math.round(secs));
  return `${String(Math.floor(s / 60)).padStart(2, "0")}:${String(s % 60).padStart(2, "0")}`;
}

function mb(bytes: number) {
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${Math.max(1, Math.round(bytes / 1024))} kB`;
}

/** 相对时间。"3 分钟前"比"2026-09-23 14:02"更容易对上刚才那一条。 */
function ago(unixSecs: number) {
  if (!unixSecs) return "";
  const d = Date.now() / 1000 - unixSecs;
  if (d < 90) return "刚刚";
  if (d < 3600) return `${Math.round(d / 60)} 分钟前`;
  if (d < 86400) return `${Math.round(d / 3600)} 小时前`;
  if (d < 86400 * 7) return `${Math.round(d / 86400)} 天前`;
  return new Date(unixSecs * 1000).toLocaleDateString("zh-CN");
}

export function TakesView(p: TakesProps) {
  const [takes, setTakes] = useState<TakeInfo[]>([]);
  const [root, setRoot] = useState("");
  const [sel, setSel] = useState("");
  const [charId, setCharId] = useState(p.activeId);
  const [st, setSt] = useState<OfflineStatus>(EMPTY);
  const [err, setErr] = useState("");
  const [loaded, setLoaded] = useState(false);

  /** 正在改名的那条（路径）与输入框内容。 */
  const [editing, setEditing] = useState("");
  const [draft, setDraft] = useState("");
  /** 已经点过一次删除的那条。两步确认 —— 删录音没有后悔药。 */
  const [confirming, setConfirming] = useState("");
  /**
   * 这次离线任务是给哪条录音跑的。
   *
   * 任务状态是**全局**的（同时只允许一个），但进度条和结果必须挂在
   * 对应的那条上 —— 否则处理完 A 再展开 B，B 底下会显示 A 的战果。
   */
  const [jobPath, setJobPath] = useState("");

  const refresh = useCallback(async () => {
    try {
      setTakes(await api.listRecordings());
      setErr("");
    } catch (e) {
      setErr(String(e));
    } finally {
      setLoaded(true);
    }
  }, []);

  useEffect(() => {
    refresh();
    api.recordingsRoot().then(setRoot).catch(() => {});
    api.offlineStatus().then(setSt).catch(() => {});
  }, [refresh]);

  // 角色库是异步加载的，挂载那一刻 activeId 往往还是空串 ——
  // 不补这一下，下拉框会停在"没选中"，用户得自己再选一次。
  useEffect(() => {
    setCharId((c) => c || p.activeId);
  }, [p.activeId]);

  // 只在跑的时候轮询。常驻定时器是白烧 CPU —— 而这个应用的 CPU
  // 本来就要留给音频线程。
  useEffect(() => {
    const sync = () => api.offlineStatus().then(setSt).catch(() => {});
    sync();
    if (!st.running) return;
    const id = window.setInterval(sync, 300);
    return () => window.clearInterval(id);
  }, [st.running]);

  // 任务一结束就重扫目录 —— 产物得立刻出现在它那条干声下面，
  // 否则用户会以为没成功，然后再点一次。
  const wasRunning = useRef(false);
  useEffect(() => {
    if (wasRunning.current && !st.running) refresh();
    wasRunning.current = st.running;
  }, [st.running, refresh]);

  const character = p.characters.find((c) => c.id === charId);
  const selected = takes.find((t) => t.path === sel);
  const blocked = p.running;

  const totalSecs = useMemo(
    () => takes.reduce((a, t) => a + t.seconds, 0),
    [takes],
  );

  const start = async (path: string) => {
    if (!character) return;
    setErr("");
    try {
      await api.offlineStart(path, character);
      setJobPath(path);
      setSt((s) => ({ ...s, running: true, output: null, error: null }));
    } catch (e) {
      setErr(String(e));
    }
  };

  /** 两步确认会一直亮着，几秒后自动缩回去 —— 一个长期停在「确认删除」
      状态的按钮，下一次误点就是真删。 */
  const askDelete = (path: string) => {
    setConfirming(path);
    window.setTimeout(() => setConfirming((c) => (c === path ? "" : c)), 4000);
  };

  const commitRename = async (t: TakeInfo) => {
    const name = draft.trim();
    setEditing("");
    if (!name || name === t.name.replace(/\.wav$/i, "")) return;
    try {
      const np = await api.renameRecording(t.path, name);
      setSel((s) => (s === t.path ? np : s));
      await refresh();
    } catch (e) {
      setErr(String(e));
    }
  };

  const remove = async (t: TakeInfo) => {
    setConfirming("");
    try {
      await api.deleteRecording(t.path, true);
      setSel((s) => (s === t.path ? "" : s));
      await refresh();
    } catch (e) {
      setErr(String(e));
    }
  };

  return (
    <div className="takes">
      <Panel
        title="录音"
        right={
          <span className="takes-sum mono">
            {takes.length} 条 · {mmss(totalSecs)}
          </span>
        }
      >
        <p className="knob-intro">
          落盘的是<b>干声</b> —— 采集侧的原始输入，不是耳返里那个修正过的声音。
          修正音有损且不可逆，只存它等于永久放弃了换角色重来、
          离线重新校准、以及将来送进声线转换的机会。
          {root && (
            <>
              <br />
              文件在 <span className="mono dim">{root}</span>
              {" · "}
              <button className="link" onClick={() => api.revealRecordings()} type="button">
                打开文件夹
              </button>
              {" · "}
              <button className="link" onClick={refresh} type="button">
                重新扫描
              </button>
            </>
          )}
        </p>

        {err && <p className="hint warn">{err}</p>}

        {loaded && takes.length === 0 && (
          <div className="takes-empty">
            <p>
              还没有录音。去<b>调音</b>页启动引擎，然后按「录制」——
              录下来的东西会出现在这里。
            </p>
            <button className="btn" onClick={p.onGoTune} type="button">
              去调音页
            </button>
          </div>
        )}

        <ul className="take-list">
          {takes.map((t) => {
            const open = sel === t.path;
            const mine = jobPath === t.path;
            const busy = st.running && mine;
            return (
              <li key={t.path} className={`take ${open ? "open" : ""}`}>
                <div className="take-head">
                  <button
                    className="take-name"
                    onClick={() => setSel(open ? "" : t.path)}
                    type="button"
                    title={t.path}
                  >
                    <span className={`take-caret ${open ? "on" : ""}`}>▸</span>
                    {editing === t.path ? (
                      <input
                        className="take-edit"
                        value={draft}
                        autoFocus
                        onChange={(e) => setDraft(e.target.value)}
                        onClick={(e) => e.stopPropagation()}
                        onKeyDown={(e) => {
                          if (e.key === "Enter") commitRename(t);
                          if (e.key === "Escape") setEditing("");
                        }}
                        onBlur={() => commitRename(t)}
                      />
                    ) : (
                      <b>{t.name.replace(/\.wav$/i, "")}</b>
                    )}
                  </button>

                  <span className="take-meta mono">
                    {mmss(t.seconds)} · {mb(t.bytes)} · {ago(t.modified)}
                  </span>

                  {t.correctedPath && <span className="take-tag">已校准</span>}

                  <div className="take-acts">
                    <button
                      className="btn tiny"
                      onClick={() => {
                        setEditing(t.path);
                        setDraft(t.name.replace(/\.wav$/i, ""));
                      }}
                      type="button"
                    >
                      改名
                    </button>
                    <button
                      className="btn tiny"
                      onClick={() => api.revealFile(t.path).catch((e) => setErr(String(e)))}
                      type="button"
                      title="在资源管理器里选中它"
                    >
                      定位
                    </button>
                    {/* 两步确认。删录音没有后悔药 —— 虽然后端走的是回收站，
                        但让用户翻回收站找回来仍然是一次失败体验。 */}
                    {confirming === t.path ? (
                      <button className="btn tiny danger" onClick={() => remove(t)} type="button">
                        确认删除
                      </button>
                    ) : (
                      <button
                        className="btn tiny"
                        onClick={() => askDelete(t.path)}
                        type="button"
                        title="移进回收站"
                      >
                        删除
                      </button>
                    )}
                  </div>
                </div>

                {open && (
                  <div className="take-body">
                    <Clip label="干声" path={t.path} />
                    {t.correctedPath && (
                      <Clip
                        label="校准版"
                        path={t.correctedPath}
                        note={`${mmss(t.correctedSeconds)} · ${mb(t.correctedBytes)}`}
                      />
                    )}

                    {/* 提前挡住，而不是让用户点下去再收到报错 */}
                    {blocked ? (
                      <p className="hint warn">
                        <b>引擎正在运行，离线处理已禁用。</b>
                        它会占满一个核，和实时链路抢 CPU 会直接导致爆音。
                        请先回「调音」页停止引擎。
                      </p>
                    ) : (
                      <div className="take-run">
                        <Field label="用哪个角色" hint="调、修正速度、移调、共振峰都取自它">
                          <select
                            value={charId}
                            onChange={(e) => setCharId(e.target.value)}
                            disabled={st.running}
                          >
                            {p.characters.map((c) => (
                              <option key={c.id} value={c.id}>
                                {c.name}
                              </option>
                            ))}
                          </select>
                        </Field>

                        <button
                          className="btn primary"
                          onClick={() => start(t.path)}
                          disabled={!character || st.running}
                          type="button"
                        >
                          {t.correctedPath ? "重新校准" : "离线校准"}
                        </button>
                        {st.running && (
                          <button className="btn" onClick={() => api.offlineCancel()} type="button">
                            取消
                          </button>
                        )}
                        {!st.running && (
                          <span className="take-est mono">
                            约 {(t.seconds / 26).toFixed(1)} 秒（实测 ~26× 实时）
                          </span>
                        )}
                      </div>
                    )}

                    {busy && (
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

                    {mine && st.error && !st.running && (
                      <p className="hint warn">处理失败：{st.error}</p>
                    )}

                    {/* 把"离线到底多做了什么"摊开说 ——
                        否则用户没法判断这一步值不值。 */}
                    {mine && st.output && !st.running && (
                      <p className="hint">
                        音高轨修了 <b>{st.octaveFixes}</b> 帧八度错误、补了{" "}
                        <b>{st.gapFills}</b> 帧空洞 —— 这两样实时链路都做不到
                        （看不到邻域）。产物与干声<b>等长</b>，可以直接对着听。
                      </p>
                    )}
                  </div>
                )}
              </li>
            );
          })}
        </ul>
      </Panel>

      {selected && (
        <p className="hint dim takes-foot">
          离线校准只动<b>音高</b>。声线塑形仍然是实时链路上的纯 DSP，在「角色」页调；
          基于神经网络的声线转换（「像某个指定的人」那一档）还在做 ——
          这条离线管线产出的音高轨，正是将来接模型时的必需输入。
        </p>
      )}
    </div>
  );
}

/**
 * 一条可播放的音频。
 *
 * 播放失败不吞掉：WebView 解不了 32-bit float WAV、或者引擎正独占声卡，
 * 都会走到这里。这时候给出系统播放器这条路，而不是让按钮点了没反应。
 */
function Clip({ label, path, note }: { label: string; path: string; note?: string }) {
  const [failed, setFailed] = useState(false);
  const src = useMemo(() => {
    try {
      return convertFileSrc(path);
    } catch {
      return "";
    }
  }, [path]);

  return (
    <div className="clip">
      <span className="clip-label">{label}</span>
      {!failed && src ? (
        <audio className="clip-audio" src={src} controls preload="none" onError={() => setFailed(true)} />
      ) : (
        <span className="clip-fail">
          这里播不了（引擎独占声卡时系统里所有声音都出不来，
          或者内核解不了 32 位浮点 WAV）。
        </span>
      )}
      <button className="link" onClick={() => api.openFile(path)} type="button">
        用系统播放器打开
      </button>
      {note && <span className="clip-note mono">{note}</span>}
    </div>
  );
}
