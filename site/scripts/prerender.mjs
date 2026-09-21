/**
 * 构建期预渲染：把 React 渲染成静态 HTML，然后**把 JS 整个删掉**。
 *
 * 详见 `src/entry-server.tsx` 的说明 —— 本站零交互，
 * 发 React 运行时给访客是纯浪费。
 *
 * 流程（由 package.json 的 build 串起来）：
 *   1. vite build                         → dist/（HTML + CSS + 一个用不上的 JS）
 *   2. vite build --ssr entry-server.tsx  → dist-ssr/（Node 可 import 的渲染器）
 *   3. 本脚本                              → 注入 HTML、删 JS、生成 sitemap
 *
 * 任何一步失败都直接抛错退出：**宁可构建红，也不要悄悄发一个空壳页面**。
 */
import { readFile, writeFile, rm, readdir, stat } from "node:fs/promises";
import { existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const dist = join(root, "dist");
const distSsr = join(root, "dist-ssr");

/** 站点绝对地址。canonical / og:url / sitemap 需要它，相对路径顶不了。 */
const SITE_URL = (process.env.SITE_URL ?? "https://nudc.github.io/wego-voice/")
  .replace(/\/*$/, "/");

const kb = (n) => `${(n / 1024).toFixed(1)} kB`;

async function dirSize(dir, filter = () => true) {
  if (!existsSync(dir)) return 0;
  let total = 0;
  for (const name of await readdir(dir)) {
    const p = join(dir, name);
    const s = await stat(p);
    if (s.isDirectory()) total += await dirSize(p, filter);
    else if (filter(name)) total += s.size;
  }
  return total;
}

const htmlPath = join(dist, "index.html");
if (!existsSync(htmlPath)) {
  throw new Error("dist/index.html 不存在 —— vite build 没跑或失败了");
}

// ── 1. 渲染 ──
const entry = join(distSsr, "entry-server.js");
if (!existsSync(entry)) {
  throw new Error("dist-ssr/entry-server.js 不存在 —— SSR 构建没跑或失败了");
}
const { render } = await import(pathToFileURL(entry).href);
const body = render();
if (!body || body.length < 500) {
  // 渲染塌成空字符串时 HTML 结构依然合法，页面却是白的 ——
  // 这种失败不拦住的话会一路发到线上
  throw new Error(`预渲染结果异常：只有 ${body?.length ?? 0} 字符`);
}

let html = await readFile(htmlPath, "utf8");
const jsBefore = await dirSize(join(dist, "assets"), (n) => n.endsWith(".js"));

// ── 2. 注入内容 ──
if (!html.includes('<div id="root"></div>')) {
  throw new Error("index.html 里找不到 <div id=\"root\"></div> 挂载点");
}
html = html.replace('<div id="root"></div>', `<div id="root">${body}</div>`);

// ── 3. 拆掉客户端 JS ──
//
// 连 modulepreload 一起删：留着会让浏览器去拉一个马上就被删掉的文件。
const scriptRe = /\s*<script\b[^>]*\bsrc="[^"]*assets\/[^"]*"[^>]*><\/script>/g;
const preloadRe = /\s*<link\b[^>]*rel="modulepreload"[^>]*>/g;
const removed = (html.match(scriptRe) ?? []).length;
html = html.replace(scriptRe, "").replace(preloadRe, "");

// ── 4. 注入依赖绝对地址的 meta ──
const seo = [
  `<link rel="canonical" href="${SITE_URL}" />`,
  `<meta property="og:url" content="${SITE_URL}" />`,
].join("\n    ");
html = html.replace("</head>", `  ${seo}\n  </head>`);

await writeFile(htmlPath, html, "utf8");

// ── 5. 删掉已经没人引用的 JS 产物 ──
const assets = join(dist, "assets");
if (existsSync(assets)) {
  for (const name of await readdir(assets)) {
    if (name.endsWith(".js") || name.endsWith(".js.map")) {
      await rm(join(assets, name));
    }
  }
}
await rm(distSsr, { recursive: true, force: true });

// ── 6. sitemap ──
//
// 单页站点的 sitemap 收益有限，但成本是零，而且能顺带把
// "这个站的正式地址是什么" 这件事写死在产物里。
await writeFile(
  join(dist, "sitemap.xml"),
  `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>${SITE_URL}</loc><changefreq>monthly</changefreq><priority>1.0</priority></url>
</urlset>
`,
  "utf8",
);
await writeFile(
  join(dist, "robots.txt"),
  `User-agent: *\nAllow: /\nSitemap: ${SITE_URL}sitemap.xml\n`,
  "utf8",
);

const cssSize = await dirSize(assets, (n) => n.endsWith(".css"));
const htmlSize = Buffer.byteLength(html);

console.log("【预渲染】");
console.log(`  站点地址   ${SITE_URL}`);
console.log(`  HTML       ${kb(htmlSize)}（含全部正文）`);
console.log(`  CSS        ${kb(cssSize)}`);
console.log(`  JS         0 B —— 已移除 ${kb(jsBefore)}（${removed} 个 script 标签）`);
