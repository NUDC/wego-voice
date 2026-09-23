/**
 * 对**已构建产物**里那段内联版本探针做端到端校验。
 *
 * # 为什么值得单独测
 *
 * 它是全站唯一会跑的 JS，而且干的是最要命的一件事：决定下载按钮指向哪儿。
 * 它还是**跨文件契约** —— `Hero.tsx` 挂 `data-dl*` 钩子，探针按钩子找元素。
 * 谁改了其中一边，构建都不会报错，页面看上去也正常，
 * 只有真点下载按钮才会发现指着旧版本。这种错必须在构建期挡住。
 *
 * # 怎么测
 *
 * 不测源码，测 `dist/index.html` 里**真正会发出去的那一段**：
 * 从产物里抠出内联 script，元素清单也从产物 HTML 里扫出来，
 * 然后在一个最小 DOM 桩上跑它。桩只实现探针用到的那几个 API ——
 * 目的是验契约，不是重写浏览器。
 *
 * 四个场景：拿到新版本 / 请求失败 / Release 里没有 exe / 没发过版时把块放出来。
 * 后三个都是"别乱改页面"，它们比第一个更容易写错。
 */
import { readFile } from "node:fs/promises";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { runInNewContext } from "node:vm";
import assert from "node:assert/strict";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const html = await readFile(join(root, "dist", "index.html"), "utf8");

// ── 从产物里抠出内联探针 ──
const inline = html.match(/<script>([\s\S]*?)<\/script>/);
if (!inline) throw new Error("dist/index.html 里没有内联 script —— 探针没被注入？");
const code = inline[1];
if (!code.includes("api.github.com")) {
  throw new Error("内联 script 里没有 API 地址 —— 占位符没被替换？");
}

/** 极简元素桩。只实现探针用到的那几个成员。 */
class El {
  constructor(tag, attrs, text) {
    this.tag = tag;
    this.attrs = attrs;
    this.textContent = text;
  }
  hasAttribute(n) {
    return n in this.attrs;
  }
  removeAttribute(n) {
    delete this.attrs[n];
  }
  get href() {
    return this.attrs.href ?? "";
  }
  set href(v) {
    this.attrs.href = v;
  }
}

/**
 * 从产物 HTML 里扫出所有带 data-dl* 的元素。
 *
 * 用产物而不是手写清单：手写的清单会跟页面分叉，
 * 而分叉正是这个校验要抓的东西。
 */
function scan() {
  const els = [];
  // 只匹配**开标签**。想用 `<tag>…</tag>` 一把捞会漏掉嵌套的那几个 ——
  // `.dl-meta` 那个 <p> 自己带 data-dl-only，里面还套着 data-dl-size，
  // 配对匹配会把整个 <p> 连同内部一起吃掉，内层就再也扫不到了。
  const tagRe = /<(a|span|p)\b([^>]*?\bdata-dl[^>]*?)\/?>/g;
  let m;
  while ((m = tagRe.exec(html))) {
    const attrs = {};
    for (const a of m[2].matchAll(/([a-z-]+)(?:="([^"]*)")?/g)) {
      attrs[a[1]] = a[2] ?? "";
    }
    // 文本取到下一个 `<` 为止。探针只整体替换 textContent，
    // 够用了 —— 这里不是要还原 DOM，是要验契约。
    const text = html.slice(m.index + m[0].length).match(/^[^<]*/)[0];
    els.push(new El(m[1], attrs, text));
  }
  return els;
}

/** 只支持 `[attr]` 形式 —— 探针只用这一种。别的写法直接报错，不要静默返回空。 */
function makeDocument(els) {
  return {
    querySelectorAll(sel) {
      const m = sel.match(/^\[([a-z-]+)\]$/);
      if (!m) throw new Error(`桩不支持的选择器：${sel}`);
      return els.filter((e) => e.hasAttribute(m[1]));
    },
  };
}

/** 跑一遍探针，返回元素清单。`respond` 决定这次 fetch 怎么应答。 */
async function run(respond) {
  const els = scan();
  const ctx = {
    document: makeDocument(els),
    fetch: respond,
    console,
  };
  runInNewContext(code, ctx);
  // 给 fetch 的 then 链留出微任务轮次
  await new Promise((r) => setTimeout(r, 0));
  return els;
}

const ASSET = {
  name: "wego-voice_9.9.9_x64.exe",
  size: 12_897_485,
  browser_download_url:
    "https://github.com/NUDC/wego-voice/releases/download/v9.9.9/wego-voice_9.9.9_x64.exe",
};
const ok = (body) => async () => ({ ok: true, json: async () => body });

const pick = (els, attr) => els.filter((e) => e.hasAttribute(attr));

// ── 场景 1：拿到新版本，页面要被改成它 ──
{
  const els = await run(
    ok({ tag_name: "v9.9.9", published_at: "2026-01-02T03:04:05Z", assets: [ASSET] }),
  );
  const dl = pick(els, "data-dl");
  assert.equal(dl.length, 2, "页面上应该有两个下载按钮（导航 + 首屏）");
  for (const a of dl) {
    assert.equal(a.href, ASSET.browser_download_url, "下载按钮没指向新资产");
  }
  const tagged = dl.find((a) => a.hasAttribute("data-dl-tag"));
  assert.ok(tagged, "首屏按钮缺 data-dl-tag —— 版本号不会显示");
  assert.equal(tagged.textContent, "下载 v9.9.9");
  assert.equal(dl.find((a) => a !== tagged).textContent, "下载");

  assert.equal(pick(els, "data-dl-size")[0]?.textContent, "12.3 MB");
  assert.equal(pick(els, "data-dl-date")[0]?.textContent, "2026-01-02");

  const still = pick(els, "data-dl-only").filter((e) => e.hasAttribute("hidden"));
  assert.equal(still.length, 0, "有版本了却还有块是 hidden 的");
}

// ── 场景 2：请求失败（离线 / 限流 / 404）——一个字都不许改 ──
//
// 这条比"成功"更重要：失败时乱改页面，等于把一个能用的页面改坏。
for (const respond of [
  async () => {
    throw new Error("offline");
  },
  async () => ({ ok: false, status: 403, json: async () => ({}) }),
]) {
  const before = scan().map((e) => [e.href, e.textContent, e.hasAttribute("hidden")]);
  const after = (await run(respond)).map((e) => [e.href, e.textContent, e.hasAttribute("hidden")]);
  assert.deepEqual(after, before, "请求失败时探针动了页面");
}

// ── 场景 3：有 Release 但没有可下载的 exe ──
//
// 与其把按钮改成一个指不到东西的链接，不如原样不动。
// 安装包（-setup.exe）也不算：本项目只发免安装单文件。
for (const assets of [
  [],
  [{ name: "wego-voice_9.9.9_x64-setup.exe", size: 1, browser_download_url: "x" }],
  [{ name: "notes.txt", size: 1, browser_download_url: "x" }],
]) {
  const before = scan().map((e) => e.href);
  const after = (await run(ok({ tag_name: "v9.9.9", assets }))).map((e) => e.href);
  assert.deepEqual(after, before, `没有可下载 exe 时探针仍然改了链接：${JSON.stringify(assets)}`);
}

console.log("【版本探针】4 组场景通过 —— 拿到新版 / 请求失败 / 无可下载资产");
