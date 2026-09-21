/**
 * 零散的小组件。
 *
 * 只放真正被复用两次以上的东西 —— 官网不需要组件库，
 * 过早抽象只会让改文案变得更麻烦。
 */
import type { ReactNode } from "react";
import type { Status } from "../content";

/**
 * 极简 Markdown 粗体渲染：把 `**x**` 变成 `<b>x</b>`。
 *
 * 内容写在 `content.ts` 里，那里只该有文本；
 * 为了几个粗体就引入 markdown 库不值当，但硬编码 JSX 又会让文案难改。
 * 这个折中足够覆盖官网的需要。
 */
export function RichText({ text }: { text: string }) {
  const parts = text.split(/(\*\*[^*]+\*\*)/g);
  return (
    <>
      {parts.map((p, i) =>
        p.startsWith("**") && p.endsWith("**") ? (
          <b key={i}>{p.slice(2, -2)}</b>
        ) : (
          <span key={i}>{p}</span>
        ),
      )}
    </>
  );
}

export function StatusBadge({ status }: { status: Status }) {
  return status === "done" ? (
    <p className="status ok">已完成</p>
  ) : (
    <p className="status wip">开发中</p>
  );
}

export function TickList({
  items,
  kind = "tick",
}: {
  items: string[];
  kind?: "tick" | "cross";
}) {
  return (
    <ul className={kind === "tick" ? "ticks" : "crosses"}>
      {items.map((it) => (
        <li key={it}>
          <RichText text={it} />
        </li>
      ))}
    </ul>
  );
}

export function Section({
  id,
  band,
  children,
}: {
  id?: string;
  band?: boolean;
  children: ReactNode;
}) {
  // band = 带背景色的通栏，用来在长页面里制造节奏
  return band ? (
    <section id={id} className="band">
      <div className="wrap">{children}</div>
    </section>
  ) : (
    <section id={id} className="wrap">
      {children}
    </section>
  );
}
