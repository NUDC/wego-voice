/**
 * 音高显示的静态复刻，内联 SVG。
 *
 * # 为什么非要有它
 *
 * 这个产品卖的是「唱的当下就**看见/听见**自己唱准了」，而改版之前
 * 官网从头到尾一张图都没有 —— 用户得读三屏文字才能想象出它长什么样。
 * 对一个视觉即核心体验的工具来说，这是最大的一处缺失。
 *
 * # 为什么是 SVG 而不是截图
 *
 * - 截图要维护：改一次界面就得重截，而且迟早忘
 * - 截图在暗色/亮色、各种 DPI 下都要出好几份
 * - **官网是零 JavaScript 的静态页**（见 entry-server.tsx），
 *   内联 SVG 直接进 HTML，不增加一次请求、不增加一个字节的 JS
 *
 * 它是**示意**不是实拍，所以画的数字取的是真实场景里的典型值：
 * A4、偏高 3 音分（在"准"的区间内）。不编造夸张的效果。
 */

/** 历史曲线：一段像真的在唱的音分轨迹（±50 音分映射到 y）。 */
const TRACE = [
  -34, -31, -26, -18, -11, -6, -3, -1, 2, 5, 7, 6, 3, 1, -1, -2, -1, 1, 3, 4,
  3, 1, -1, -3, -2, 0, 2, 3, 2, 1, 0, -1, 1, 2, 3, 2, 1, 2, 3, 3,
];

const W = 420;
const H = 260;
/** 曲线区域。 */
const PLOT = { x: 16, y: 104, w: W - 32, h: H - 120 };
/** 纵轴半量程（音分）。±50 正好一个半音。 */
const RANGE = 50;

function y(cents: number) {
  return PLOT.y + PLOT.h / 2 - (cents / RANGE) * (PLOT.h / 2);
}

export function Tuner() {
  const step = PLOT.w / (TRACE.length - 1);
  const path = TRACE.map(
    (c, i) => `${i === 0 ? "M" : "L"}${(PLOT.x + i * step).toFixed(1)} ${y(c).toFixed(1)}`,
  ).join(" ");

  return (
    <svg
      className="tuner"
      viewBox={`0 0 ${W} ${H}`}
      role="img"
      aria-label="音高显示示意：当前音 A4，偏高 3 音分，位于「准」的区间内；下方是最近 8 秒的音准走势"
    >
      <defs>
        {/* 准区带：中间亮、两边淡，视线自然被拉到中线 */}
        <linearGradient id="band" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor="#4ade80" stopOpacity="0" />
          <stop offset="50%" stopColor="#4ade80" stopOpacity="0.16" />
          <stop offset="100%" stopColor="#4ade80" stopOpacity="0" />
        </linearGradient>
        {/* 曲线尾部渐隐 —— 越旧越淡，暗示它在往左滚 */}
        <linearGradient id="trace" x1="0" y1="0" x2="1" y2="0">
          <stop offset="0%" stopColor="#56ccf2" stopOpacity="0.15" />
          <stop offset="55%" stopColor="#56ccf2" stopOpacity="0.75" />
          <stop offset="100%" stopColor="#4ade80" stopOpacity="1" />
        </linearGradient>
      </defs>

      <rect x="0.5" y="0.5" width={W - 1} height={H - 1} rx="10" className="t-panel" />

      {/* ── 读数行 ── */}
      <text x="22" y="62" className="t-note">
        A
        <tspan className="t-oct" dy="4">
          4
        </tspan>
      </text>
      <text x="22" y="84" className="t-meta">
        442.6 Hz
      </text>
      <text x={W - 22} y="62" className="t-cents" textAnchor="end">
        +3
        <tspan className="t-unit">¢</tspan>
      </text>
      <text x={W - 22} y="84" className="t-meta" textAnchor="end">
        准
      </text>

      {/* ── 曲线区 ── */}
      <rect
        x={PLOT.x}
        y={y(12)}
        width={PLOT.w}
        height={y(-12) - y(12)}
        fill="url(#band)"
      />
      {/* 刻度：±50 / ±25 / 0 */}
      {[50, 25, 0, -25, -50].map((c) => (
        <line
          key={c}
          x1={PLOT.x}
          x2={PLOT.x + PLOT.w}
          y1={y(c)}
          y2={y(c)}
          className={c === 0 ? "t-axis" : "t-grid"}
        />
      ))}

      <path d={path} className="t-trace" />
      {/* 当前点：落在准区里，所以是绿的 */}
      <circle
        cx={PLOT.x + PLOT.w}
        cy={y(TRACE[TRACE.length - 1] ?? 0)}
        r="4.5"
        className="t-head"
      />
    </svg>
  );
}
