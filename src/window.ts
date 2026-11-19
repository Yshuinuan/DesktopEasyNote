/**
 * 主窗口控制 API（P0，R13 无边框窗口）。
 *
 * 主窗口去掉系统标题栏后没有系统「×」，由 P1 在页头放「—」按钮调用 `hideToTray()`：
 * 隐藏主窗口到托盘，进程保留，效果等同原来的系统关闭按钮
 * （`lib.rs` 的 CloseRequested 拦截逻辑同样是隐藏）。托盘左键 / 二次启动可再唤起。
 *
 * 权限：`getCurrentWindow().hide()` 需要 `core:window:allow-hide`
 * （见 `src-tauri/capabilities/default.json`）。
 * 拖动不在这里实现：P1 在页头加 `data-tauri-drag-region`，
 * 对应权限为 `core:window:allow-start-dragging`。
 */

import { getCurrentWindow } from "@tauri-apps/api/window";

/**
 * 隐藏主窗口到托盘（进程保留）。
 *
 * 异步失败不抛出、不打断调用方：这类失败只可能是窗口不存在/权限缺失，
 * 属于环境异常，记录到 console 便于排查即可（不含任何用户数据）。
 */
export function hideToTray(): void {
  void getCurrentWindow()
    .hide()
    .catch((error: unknown) => {
      console.error("隐藏主窗口到托盘失败：", error);
    });
}
