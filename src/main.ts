import "./styles/main.css";

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import { mountRecords } from "./records/index";
import { openSettings } from "./settings";
import type { SettingsView } from "./types";

/**
 * 主窗口外壳：只提供记录页容器与底部状态行。
 * 页面头部（标题 / 保存 / 设置）由 P1 的 `src/records/**` 实现（契约 v1.1 §10.1）。
 */
function buildShell(app: HTMLElement): HTMLElement {
  app.replaceChildren();

  const recordsRoot = document.createElement("main");
  recordsRoot.id = "records-root";
  recordsRoot.className = "records-root";

  const statusLine = document.createElement("footer");
  statusLine.id = "status-line";
  statusLine.className = "status-line";
  statusLine.hidden = true;

  app.append(recordsRoot, statusLine);
  return recordsRoot;
}

let statusTimer: number | undefined;

function setStatus(message: string, kind: "info" | "error" = "info"): void {
  const statusLine = document.querySelector<HTMLElement>("#status-line");
  if (!statusLine) return;
  statusLine.textContent = message;
  statusLine.hidden = message.length === 0;
  statusLine.classList.toggle("is-error", kind === "error");
  if (statusTimer !== undefined) window.clearTimeout(statusTimer);
  if (message.length > 0) {
    statusTimer = window.setTimeout(() => {
      statusLine.hidden = true;
      statusLine.textContent = "";
    }, 5000);
  }
}

/**
 * 首次启动（以及任何一次「没有 key」的启动）自动打开设置。
 * 未填也可以关掉，下次启动会再次引导。
 */
async function openSettingsIfNoKey(): Promise<void> {
  try {
    const settings = await invoke<SettingsView>("get_settings");
    if (!settings.has_api_key) openSettings();
  } catch (error) {
    console.error("读取设置失败：", error);
    setStatus("读取设置失败，可点右上角「设置」重试", "error");
  }
}

async function bootstrap(): Promise<void> {
  const app = document.querySelector<HTMLElement>("#app");
  if (!app) throw new Error("缺少 #app 根节点");

  const recordsRoot = buildShell(app);

  // P1 的实现挂载点：签名固定，P1 只改 src/records/index.ts
  await Promise.resolve(mountRecords(recordsRoot));

  document.addEventListener("settings-saved", () => {
    setStatus("API key 已保存");
  });

  // 托盘「设置key」或首启引导要求前端打开设置；
  // 事件监听失败不应阻断「无 key 自动弹设置」的引导。
  try {
    await listen("open-settings", () => {
      openSettings();
    });
  } catch (error) {
    console.error("监听 open-settings 事件失败：", error);
  }

  await openSettingsIfNoKey();
}

window.addEventListener("DOMContentLoaded", () => {
  void bootstrap().catch((error: unknown) => {
    console.error("应用初始化失败：", error);
    setStatus("应用初始化失败，请重启", "error");
  });
});
