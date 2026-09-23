/**
 * 自定义标题栏。
 *
 * 窗口设了 `decorations: false`，系统边框没了，这里要自己补：
 * 拖拽区（`data-tauri-drag-region`，没有它窗口拖不动）、窗口控制、
 * 以及双击最大化这个用户会下意识去做的动作。
 *
 * 视图切换也放这里 —— 标题栏是唯一一直可见的地方，
 * 而「在乐器和诊断之间切换」是本应用最高频的导航。
 */
import { useEffect, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { Segmented } from "./ui";

const win = getCurrentWindow();

export type View = "tuner" | "cast" | "offline" | "diag";

export function TitleBar({
  view,
  onView,
  running,
  latencyMs,
  xruns,
}: {
  view: View;
  onView: (v: View) => void;
  running: boolean;
  latencyMs?: number | undefined;
  xruns?: number | undefined;
}) {
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    let alive = true;
    const sync = () =>
      win
        .isMaximized()
        .then((v) => alive && setMaximized(v))
        .catch(() => {});
    sync();
    // 最大化状态可能被系统改变（Win+↑、拖到屏幕顶端、双击边框），
    // 不监听的话按钮图标会和实际状态对不上
    const un = win.onResized(sync);
    return () => {
      alive = false;
      un.then((f) => f()).catch(() => {});
    };
  }, []);

  return (
    <div className="titlebar" data-tauri-drag-region>
      <div className="tb-brand" data-tauri-drag-region>
        <Logo />
        <span data-tauri-drag-region>wego-voice</span>
      </div>

      <div className="tb-nav">
        <Segmented
          value={view}
          onChange={onView}
          options={[
            { value: "tuner", label: "调音" },
            { value: "cast", label: "角色" },
            { value: "offline", label: "处理" },
            { value: "diag", label: "诊断" },
          ]}
        />
      </div>

      <div className="tb-status" data-tauri-drag-region>
        {running && (
          <>
            <span className="tb-live">
              <i />
              运行中
            </span>
            {latencyMs !== undefined && (
              <span className="tb-lat mono">{latencyMs.toFixed(1)} ms</span>
            )}
            {!!xruns && <span className="tb-xrun mono">xrun {xruns}</span>}
          </>
        )}
      </div>

      <div className="tb-controls">
        <button className="tb-btn" onClick={() => win.minimize()} title="最小化">
          <svg width="10" height="10" viewBox="0 0 10 10">
            <rect x="0" y="4.5" width="10" height="1" fill="currentColor" />
          </svg>
        </button>
        <button
          className="tb-btn"
          onClick={() => win.toggleMaximize()}
          title={maximized ? "还原" : "最大化"}
        >
          {maximized ? (
            <svg width="10" height="10" viewBox="0 0 10 10">
              <rect x="0.5" y="2.5" width="7" height="7" fill="none" stroke="currentColor" />
              <path d="M2.5 2.5V0.5h7v7h-2" fill="none" stroke="currentColor" />
            </svg>
          ) : (
            <svg width="10" height="10" viewBox="0 0 10 10">
              <rect x="0.5" y="0.5" width="9" height="9" fill="none" stroke="currentColor" />
            </svg>
          )}
        </button>
        {/* tooltip 要提前说清 —— 关闭不退出是反直觉的，
            不能等用户点完发现窗口没了、程序还在才知道。 */}
        <button
          className="tb-btn close"
          onClick={() => win.close()}
          title="收进托盘（退出请用托盘菜单）"
        >
          <svg width="10" height="10" viewBox="0 0 10 10">
            <path d="M0 0l10 10M10 0L0 10" stroke="currentColor" fill="none" />
          </svg>
        </button>
      </div>
    </div>
  );
}

function Logo() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden>
      <path
        d="M1 8h2l1.6-5 2.2 10L9.2 6l1.4 4L12 8h3"
        fill="none"
        stroke="var(--accent)"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}
