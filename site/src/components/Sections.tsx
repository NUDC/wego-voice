import {
  FACTS,
  RELEASE,
  REPO,
  FAQ,
  FEATURES,
  LATENCY_BUDGET,
  LATENCY_SCALE,
  POSITIONING,
  REQUIREMENTS,
} from "../content";
import { RichText, Section, StatusBadge, TickList } from "./Bits";

export function Features() {
  return (
    <Section band>
      <div className="two">
        {FEATURES.map((f) => (
          <article key={f.title}>
            <h3 style={{ marginTop: 0 }}>{f.title}</h3>
            <p>{f.body}</p>
            <TickList items={f.points} />
            <StatusBadge status={f.status} />
          </article>
        ))}
      </div>
    </Section>
  );
}

export function HowItWorks() {
  return (
    <Section id="how">
      <h2>为什么「实时」这么难，以及我们怎么做的</h2>

      <p className="para">
        人唱歌时，自己的声音通过两条路进大脑：骨传导（零延迟）和耳返（有延迟）。
        两条路叠加会出问题 ——
      </p>

      <div className="scale">
        {LATENCY_SCALE.map((s) => (
          <div key={s.range} className={`seg ${s.tone}`}>
            <b>{s.range}</b>
            <span>{s.desc}</span>
          </div>
        ))}
      </div>

      <p className="para">
        超过 {FACTS.dafThresholdMs} 毫秒，开着比关着更糟。所以目标很明确：
        <strong>必须压进 {FACTS.budgetMs} 毫秒。</strong>
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

      <table className="budget">
        <caption>实测延迟构成（48kHz）</caption>
        <tbody>
          {LATENCY_BUDGET.map((r) => (
            <tr key={r.label} className={r.total ? "total" : undefined}>
              <td>{r.label}</td>
              <td className="n">{r.frames}</td>
              <td className="n">{r.ms}</td>
            </tr>
          ))}
        </tbody>
      </table>

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
      <h2>硬件要求（请先确认）</h2>

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
    </Section>
  );
}

export function Positioning() {
  return (
    <Section>
      <h2>这是个工具，不是 App</h2>
      <div className="two">
        <div>
          <h4>是</h4>
          <TickList items={POSITIONING.is} />
        </div>
        <div>
          <h4>不是</h4>
          <TickList items={POSITIONING.isnt} kind="cross" />
        </div>
      </div>
      <p className="note">
        给要<b>把唱的东西做成成品</b>的人用：翻唱创作者、内容作者、
        自弹自唱、想练音准的人。
      </p>
    </Section>
  );
}

export function Faq() {
  return (
    <Section id="faq" band>
      <h2>常见问题</h2>
      {FAQ.map((item) => (
        <details key={item.q}>
          <summary>{item.q}</summary>
          <p>{item.a}</p>
        </details>
      ))}
    </Section>
  );
}

export function Download() {
  // 没有安装包时**不要**给一个点了会 404 的按钮。
  // 版本信息是构建期注入的，为空是正常状态（见 content.ts 的 RELEASE）。
  if (!RELEASE.url) {
    return (
      <section id="download" className="wrap download">
        <h2>下载</h2>
        <div className="soon">
          <p className="big">尚未发布</p>
          <p>
            实时修音已完成，声线模拟的离线转换还在做。
            没有可下载的版本，也没有预约、抢先体验或等待列表。
          </p>
          <p className="note">
            发布之后这一段会自动换成下载按钮 ——
            构建流水线会从 GitHub Release 取版本号写进页面。
          </p>
        </div>
      </section>
    );
  }

  return (
    <section id="download" className="wrap download">
      <h2>下载</h2>
      <div className="release">
        <div className="release-head">
          <span className="release-tag mono">{RELEASE.tag}</span>
          <span className="release-meta">
            {FACTS.platform}
            {RELEASE.size && ` · ${RELEASE.size}`}
            {RELEASE.date && ` · ${RELEASE.date}`}
          </span>
        </div>

        <a className="btn primary big" href={RELEASE.url}>
          下载安装包
        </a>

        {/* 没有代码签名，SmartScreen 必弹。事先说清楚，
            比让用户以为下到了病毒强。 */}
        <p className="note">
          安装包<strong>未做代码签名</strong>，Windows 会弹「已保护你的电脑」——
          点「更多信息 → 仍要运行」。Release 页附有 SHA256 可自行核对。
        </p>
        <p className="note">
          <a href={`${REPO}/releases`}>所有版本与更新说明</a>
        </p>
      </div>
    </section>
  );
}

export function Footer() {
  return (
    <footer>
      <div className="wrap foot">
        <span>wego-voice</span>
        <span className="dim">Windows 桌面工具 · 单机运行</span>
      </div>
    </footer>
  );
}
