import { FACTS, NAV, RELEASE } from "../content";

export function Nav() {
  return (
    <header className="nav">
      {/* 不能写 href="/" —— 站点挂在 GitHub Pages 的 /wego-voice/ 子路径下，
          "/" 会跳到 nudc.github.io 根目录去。回顶部用锚点，跟部署路径无关。 */}
      <a className="brand" href="#top">
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

export function Hero() {
  return (
    <section className="hero" id="top">
      <p className="eyebrow">Windows 桌面工具 · 单机运行 · 无需联网</p>
      <h1>
        唱的时候，
        <br />
        就听见自己唱准了。
      </h1>
      <p className="lede">
        实时修音直接进耳返 —— 不是唱完再修，是<strong>唱的当下</strong>
        就听到。录完把干声换成想要的音色，分轨导出，直接进 DAW。
      </p>

      <div className="hero-stat">
        <div className="stat">
          <div className="num">
            {FACTS.latencyMs}
            <span className="unit">ms</span>
          </div>
          <div className="cap">端到端延迟（实测）</div>
        </div>
        <div className="stat">
          <div className="num">0</div>
          <div className="cap">账号 · 服务器 · 上传</div>
        </div>
      </div>

      <div className="hero-actions">
        <a className="btn primary" href="#download">
          {RELEASE.url ? `下载 ${RELEASE.tag}` : "下载（开发中）"}
        </a>
        <a className="btn" href="#how">
          先看技术细节
        </a>
      </div>

      <p className="fineprint">
        需要有线耳机或外置声卡。蓝牙耳机做不了实时耳返 —— 原因见{" "}
        <a href="#require">硬件要求</a>。
      </p>
    </section>
  );
}
