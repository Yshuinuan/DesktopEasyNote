import { defineConfig } from "vite";
// @ts-expect-error 未安装 @types/node 时，node:process 没有类型声明
import process from "node:process";
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(() => ({
  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
  build: {
    // Tauri 内置的 WebView2 版本较新，允许使用较新的 JS 语法
    target: "chrome105",
    // 多页：主窗口 index.html + 屏底提示窗口 src/toast.html
    rollupOptions: {
      input: {
        main: "index.html",
        toast: "src/toast.html",
      },
    },
  },
}));
