/// <reference types="vite/client" />

// 声明 Vite 注入的模块类型（`*.css`、`*.svg`、`import.meta.env` 等）。
//
// TypeScript 7 起，副作用式 import（`import "./styles.css"`）
// 必须能找到对应的类型声明，否则报 TS2882。
// TS 5 对此是容忍的，所以早先缺了这个文件也没报错。

/**
 * 发版信息。由 `.github/workflows/pages.yml` 在**构建期**从
 * GitHub Release API 取出来注入。
 *
 * 为什么不在页面上用 JS 拉：官网刻意做成零 JavaScript（见 entry-server.tsx）。
 * 为了显示一个版本号就把 React 运行时和一次网络请求加回去，不划算 ——
 * 发版时重新部署一次就够了。
 *
 * 还没有任何 Release 时全部为空串，页面显示「尚未发布」。
 */
interface ImportMetaEnv {
  readonly VITE_RELEASE_TAG?: string;
  readonly VITE_RELEASE_DATE?: string;
  readonly VITE_RELEASE_URL?: string;
  readonly VITE_RELEASE_SIZE?: string;
  readonly VITE_RELEASE_FILE?: string;
  /** 安装版（NSIS）。免安装版走上面那组字段。 */
  readonly VITE_RELEASE_SETUP_URL?: string;
  readonly VITE_RELEASE_SETUP_SIZE?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
