/**
 * 预渲染入口。构建期在 Node 里跑，产出静态 HTML。
 *
 * # 为什么官网要预渲染
 *
 * 这个站**一个交互都没有**：没有 useState、没有 useEffect，
 * FAQ 折叠用的是原生 `<details>`，平滑滚动是 CSS 的
 * `scroll-behavior`，锚点跳转是浏览器自己的事。
 *
 * 既然如此，把 React 运行时发给访客纯属浪费 —— 200KB 的 JS
 * 只为了画一段从构建那一刻起就不会再变的 HTML。
 *
 * 所以：**用 React 写，但不发 React**。构建期渲染成字符串塞进
 * index.html，客户端框架 JS 整个删掉。产物是纯 HTML + CSS，
 * 首屏不依赖 JS、不受网络抖动影响、禁用 JS 也能看。
 *
 * # 唯一的例外：版本探针
 *
 * `scripts/release-probe.js`（~1.5 kB，内联，无框架）在加载后问一次
 * GitHub Release API，改写下载按钮。它是例外而不是口子的松动：
 * 版本号是页面上**唯一会自己过期**的东西，而它过期的后果是
 * 按钮指向一个已删除的资产。其余一切仍然必须零 JS。
 *
 * # ⚠️ 加交互之前先读这里
 *
 * 一旦某个组件需要 `useState` / 事件处理 / 浏览器 API，
 * 这套「零 JS」就不成立了，必须改成 hydrate：
 * 在 `scripts/prerender.mjs` 里**保留** script 标签，
 * 并把 `main.tsx` 的 `createRoot` 换成 `hydrateRoot`。
 *
 * 在那之前，请优先找原生 HTML 的解法（`<details>`、`<dialog>`、
 * `:target`、纯 CSS 交互）—— 对一个落地页来说，它们几乎总是够用。
 */
import { renderToStaticMarkup } from "react-dom/server";
import App from "./App";

/**
 * 用 `renderToStaticMarkup` 而不是 `renderToString`：
 * 后者会插入 hydration 用的注释标记和 `data-reactroot`，
 * 而我们根本不 hydrate，那些字节是纯浪费。
 */
export function render(): string {
  return renderToStaticMarkup(<App />);
}
