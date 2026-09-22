import { FACTS, LATENCY_SCALE, NAV, RELEASE } from "../content";
import { Tuner } from "./Tuner";

export function Nav() {
  return (
    <header className="nav">
      {/* 不能写 href="/" —— 站点挂在 GitHub Pages 的 /wego-voice/ 子路径下，
          "/" 会跳到 nudc.github.io 根目录去。回顶部用锚点，跟部署路径无关。 */}
      <a className="brand" href="#top">
        <Mark />
        wego-voice
      </a>
      <nav aria-label="主导航">
        {NAV.map((n) => (
          <a key={n.href} href={n.href}>
            {n.label}
          </a>
        ))}
        <a className="cta" href="#download">
          下载
        </a>
      </nav>
    </header>
  );
}

function Mark() {
  return (
    <svg width="17" height="17" viewBox="0 0 16 16" aria-hidden>
      <path
        d="M1 8h2l1.6-5 2.2 10L9.2 6l1.4 4L12 8h3"
        fill="none"
        stroke="var(--accent)"
        strokeWidth="1.6"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/**
 * 首屏。
 *
 * 排版是**左文右器**：左边说清是什么，右边直接给出那个界面。
 * 一个卖「唱的当下就看见自己唱准了」的工具，首屏不出现那个显示是说不通的。
 *
 * 延迟数字跟着刻度一起给 —— 单看「29.92」没有意义，
 * 看到它压在 30 ms 那条线的下沿才有意义。
 */
export function Hero() {
  return (
    <section className="hero" id="top">
      <div className="hero-text">
        <p className="eyebrow">Windows 桌面工具 · 单机运行 · 无需联网</p>
        <h1>
          唱的时候，
          <br />
          就听见自己唱准了。
        </h1>
        <p className="lede">
          实时修音直接进耳返 —— 不是唱完再修，是<strong>唱的当下</strong>
          就听到。录完把干声换成想要的声线，分轨导出，直接进 DAW。
        </p>

        <div className="hero-actions">
          <a className="btn primary" href="#download">
            {RELEASE.url ? `下载 ${RELEASE.tag}` : "下载（开发中）"}
          </a>
          <a className="btn" href="#how">
            先看技术细节
          </a>
        </div>

        <p className="fineprint">
          需要有线耳机或外置声卡。蓝牙做不了实时耳返 —— 原因见{" "}
          <a href="#require">硬件要求</a>。
        </p>
      </div>

      <div className="hero-visual">
        <Tuner />
        <p className="hero-cap">唱的当下看到的东西：音名、偏差音分、最近 8 秒走势</p>
      </div>

      {/* 延迟是这个产品的全部技术前提，所以它占满首屏下沿。
          数字 + 刻度一起给：29.92 单独看没有意义，压在 30ms 线下才有意义。 */}
      <div className="gauge" id="latency">
        <div className="gauge-num">
          <span className="num">{FACTS.latencyMs}</span>
          <span className="unit">ms</span>
          <span className="cap">端到端实测延迟</span>
        </div>
        <ol className="gauge-scale">
          {LATENCY_SCALE.map((s) => (
            <li key={s.range} className={s.tone}>
              <b className="mono">{s.range}</b>
              <span>{s.desc}</span>
            </li>
          ))}
        </ol>
      </div>
    </section>
  );
}
