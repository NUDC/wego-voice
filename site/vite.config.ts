import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],

  // 相对路径基址。
  //
  // GitHub Pages 的项目站点挂在 `/<repo>/` 下（本项目是
  // https://nudc.github.io/wego-voice/），绝对路径 `/assets/...`
  // 会 404。写死 `/wego-voice/` 又会在换自定义域名、改仓库名、
  // 或本地 `file://` 打开时再坏一次。
  //
  // `"./"` 让所有资源引用都是相对的 —— 挂到哪个路径下都对。
  // 前提是站点只有一层页面（本站就是单页 + 锚点），这条成立。
  base: "./",

  build: {
    target: "es2020",
    // 官网体积小，内联小资源比多一次往返划算
    assetsInlineLimit: 8192,
    // 预渲染之后客户端不跑 JS，source map 只会白白占带宽
    sourcemap: false,
  },

  server: {
    // 避开 app 的 5173，两边可以同时开着调
    port: 5174,
    strictPort: true,
  },
});
