/**
 * 应用外壳：状态、IPC 装配、两个视图之间的切换。
 *
 * 视图划分见 `TunerView`（主）与 `DiagnosticsView`（次）的文件头。
 */
import { useCallback, useEffect, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  api,
  onTick,
  type BackendInfo,
  type Character,
  type DeviceList,
  type LatencyResult,
  type Tick,
} from "./ipc";
import { TitleBar, type View } from "./components/TitleBar";
import { TunerView } from "./components/TunerView";
import { CastView } from "./components/CastView";
import { DiagnosticsView } from "./components/DiagnosticsView";

const EMPTY_TICK: Tick = {
  running: false,
  metrics: {} as Tick["metrics"],
  driftPpm: 0,
  uncompensated10minMs: 0,
  latencyMs: 0,
  latencyVerdict: "",
  targetFill: 0,
};

export default function App() {
  const [view, setView] = useState<View>("tuner");
  const [devices, setDevices] = useState<DeviceList | null>(null);
  const [info, setInfo] = useState<BackendInfo | null>(null);
  const [tick, setTick] = useState<Tick>(EMPTY_TICK);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [latency, setLatency] = useState<LatencyResult | null>(null);

  const [backend, setBackend] = useState("auto");
  const [inputDevice, setInputDevice] = useState("");
  const [outputDevice, setOutputDevice] = useState("");
  const [f0Floor, setF0Floor] = useState(130);
  const [targetFill, setTargetFill] = useState(2.5);
  const [muted, setMuted] = useState(true);

  // 角色库。「调」「修正速度」「移调」「共振峰」都住在角色里 ——
  // 这里是它们唯一的真相来源，UI 各处都从这读。
  const [characters, setCharacters] = useState<Character[]>([]);
  const [activeId, setActiveId] = useState("");
  const [castError, setCastError] = useState("");

  useEffect(() => {
    api.devices().then(setDevices).catch((e) => setError(String(e)));

    // ⚠️ 必须查一次已有状态。
    //
    // 引擎归 Rust 侧所有，生命周期和这个页面无关 ——
    // `--autostart` 启动的、或页面刷新前就在跑的引擎，都属于这种情况。
    api.engineInfo().then((i) => i && setInfo(i)).catch(() => {});

    api
      .charactersLoad()
      .then((s) => {
        setCharacters(s.characters);
        setActiveId(s.activeId);
      })
      .catch((e) => setCastError(String(e)));

    const un = onTick(setTick);
    return () => {
      un.then((f) => f());
      // 不在卸载时停引擎：页面可能只是热重载，停掉反而更糟
    };
  }, []);

  // 挂载时那一次查询可能太早：`--autostart` 的引擎要先协商独占模式（约 1 秒），
  // 前端挂载比它快，于是拿到 null 之后再也不重试 ——
  // 结果是 tick 说「在跑」但界面显示「未启动」，两边打架。
  useEffect(() => {
    if (tick.running && !info) {
      api.engineInfo().then((i) => i && setInfo(i)).catch(() => {});
    }
  }, [tick.running, info]);

  // 视口尺寸与内容是否溢出。
  //
  // 放在诊断页里有两个用处：用户报 bug 时能直接看到；
  // 我们改布局时能确认有没有撑破窗口 ——
  // **用截图查 WebView 的布局极不可靠**（抓 DirectComposition
  // 经常给出缩放错位的画面），这个读数才是准的。
  const [viewport, setViewport] = useState({ w: 0, h: 0, overflow: 0 });
  useEffect(() => {
    const probe = () => {
      const el = document.querySelector("main");
      setViewport({
        w: window.innerWidth,
        h: window.innerHeight,
        overflow: el ? el.scrollHeight - el.clientHeight : 0,
      });
    };
    const id = window.setTimeout(probe, 200);
    window.addEventListener("resize", probe);
    return () => {
      window.clearTimeout(id);
      window.removeEventListener("resize", probe);
    };
  }, [view, tick.running]);

  // 关键状态写进窗口标题 —— 最小化后仍能从任务栏看到延迟和 xrun
  useEffect(() => {
    const t = tick.running
      ? `wego-voice — ${tick.latencyMs.toFixed(1)}ms${
          tick.metrics?.xruns ? ` · xrun ${tick.metrics.xruns}` : ""
        }`
      : "wego-voice";
    getCurrentWindow().setTitle(t).catch(() => {});
  }, [tick.running, tick.latencyMs, tick.metrics?.xruns]);

  const running = tick.running && !!info;
  const active = characters.find((c) => c.id === activeId);

  // 角色变化 → 立刻下发。
  //
  // 整包下发（`apply_character`）而不是逐个 setParams：换角色是**一个**动作，
  // 不能出现调已经换了、共振峰还没跟上的中间态 —— 那半拍会被听见。
  //
  // 依赖写成 active 本身，所以拖滑杆改参数也会走这条路径，声音当场就变。
  useEffect(() => {
    if (running && active) {
      api.applyCharacter(active).catch((e) => setCastError(String(e)));
    }
  }, [running, active]);

  // 角色库落盘。
  //
  // 防抖 500ms：拖一次滑杆会产生几十次变更，每次都写文件既浪费也容易
  // 和原子替换打架。初始为空（还没加载完）时不写 —— 否则会把用户的库清空。
  useEffect(() => {
    if (!characters.length) return;
    const id = window.setTimeout(() => {
      api
        .charactersSave({ characters, activeId })
        .then(() => setCastError(""))
        .catch((e) => setCastError(String(e)));
    }, 500);
    return () => window.clearTimeout(id);
  }, [characters, activeId]);

  const start = useCallback(async () => {
    setBusy(true);
    setError("");
    setLatency(null);
    try {
      const i = await api.start({
        backend,
        inputDevice: inputDevice || undefined,
        outputDevice: outputDevice || undefined,
        f0Floor,
        targetFillBlocks: targetFill,
      });
      setInfo(i);
      await api.setParams({ monitorMuted: muted });
      if (active) await api.applyCharacter(active);
    } catch (e) {
      setError(String(e));
      setInfo(null);
    } finally {
      setBusy(false);
    }
  }, [backend, inputDevice, outputDevice, f0Floor, targetFill, muted, active]);

  const stop = useCallback(async () => {
    await api.stop();
    setInfo(null);
    setLatency(null);
  }, []);

  // 运行中改的参数要立刻下发，否则滑杆动了声音没变，用户会以为坏了
  const pushMuted = useCallback((v: boolean) => {
    setMuted(v);
    if (running) api.setParams({ monitorMuted: v }).catch(() => {});
  }, [running]);

  // ── 角色库的增删改 ──
  //
  // 全部只改 state：下发和落盘由上面两个 effect 统一负责，
  // 不在每个操作里各写一遍 —— 那样迟早漏掉一处。
  const patchCharacter = useCallback(
    (id: string, upd: Partial<Character>) => {
      setCharacters((cs) => cs.map((c) => (c.id === id ? { ...c, ...upd } : c)));
    },
    [],
  );

  const createCharacter = useCallback(() => {
    const id = `c${Date.now().toString(36)}`;
    setCharacters((cs) => [
      ...cs,
      {
        id,
        name: `新角色 ${cs.filter((c) => !c.builtin).length + 1}`,
        key: "C",
        retuneMs: 40,
        pitchShift: 0,
        formantShift: 0,
        note: "",
        builtin: false,
      },
    ]);
    setActiveId(id);
  }, []);

  const duplicateCharacter = useCallback((id: string) => {
    const nid = `c${Date.now().toString(36)}`;
    setCharacters((cs) => {
      const src = cs.find((c) => c.id === id);
      if (!src) return cs;
      return [...cs, { ...src, id: nid, name: `${src.name} 副本`, builtin: false }];
    });
    setActiveId(nid);
  }, []);

  const deleteCharacter = useCallback((id: string) => {
    setCharacters((cs) => {
      const next = cs.filter((c) => c.id !== id);
      // 删掉的正好是当前角色时要交棒，否则界面会落到"没有选中"的空态
      setActiveId((a) => (a === id ? (next[0]?.id ?? "") : a));
      return next;
    });
  }, []);

  /** 内置角色复位到出厂值。出厂值由 Rust 侧提供，前端不内嵌副本。 */
  const resetBuiltin = useCallback(async (id: string) => {
    try {
      const defaults = await api.charactersBuiltins();
      const d = defaults.find((c) => c.id === id);
      if (d) setCharacters((cs) => cs.map((c) => (c.id === id ? d : c)));
    } catch (e) {
      setCastError(String(e));
    }
  }, []);

  const measure = useCallback(async () => {
    setBusy(true);
    setLatency(null);
    try {
      setLatency(await api.measureLatency(25));
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }, []);

  const m = tick.metrics ?? ({} as Tick["metrics"]);

  return (
    <div className="app">
      <TitleBar
        view={view}
        onView={setView}
        running={running}
        latencyMs={running ? tick.latencyMs : undefined}
        xruns={m.xruns}
      />

      <main>
        {error && (
          <div className="notice alert">
            {error}
            <button className="notice-x" onClick={() => setError("")} title="关闭">
              ✕
            </button>
          </div>
        )}

        {info?.fallbackReason && (
          <div className="notice alert">
            <b>已降级到共享模式，延迟远超可用范围。</b>
            <br />
            独占模式启动失败：<span className="mono">{info.fallbackReason}</span>
            <br />
            最常见原因是<b>声卡正被其他程序占用</b>
            （播放器、通话软件、浏览器标签页）。关掉它们后重新启动即可。
          </div>
        )}

        {view === "tuner" && (
          <TunerView
            running={running}
            busy={busy}
            devices={devices}
            sample={{
              voiced: !!m.voiced,
              centsOff: m.centsOff ?? 0,
              targetMidi: m.targetMidi ?? 69,
              f0Hz: m.f0Hz ?? 0,
              rms: m.rms ?? 0,
              clipping: !!m.clipping,
            }}
            backend={backend}
            onBackend={setBackend}
            inputDevice={inputDevice}
            onInputDevice={setInputDevice}
            outputDevice={outputDevice}
            onOutputDevice={setOutputDevice}
            muted={muted}
            onMuted={pushMuted}
            characters={characters}
            activeId={activeId}
            onActiveId={setActiveId}
            onRetuneMs={(v) => activeId && patchCharacter(activeId, { retuneMs: v })}
            onManage={() => setView("cast")}
            onStart={start}
            onStop={stop}
          />
        )}

        {view === "cast" && (
          <CastView
            characters={characters}
            activeId={activeId}
            onActiveId={setActiveId}
            onPatch={patchCharacter}
            onCreate={createCharacter}
            onDuplicate={duplicateCharacter}
            onDelete={deleteCharacter}
            onResetBuiltin={resetBuiltin}
            running={running}
            saveError={castError}
          />
        )}

        {view === "diag" && (
          <DiagnosticsView
            running={running}
            busy={busy}
            tick={tick}
            info={info}
            latency={latency}
            onMeasure={measure}
            onReset={() => api.resetMetrics()}
            f0Floor={f0Floor}
            onF0Floor={setF0Floor}
            targetFill={targetFill}
            onTargetFill={setTargetFill}
            viewport={viewport}
          />
        )}
      </main>
    </div>
  );
}
