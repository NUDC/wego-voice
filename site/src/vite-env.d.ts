/// <reference types="vite/client" />

// 声明 Vite 注入的模块类型（`*.css`、`*.svg`、`import.meta.env` 等）。
//
// TypeScript 7 起，副作用式 import（`import "./styles.css"`）
// 必须能找到对应的类型声明，否则报 TS2882。
// TS 5 对此是容忍的，所以早先缺了这个文件也没报错。

/**
 * 发版信息的**兜底值**。由 `.github/workflows/pages.yml` 在构建期从
 * GitHub Release API 取出来注入。
 *
 * 线上真正生效的是 `scripts/release-probe.js`：页面加载后直接问 API
 * 要最新版本，然后改写下载按钮。这里烤进去的值负责首屏不闪、
 * 禁用 JS 也能下载、以及 API 限流时不至于开天窗。
 *
 * 还没有任何 Release 时全部为空串，页面显示「尚未发布」。
 */
interface ImportMetaEnv {
  readonly VITE_RELEASE_TAG?: string;
  readonly VITE_RELEASE_DATE?: string;
  readonly VITE_RELEASE_URL?: string;
  readonly VITE_RELEASE_SIZE?: string;
  readonly VITE_RELEASE_FILE?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
