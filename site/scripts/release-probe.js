/**
 * 版本探针 —— 页面加载后直接问 GitHub Release API 要最新版本。
 *
 * 由 `scripts/prerender.mjs` 内联到 `</body>` 之前，`__RELEASE_API__`
 * 在那时被替换成真实地址（从 `src/content.ts` 的 `REPO` 派生，不写死两份）。
 *
 * # 为什么破例加 JS
 *
 * 本站其余部分零 JS（见 `src/entry-server.tsx`），这一处是唯一的例外。
 *
 * 版本号和下载链接是**页面上唯一会自己过期的东西**：构建期烤进 HTML
 * 之后，改 Release、删 Release、发新版，页面全都不知道 —— 最糟的情况
 * 是按钮指着一个已被删除的资产，点下去 404。发版流程会派发一次重新部署，
 * 但那只覆盖"正常发版"这一条路径，覆盖不了在网页上手改 Release。
 *
 * 代价是这一个文件（剥掉注释后 ~1.5 kB，无框架、无依赖），换来的是
 * **下载按钮永远指向当前最新版**。这笔账划算，React 运行时那笔不划算。
 *
 * # 渐进增强，不是替代
 *
 * HTML 里已经有构建期烤好的那一版。探针只在**确实拿到了更好的数据**时
 * 才改 DOM；网络失败、限流、字段缺失一律静默退出，页面保持原样。
 * 禁用 JS 的访客看到的也是可用的页面，只是可能差一个版本。
 */
(function () {
  "use strict";

  var API = "__RELEASE_API__";

  function mb(bytes) {
    return (bytes / 1048576).toFixed(1) + " MB";
  }

  function apply(rel) {
    var tag = rel && rel.tag_name;
    var assets = (rel && rel.assets) || [];
    var exe = null;
    for (var i = 0; i < assets.length; i++) {
      var n = assets[i].name || "";
      // 只发免安装单文件，所以取第一个 .exe。仍然排掉 -setup.exe：
      // 老 Release 里还留着安装包，不能把安装包当免安装版发出去。
      if (/\.exe$/i.test(n) && !/-setup\.exe$/i.test(n)) {
        exe = assets[i];
        break;
      }
    }
    // 有 Release 但没有可下载的资产 —— 与其把按钮改成一个指不到东西的链接，
    // 不如什么都不做，保留构建期那一版。
    if (!tag || !exe || !exe.browser_download_url) return;

    var links = document.querySelectorAll("[data-dl]");
    for (var j = 0; j < links.length; j++) {
      var a = links[j];
      a.href = exe.browser_download_url;
      a.textContent = a.hasAttribute("data-dl-tag") ? "下载 " + tag : "下载";
    }

    set("[data-dl-size]", exe.size ? mb(exe.size) : "");
    set("[data-dl-date]", (rel.published_at || "").slice(0, 10));

    // 构建期没有任何 Release 时这些块是 hidden 的（页面如实显示"尚未发布"）。
    // 现在确认线上有版本了，把它们放出来。
    var hid = document.querySelectorAll("[data-dl-only]");
    for (var k = 0; k < hid.length; k++) hid[k].removeAttribute("hidden");
  }

  function set(sel, text) {
    if (!text) return;
    var els = document.querySelectorAll(sel);
    for (var i = 0; i < els.length; i++) els[i].textContent = text;
  }

  try {
    fetch(API, { headers: { Accept: "application/vnd.github+json" } })
      .then(function (r) {
        // 404 = 还没发过版；403 = 匿名限流（每 IP 每小时 60 次）。
        // 两种都不是错误，是"这次没拿到"，退回已经烤好的那一版。
        return r.ok ? r.json() : null;
      })
      .then(function (rel) {
        if (rel) apply(rel);
      })
      .catch(function () {});
  } catch (e) {
    /* 老浏览器没有 fetch —— 页面照常可用 */
  }
})();
