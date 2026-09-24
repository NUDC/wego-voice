import { FACTS, LATENCY_SCALE, NAV, RELEASE, RELEASES } from "../content";
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
        {/* 直链到安装包。GitHub 的 release 资产带 Content-Disposition:
            attachment，点了就是下载，不会跳走。

            `data-dl` 是给版本探针认的（scripts/release-probe.js）——
            页面加载后它会把 href 换成 API 报的最新版。这里烤的是兜底值。 */}
        <a className="cta" data-dl href={RELEASE.url || RELEASES}>
          {RELEASE.url ? "下载" : "尚未发布"}
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
 * 一个卖「实时修音」的工具，首屏不出现那个音高显示是说不通的。
 *
 * 延迟数字跟着刻度一起给 —— 单看「29.92」没有意义，
 * 看到它压在 30 ms 那条线的下沿才有意义。
 */
export function Hero() {
  return (
    <section className="hero" id="top">
      <div className="hero-text">
        <p className="eyebrow">Windows 桌面工具 · 不联网 · 声音不出本机</p>
        {/* ⚠️ 首屏这句必须**两根柱子都带到**，而且**两根都得是现在就能用的**。
            音准和声线是这个工具的两个核心功能，只讲音准会让人以为
            声线那块还没有。

            两个「实时」都是实测过的：修音端到端 29.92 毫秒，
            而声线塑形**不增加延迟**（共振峰靠合成标记间距出音高，
            与颗粒内容无关）。所以这句是陈述，不是口号。

            ⚠️ 不能写成「变成某个指定的人」—— 那一档还在开发。
            写进首屏就是承诺一个没发布的东西，和骗没有区别。
            能力边界在下面的功能区里明确标着。 */}
        <h1>
          实时修音，
          <br />
          实时换声
        </h1>
        <p className="lede">
          <strong>不是唱完再修</strong> —— 麦克风进来的声音，修正之后
          直接回到耳返里，实测 {FACTS.latencyMs} 毫秒。声线同样是当场换的：
          更细、更厚、更亮，<strong>都不动音高、不增加延迟</strong>。
          干声原样落盘，随时能重来。
        </p>

        <div className="hero-actions">
          {/* 点了直接下载，不再跳到页面底部再点一次。
              没有发布版本时退回 Releases 页 —— 那里会如实显示"还没有"，
              而不是给一个点了 404 的链接。

              `data-dl-tag` 让探针知道这个按钮的文案要带版本号。 */}
          <a className="btn primary" data-dl data-dl-tag href={RELEASE.url || RELEASES}>
            {RELEASE.url ? `下载 ${RELEASE.tag}` : "尚未发布"}
          </a>
          <a className="btn" href="#how">
            先看技术细节
          </a>
        </div>

        {/* 始终渲染、按需 hidden，而不是条件渲染：还没发版时页面里也得有
            这些节点，探针拿到数据后才有东西可填 —— 否则「站点部署时还没发版」
            的访客永远看不到版本信息，哪怕线上早就有了。 */}
        <p className="dl-meta" data-dl-only hidden={!RELEASE.url}>
          <span className="mono">{FACTS.platform}</span>
          <span className="mono" data-dl-size>
            {RELEASE.size}
          </span>
          <span className="mono">免安装单文件</span>
          <span className="mono dim">
            <span data-dl-date>{RELEASE.date}</span> 发布
          </span>
        </p>

        {/* 下载卡片删掉之后，两条关键告知搬到这里：
            戴耳机（否则啸叫）、首次运行会被 Windows 拦（否则以为是病毒）。
            详情各自链到对应段落，不在首屏铺开。 */}
        <p className="fineprint">
          需要有线耳机或外置声卡，蓝牙做不了实时耳返（
          <a href="#require">为什么</a>）。
          <span data-dl-only hidden={!RELEASE.url}>
            {" "}首次运行 Windows 会拦一下 ——{" "}
            <a href="#require">怎么过</a>。
          </span>
        </p>
      </div>

      <div className="hero-visual">
        <Tuner />
        <p className="hero-cap">实时显示：音名、偏差音分、最近 8 秒走势</p>
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
