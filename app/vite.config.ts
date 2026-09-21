import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 在固定端口上找 dev server；端口被占时直接报错比静默换端口好，
// 否则窗口会白屏而看不出原因。
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    // WebView2 跟得上现代语法，不必降级编译
    target: "chrome110",
    sourcemap: true,
  },
});
