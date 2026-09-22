import {
  BUDGET_SCALE_MS,
  FACTS,
  FEATURES,
  LATENCY_BUDGET,
  REPO,
  REQUIREMENTS,
} from "../content";
import { RichText, Section, StatusBadge, TickList } from "./Bits";

export function Features() {
  return (
    <Section band>
      <div className="cards">
        {FEATURES.map((f) => (
          <article key={f.title}>
            <h3 style={{ marginTop: 0 }}>{f.title}</h3>
            <p>{f.body}</p>
            <TickList items={f.points} />
            <StatusBadge status={f.status} />
          </article>
        ))}
      </div>

      {/* 原来这条在 FAQ 里。FAQ 删掉之后必须搬过来 ——
          它不是营销文案，是**能力边界 + 使用者义务**的公开声明。 */}
      <p className="note">
        声线塑形<b>做不到「像某个指定的人」</b>，也不提供任何声线来源 ——
        不上架、不托管任何人的声纹，参考素材完全由你自己提供。
        相应地，<b>取得被模拟者同意是使用者的责任</b>，软件在导入时会明确提示；
        所有输出都带 AI 生成标识与不可听水印，不可关闭。
      </p>
    </Section>
  );
}

export function HowItWorks() {
  return (
    <Section id="how">
      <h2>为什么「实时」这么难，以及我们怎么做的</h2>

      <p className="para">
        人唱歌时，自己的声音通过两条路进大脑：骨传导（零延迟）和耳返（有延迟）。
        两条路一旦拉开距离，人会不自觉地结巴、跑调 ——
        这是<strong>延迟听觉反馈效应</strong>，超过 {FACTS.dafThresholdMs}{" "}
        毫秒之后，开着比关着更糟。
      </p>

      <p className="para">
        所以目标很明确：<strong>必须压进 {FACTS.budgetMs} 毫秒。</strong>
        这不是性能指标，是这个产品成不成立的前提。
      </p>

      <h3>难点不在算法，在音频后端</h3>

      <p className="para">
        跨平台音频库在 Windows 上走的是共享模式，块周期由系统音频引擎决定、改不了。
        实测恒定 480 帧（10 毫秒），光输入输出缓冲就吃掉 35 毫秒 ——
        <strong>算法再快也进不了线。</strong>
      </p>

      <p className="para">
        我们绕开它，直接用 WASAPI 独占模式，把周期压到输入 2 毫秒 / 输出 3 毫秒。
        代价是运行期间独占声卡（系统其他声音会静音），
        这对录音工具是可以接受的 —— 录音时本来就该独占。
      </p>

      {/* 五行数字远不如一根条 —— 要让人看见的是"它贴着 30ms 那条线"，
          而不是让人自己把 2.00 + 9.50 + 3.00 + 15.42 加起来。 */}
      <figure className="budget">
        <div className="budget-bar" aria-hidden>
          {LATENCY_BUDGET.map((r) => (
            <span
              key={r.label}
              className={`seg-${r.kind}`}
              style={{ width: `${(r.ms / BUDGET_SCALE_MS) * 100}%` }}
            />
          ))}
          <span className="budget-line" />
        </div>

        <ol className="budget-legend">
          {LATENCY_BUDGET.map((r) => (
            <li key={r.label}>
              <i className={`seg-${r.kind}`} />
              <span className="bl-name">{r.label}</span>
              <span className="bl-frames mono">{r.frames}</span>
              <span className="bl-ms mono">{r.ms.toFixed(2)} ms</span>
            </li>
          ))}
          <li className="bl-total">
            <i />
            <span className="bl-name">端到端</span>
            <span className="bl-frames mono">48 kHz · WASAPI 独占</span>
            <span className="bl-ms mono">{FACTS.latencyMs} ms</span>
          </li>
        </ol>

        <figcaption>
          横轴满量程就是 {FACTS.budgetMs} ms 那条线。
          <strong>余量只剩 0.08 ms</strong> —— 任何一环再多要一点都进不来。
        </figcaption>
      </figure>

      <p className="note">
        这是同一台机器上实测出来的数字，不是理论估算。
        软件内置诊断页，你可以在自己的机器上量一遍 ——
        它会告诉你设备实际拿到的块大小、时钟漂移、以及有没有丢帧。
      </p>
    </Section>
  );
}

export function Requirements() {
  return (
    <Section id="require" band>
      <h2>跑起来需要什么</h2>

      <div className="req">
        <div className="req-item yes">
          <h4>需要</h4>
          <ul>
            {REQUIREMENTS.yes.map((r) => (
              <li key={r}>
                <RichText text={r} />
              </li>
            ))}
          </ul>
        </div>
        <div className="req-item no">
          <h4>做不到</h4>
          <ul>
            {REQUIREMENTS.no.map((r) => (
              <li key={r}>
                <RichText text={r} />
              </li>
            ))}
          </ul>
        </div>
      </div>

      <p className="note">
        用蓝牙耳机时软件不会假装能用：它会检测到并切换成纯视觉模式
        （只看音高条，不听修正声），同时明确告诉你原因。
      </p>

      {/* 首屏那句"会拦一下，怎么过"链到这里。
          两种症状看起来完全不同，所以分开写 —— 用户是按现象找答案的。 */}
      <h3>第一次运行</h3>
      <p className="para">
        <strong>弹出「Windows 已保护你的电脑」</strong> ——
        程序未做代码签名，点「更多信息 → 仍要运行」即可。
        不放心的话{" "}
        <a href={`${REPO}/releases`}>Release 页</a>
        附有 SHA256，可以自己核对。
      </p>
      <p className="para">
        <strong>双击完全没反应</strong> —— 多半是缺 WebView2 运行时。
        这种情况程序会弹窗说明原因，不会静默失败；
        如果连弹窗都没有，那是被安全软件拦在了启动之前。
      </p>
    </Section>
  );
}

export function Footer() {
  return (
    <footer>
      <div className="foot">
        <span>wego-voice</span>
        <span className="dim">Windows 桌面工具 · 单机运行</span>
        <span className="spacer" />
        <a href={REPO}>源码</a>
        <a href={`${REPO}/releases`}>版本</a>
      </div>
    </footer>
  );
}
