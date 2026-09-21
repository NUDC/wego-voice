/**
 * 音高显示 —— 这一页的主角。
 *
 * # 为什么它值得占这么大地方
 *
 * 产品的核心体验是「唱的当下就看见/听见自己唱准了」。
 * 这个显示就是那个「看见」。把它塞进一个小方框里，
 * 等于把产品最重要的部分藏起来。
 *
 * # 分工：DOM 画字，canvas 画动画
 *
 * - **音名**（A4）变化很慢 —— 只在跨半音时变，交给 React/DOM，
 *   字体渲染更清晰，也能用上可变字重。
 * - **指针和历史曲线**每帧都变 —— 交给 canvas + rAF，
 *   绝不走 React state（每秒 60 次协调会把主线程打满）。
 */
import { useEffect, useRef } from "react";

const NOTE_NAMES = [
  "C",
  "C♯",
  "D",
  "D♯",
  "E",
  "F",
  "F♯",
  "G",
  "G♯",
  "A",
  "A♯",
  "B",
];

export interface PitchSample {
  voiced: boolean;
  centsOff: number;
  targetMidi: number;
  f0Hz: number;
  rms: number;
  clipping: boolean;
}

/** 历史长度。20Hz 推送、50ms 一格，约 8 秒。 */
const HISTORY = 160;
/** 纵轴范围（音分）。±50 正好覆盖半音。 */
const RANGE = 50;
/** 「准」的判据。±10 音分是人耳基本听不出差别的范围。 */
const IN_TUNE = 10;

export function noteLabel(midi: number): { name: string; octave: number } {
  const pc = ((midi % 12) + 12) % 12;
  return { name: NOTE_NAMES[pc] ?? "—", octave: Math.floor(midi / 12) - 1 };
}

export function PitchDisplay({
  sample,
  running,
}: {
  sample: PitchSample;
  running: boolean;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const latest = useRef<PitchSample>(sample);
  const hist = useRef<(number | null)[]>(new Array(HISTORY).fill(null));
  latest.current = sample;

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    let raf = 0;
    let lastPush = 0;
    // 指针做平滑，否则检测抖动会让它神经质地跳
    let needle = 0;

    const draw = (now: number) => {
      raf = requestAnimationFrame(draw);

      const dpr = window.devicePixelRatio || 1;
      const w = canvas.clientWidth;
      const h = canvas.clientHeight;
      if (canvas.width !== w * dpr || canvas.height !== h * dpr) {
        canvas.width = w * dpr;
        canvas.height = h * dpr;
      }
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, w, h);

      const s = latest.current;

      // 历史以固定节奏推进，与刷新率解耦 ——
      // 否则 60Hz 和 144Hz 显示器上曲线滚动速度会不一样
      if (now - lastPush > 50) {
        lastPush = now;
        hist.current.push(s.voiced ? s.centsOff : null);
        if (hist.current.length > HISTORY) hist.current.shift();
      }

      const target = s.voiced ? s.centsOff : 0;
      needle += (target - needle) * 0.25;

      // ── 布局：上半是指针刻度，下半是历史曲线 ──
      const scaleH = Math.round(h * 0.42);
      const gap = 14;
      const histY = scaleH + gap;
      const histH = h - histY;

      drawScale(ctx, w, scaleH, needle, s.voiced && running);
      drawHistory(ctx, w, histY, histH, hist.current);
    };

    raf = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(raf);
  }, [running]);

  const { name, octave } = noteLabel(sample.targetMidi);
  const active = running && sample.voiced;
  const inTune = active && Math.abs(sample.centsOff) <= IN_TUNE;

  return (
    <div className={`pitch ${running ? "running" : ""} ${active ? "active" : ""}`}>
      <div className="pitch-note">
        <div className={`note-name num ${inTune ? "in-tune" : ""}`}>
          {active ? name : "—"}
          {active && <sub>{octave}</sub>}
        </div>
        <div className="note-meta mono">
          {active ? `${sample.f0Hz.toFixed(1)} Hz` : running ? "静音 / 清音" : "未启动"}
        </div>
        <div className={`note-cents mono ${inTune ? "in-tune" : ""}`}>
          {active
            ? `${sample.centsOff >= 0 ? "+" : ""}${sample.centsOff.toFixed(0)}`
            : "·"}
          <span className="cent-unit">¢</span>
        </div>
      </div>

      <div className="pitch-canvas">
        <canvas ref={canvasRef} />
      </div>
    </div>
  );
}

/** 上半：指针刻度。像调音器那样，一眼看出偏高还是偏低。 */
function drawScale(
  ctx: CanvasRenderingContext2D,
  w: number,
  h: number,
  cents: number,
  active: boolean,
) {
  const mid = w / 2;
  const half = w / 2 - 16;
  const x = mid + (Math.max(-RANGE, Math.min(RANGE, cents)) / RANGE) * half;
  const baseY = h - 10;

  // 刻度线：每 10 音分一根，整十更长
  for (let c = -RANGE; c <= RANGE; c += 10) {
    const tx = mid + (c / RANGE) * half;
    const major = c % 25 === 0;
    ctx.strokeStyle = major ? "#2c3240" : "#1e222c";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(Math.round(tx) + 0.5, baseY - (major ? 14 : 8));
    ctx.lineTo(Math.round(tx) + 0.5, baseY);
    ctx.stroke();
  }

  // 「准」区：中间一条浅色带，给用户一个明确的目标
  const tw = (IN_TUNE / RANGE) * half;
  ctx.fillStyle = active ? "rgba(69, 212, 131, 0.10)" : "rgba(98, 106, 126, 0.05)";
  ctx.fillRect(mid - tw, baseY - 26, tw * 2, 26);

  // 中线
  ctx.strokeStyle = active ? "#45d483" : "#333a49";
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(Math.round(mid) + 0.5, baseY - 30);
  ctx.lineTo(Math.round(mid) + 0.5, baseY);
  ctx.stroke();

  if (!active) return;

  // 指针
  const good = Math.abs(cents) <= IN_TUNE;
  const color = good ? "#45d483" : "#f2b53c";
  ctx.fillStyle = color;
  ctx.beginPath();
  ctx.moveTo(x, baseY - 34);
  ctx.lineTo(x - 5, baseY - 44);
  ctx.lineTo(x + 5, baseY - 44);
  ctx.closePath();
  ctx.fill();

  ctx.strokeStyle = color;
  ctx.lineWidth = 2;
  ctx.beginPath();
  ctx.moveTo(x, baseY - 34);
  ctx.lineTo(x, baseY - 4);
  ctx.stroke();

  // 命中时给一圈辉光，是那种「对了」的即时反馈
  if (good) {
    ctx.shadowColor = color;
    ctx.shadowBlur = 12;
    ctx.beginPath();
    ctx.arc(x, baseY - 4, 3, 0, Math.PI * 2);
    ctx.fillStyle = color;
    ctx.fill();
    ctx.shadowBlur = 0;
  }
}

/** 下半：最近几秒的音准走势。看趋势，不看瞬时。 */
function drawHistory(
  ctx: CanvasRenderingContext2D,
  w: number,
  y0: number,
  h: number,
  hist: (number | null)[],
) {
  if (h <= 0) return;
  const mid = y0 + h / 2;
  const toY = (c: number) =>
    mid - (Math.max(-RANGE, Math.min(RANGE, c)) / RANGE) * (h / 2 - 4);

  // 准区带
  ctx.fillStyle = "rgba(69, 212, 131, 0.06)";
  ctx.fillRect(0, toY(IN_TUNE), w, toY(-IN_TUNE) - toY(IN_TUNE));

  ctx.strokeStyle = "#232734";
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(0, Math.round(mid) + 0.5);
  ctx.lineTo(w, Math.round(mid) + 0.5);
  ctx.stroke();

  // 曲线：渐变让「现在」比「刚才」更亮，暗示时间方向
  const grad = ctx.createLinearGradient(0, 0, w, 0);
  grad.addColorStop(0, "rgba(76, 194, 240, 0.15)");
  grad.addColorStop(1, "rgba(76, 194, 240, 0.95)");
  ctx.strokeStyle = grad;
  ctx.lineWidth = 1.75;
  ctx.lineJoin = "round";
  ctx.beginPath();
  let pen = false;
  hist.forEach((c, i) => {
    const x = (i / (HISTORY - 1)) * w;
    if (c === null) {
      pen = false;
      return;
    }
    if (!pen) {
      ctx.moveTo(x, toY(c));
      pen = true;
    } else {
      ctx.lineTo(x, toY(c));
    }
  });
  ctx.stroke();
}
