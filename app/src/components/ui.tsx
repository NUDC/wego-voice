/** 界面基元。只放真正重复出现的东西。 */
import type { ReactNode } from "react";

export type Tone = "neutral" | "good" | "caution" | "alert";

export function Panel({
  title,
  right,
  children,
  className = "",
}: {
  title?: string;
  right?: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <section className={`panel ${className}`}>
      {(title || right) && (
        <header className="panel-head">
          {title && <h2>{title}</h2>}
          {right && <div className="panel-head-right">{right}</div>}
        </header>
      )}
      {children}
    </section>
  );
}

/** 键值行。诊断信息的基本单位。 */
export function Row({
  label,
  value,
  tone = "neutral",
  muted,
}: {
  label: string;
  value: ReactNode;
  tone?: Tone;
  muted?: boolean;
}) {
  return (
    <div className="row">
      <span className="row-k">{label}</span>
      <span className={`row-v mono tone-${tone} ${muted ? "muted" : ""}`}>
        {value}
      </span>
    </div>
  );
}

/** 一个大数字 + 标签。用在需要「一眼看到」的地方。 */
export function Stat({
  label,
  value,
  unit,
  tone = "neutral",
  hint,
}: {
  label: string;
  value: ReactNode;
  unit?: string;
  tone?: Tone;
  hint?: ReactNode;
}) {
  return (
    <div className={`stat tone-${tone}`}>
      <div className="stat-label">{label}</div>
      <div className="stat-value num">
        {value}
        {unit && <span className="stat-unit">{unit}</span>}
      </div>
      {hint && <div className="stat-hint">{hint}</div>}
    </div>
  );
}

/** 状态点。比一整行文字省地方，而且能一眼扫到。 */
export function Dot({ tone = "neutral" }: { tone?: Tone }) {
  return <span className={`dot tone-${tone}`} />;
}

export function Field({
  label,
  children,
  hint,
  className = "",
}: {
  label: string;
  children: ReactNode;
  hint?: string;
  className?: string;
}) {
  return (
    <label className={`field ${className}`}>
      <span className="field-label">{label}</span>
      {children}
      {hint && <span className="field-hint">{hint}</span>}
    </label>
  );
}

/** 分段控件。用于视图切换、二选一的参数。 */
export function Segmented<T extends string | number>({
  value,
  options,
  onChange,
  disabled,
}: {
  value: T;
  options: { value: T; label: string; title?: string }[];
  onChange: (v: T) => void;
  disabled?: boolean;
}) {
  return (
    <div className={`segmented ${disabled ? "disabled" : ""}`}>
      {options.map((o) => (
        <button
          key={String(o.value)}
          className={o.value === value ? "on" : ""}
          onClick={() => !disabled && onChange(o.value)}
          disabled={disabled}
          title={o.title ?? ""}
          type="button"
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

/** 水平电平条。 */
export function Meter({
  value,
  tone = "neutral",
  marker,
}: {
  /** 0~1 */
  value: number;
  tone?: Tone;
  /** 0~1，画一条目标线 */
  marker?: number;
}) {
  return (
    <div className="meter">
      <div
        className={`meter-fill tone-${tone}`}
        style={{ width: `${Math.max(0, Math.min(1, value)) * 100}%` }}
      />
      {marker !== undefined && (
        <div className="meter-marker" style={{ left: `${marker * 100}%` }} />
      )}
    </div>
  );
}

/**
 * 一根滑杆 + 数值 + 听感说明。
 *
 * 参数本身对用户没有意义，说明才有 —— "+2.0 半音"远不如"声道更短、更细"。
 *
 * 数值**紧跟标签**而不是甩到行尾：面板可以有一千像素宽，两端对齐会让
 * 「标签 → 把手 → 数值」横跨整屏才对得上。轨道同理要限宽，
 * 更长的轨道不是更好调 —— 同样的像素位移对应更大的参数区间，反而更难瞄准。
 */
export function Knob({
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
      {/* 数值紧跟标签，而不是甩到行尾 ——
          面板有一千像素宽，两端对齐会让眼睛横扫一整屏才对得上。 */}
      <div className="knob-head">
        <span className="knob-label">{label}</span>
        <span className={`knob-val mono ${tone ? `tone-${tone}` : ""}`}>
          {step < 1 && value > 0 ? "+" : ""}
          {step < 1 ? value.toFixed(1) : value}
          <em>{unit}</em>
        </span>
      </div>
      <div className="knob-body">
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
    </div>
  );
}
