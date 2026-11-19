import "./styles/toast.css";

import { listen } from "@tauri-apps/api/event";

/**
 * 屏底提示窗口的前端：只负责把 Rust 发来的文字用动画显示 ~3 秒。
 * 窗口本身常驻、透明、置顶、点击穿透；显示/隐藏只发生在 CSS 层，避免 show() 抢焦点。
 */

const HOLD_MS = 2600;

let hideTimer: number | undefined;

function showToast(message: string): void {
  const toast = document.querySelector<HTMLElement>("#toast");
  if (!toast) return;

  toast.textContent = message;
  toast.classList.remove("is-out", "is-in");
  // 强制回流，保证连续两次触发时动画会重新播放
  void toast.offsetWidth;
  toast.classList.add("is-in");

  if (hideTimer !== undefined) window.clearTimeout(hideTimer);
  hideTimer = window.setTimeout(() => {
    toast.classList.remove("is-in");
    toast.classList.add("is-out");
  }, HOLD_MS);
}

type ToastPayload = string | { message?: string } | null;

function extractMessage(payload: ToastPayload): string {
  if (typeof payload === "string") return payload;
  if (payload && typeof payload === "object" && typeof payload.message === "string") {
    return payload.message;
  }
  return "";
}

async function bootstrap(): Promise<void> {
  await listen<ToastPayload>("toast-show", (event) => {
    const message = extractMessage(event.payload);
    if (message.length > 0) showToast(message);
  });
}

void bootstrap().catch((error: unknown) => {
  console.error("toast 初始化失败：", error);
});
