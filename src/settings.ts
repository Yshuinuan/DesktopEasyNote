import { invoke } from "@tauri-apps/api/core";

import type { SettingsView } from "./types";

/**
 * 设置弹窗（v1 只有 apikey 一项）。
 * 对外唯一 API：openSettings()。P1 不需要也不应该直接操作弹窗 DOM。
 *
 * 安全约束：key 只从输入框读一次并立即清空；不 console、不写日志、不回显。
 */

let overlay: HTMLElement | null = null;
let keyInput: HTMLInputElement | null = null;
let stateHint: HTMLElement | null = null;
let errorHint: HTMLElement | null = null;
let saveButton: HTMLButtonElement | null = null;
let saving = false;

function ensureModal(): void {
  if (overlay) return;

  overlay = document.createElement("div");
  overlay.className = "modal-mask";
  overlay.innerHTML = `
    <section class="modal" role="dialog" aria-modal="true" aria-labelledby="settings-title">
      <header class="modal-header">
        <h2 class="modal-title" id="settings-title">设置</h2>
        <button class="modal-close" type="button" aria-label="关闭设置">×</button>
      </header>
      <div class="modal-body">
        <label class="field-label" for="api-key-input">DeepSeek API Key</label>
        <input
          id="api-key-input"
          class="text-input"
          type="password"
          autocomplete="off"
          spellcheck="false"
          placeholder="粘贴 DeepSeek API key"
        />
        <p class="field-hint" id="key-state-hint"></p>
        <p class="field-error" id="key-error-hint" hidden></p>
        <p class="field-note">key 仅保存在本机配置文件里，界面不回显；时间与毒舌档首版走默认值。</p>
      </div>
      <footer class="modal-footer">
        <button class="primary-button" type="button" id="key-save-button">存入</button>
      </footer>
    </section>
  `;

  keyInput = overlay.querySelector<HTMLInputElement>("#api-key-input");
  stateHint = overlay.querySelector<HTMLElement>("#key-state-hint");
  errorHint = overlay.querySelector<HTMLElement>("#key-error-hint");
  saveButton = overlay.querySelector<HTMLButtonElement>("#key-save-button");

  overlay
    .querySelector<HTMLButtonElement>(".modal-close")
    ?.addEventListener("click", closeSettings);
  overlay.addEventListener("mousedown", (event) => {
    if (event.target === overlay) closeSettings();
  });
  saveButton?.addEventListener("click", () => {
    void saveKey();
  });
  keyInput?.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      void saveKey();
    }
  });

  document.body.appendChild(overlay);
}

function setError(message: string): void {
  if (!errorHint) return;
  errorHint.textContent = message;
  errorHint.hidden = message.length === 0;
}

function showSavedState(hasKey: boolean): void {
  if (!stateHint) return;
  stateHint.textContent = hasKey ? "已保存（不显示完整 key）" : "尚未保存 key";
  stateHint.classList.toggle("is-saved", hasKey);
}

async function refreshSavedState(): Promise<void> {
  try {
    const settings = await invoke<SettingsView>("get_settings");
    showSavedState(settings.has_api_key);
  } catch (error) {
    console.error("读取设置状态失败：", error);
    setError("读取设置状态失败，请重试");
  }
}

function setSaving(next: boolean): void {
  saving = next;
  if (saveButton) {
    saveButton.disabled = next;
    saveButton.textContent = next ? "保存中…" : "存入";
  }
  if (keyInput) keyInput.disabled = next;
}

async function saveKey(): Promise<void> {
  if (saving || !keyInput) return;

  const key = keyInput.value.trim();
  // 无论成功失败，先清空输入框，避免 key 长时间停在 DOM 里
  keyInput.value = "";

  if (key.length === 0) {
    setError("请先输入 API key");
    return;
  }

  setError("");
  setSaving(true);
  try {
    await invoke("save_api_key", { key });
    await refreshSavedState();
    document.dispatchEvent(new CustomEvent("settings-saved"));
    closeSettings();
  } catch (error) {
    setError(`保存失败：${String(error)}`);
  } finally {
    setSaving(false);
  }
}

export function openSettings(): void {
  ensureModal();
  if (!overlay) return;
  overlay.classList.add("is-open");
  setError("");
  void refreshSavedState();
  window.setTimeout(() => keyInput?.focus(), 0);
}

export function closeSettings(): void {
  if (!overlay) return;
  overlay.classList.remove("is-open");
  if (keyInput) keyInput.value = "";
  setError("");
}

document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") closeSettings();
});
