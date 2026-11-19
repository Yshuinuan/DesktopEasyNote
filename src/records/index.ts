/**
 * 记录页（P1 实现，任务书 v1.9）。
 *
 * 页面结构：页头（R14 日期/时间 + 提示语 + 保存/设置/「—」）+
 * 一整页可编辑文档（contenteditable）。没有底部输入框：
 * - 点在文档末尾空白处即可续写；
 * - 有内容的行按 Enter 立即落定（Rust 分配 HH:MM 并写盘），光标进入下一行；
 * - 空行 Enter 不写盘；输入法组合态 Enter 不落定；
 * - 已落定行可改内容/删行/拆行，时间戳保留；
 * - `#`–`######` 渲染为 1–6 级标题（无时间戳），写盘规范成 `#`×n + 空格 + 文字；
 * - 右键记录行选心情（R16：菜单来自 get_moods()，支持添加/删除/恢复）；
 * - 整篇通过 `save_today_document(lines, day)` 原子写回；`day-changed` 时先用
 *   `day: "previous"` 保存旧一天，成功后再载入新一天。
 *
 * 时间/日期全部由 Rust 计算；前端只维护 DOM 上的 `data-time` / `data-mood`。
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import { openSettings } from "../settings";
import { type HeadingPreset, type Mood, type MoodsView } from "../types";
import { hideToTray } from "../window";

import "./records.css";

type HeadingLevel = 1 | 2 | 3 | 4 | 5 | 6;
type SaveTargetDay = "today" | "previous";

type DocLine =
  | { kind: "heading"; level: HeadingLevel; text: string }
  | { kind: "entry"; time: string | null; content: string; mood: Mood | null };

interface ParsedHeading {
  level: HeadingLevel;
  text: string;
}

interface DocumentSnapshot {
  lines: DocLine[];
  elements: HTMLDivElement[];
  texts: string[];
}

interface PageRefs {
  page: HTMLDivElement;
  clock: HTMLElement;
  editor: HTMLElement;
  error: HTMLParagraphElement;
  menu: HTMLDivElement;
  menuItems: HTMLDivElement;
  menuError: HTMLParagraphElement;
  addButton: HTMLButtonElement;
  addForm: HTMLDivElement;
  addInput: HTMLInputElement;
  addError: HTMLParagraphElement;
  titleButton: HTMLButtonElement;
  presetPanel: HTMLDivElement;
  presetList: HTMLDivElement;
  presetPanelError: HTMLParagraphElement;
  presetAddButton: HTMLButtonElement;
  presetAddForm: HTMLDivElement;
  presetAddText: HTMLInputElement;
  presetAddLevel: HTMLSelectElement;
  presetAddError: HTMLParagraphElement;
  saveButton: HTMLButtonElement;
  settingsButton: HTMLButtonElement;
  hideButton: HTMLButtonElement;
}

/** 0–3 个前导空格 + 1–6 个 # + 至少一个空白 + 非空文字；0–3 空格只用于识别。 */
const HEADING_PATTERN = /^ {0,3}(#{1,6})[ \t]+(\S[\s\S]*)$/;

const WEEKDAYS = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"] as const;

/** 页头时钟定时器（模块级，重建/卸载时清理，避免残留多个）。 */
let headerRefreshTimer: number | null = null;

interface HeaderDate {
  month: number;
  day: number;
  weekday: number;
}

function must<T extends Element>(root: ParentNode, selector: string): T {
  const element = root.querySelector<T>(selector);
  if (!element) throw new Error(`记录页缺少元素：${selector}`);
  return element;
}

function describeError(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  return String(error);
}

function parseHeading(raw: string): ParsedHeading | null {
  const line = raw.replace(/[\r\n]+/g, " ");
  const match = HEADING_PATTERN.exec(line);
  if (!match) return null;
  const text = match[2].trim();
  if (text.length === 0) return null;
  return { level: match[1].length as HeadingLevel, text };
}

function readLineText(line: HTMLElement): string {
  return (line.textContent ?? "").replace(/\u00a0/g, " ");
}

function setLineText(line: HTMLElement, text: string): void {
  line.textContent = text;
  // 空块里放一个 <br>，否则 contenteditable 的空行可能点不出光标。
  if (text.length === 0) line.appendChild(document.createElement("br"));
}

function hasTime(line: HTMLElement): boolean {
  const value = line.dataset.time;
  return value !== undefined && value.length > 0;
}

function readMood(line: HTMLElement): Mood | null {
  const value = line.dataset.mood?.trim();
  return value !== undefined && value.length > 0 ? value : null;
}

function setHeadingLineText(line: HTMLElement, level: HeadingLevel, text: string): void {
  line.replaceChildren();
  const mark = document.createElement("span");
  mark.className = "doc-heading-mark";
  mark.textContent = `${"#".repeat(level)} `;
  line.appendChild(mark);
  line.appendChild(document.createTextNode(text));
}

function createHeadingLine(level: HeadingLevel, text: string): HTMLDivElement {
  const line = document.createElement("div");
  line.className = "doc-line";
  line.dataset.kind = "heading";
  line.dataset.level = String(level);
  setHeadingLineText(line, level, text);
  return line;
}

function createEntryLine(time: string, content: string, mood: Mood | null): HTMLDivElement {
  const line = document.createElement("div");
  line.className = "doc-line";
  line.dataset.kind = "entry";
  line.dataset.time = time;
  if (mood !== null) line.dataset.mood = mood;
  setLineText(line, content);
  return line;
}

function createDraftLine(content = ""): HTMLDivElement {
  const line = document.createElement("div");
  line.className = "doc-line";
  line.dataset.kind = "draft";
  setLineText(line, content);
  return line;
}

/** 构建静态 DOM（文案是常量，不含用户数据）。 */
function buildPage(): PageRefs {
  const page = document.createElement("div");
  page.className = "records-page";
  page.innerHTML = `
    <header class="records-header" data-tauri-drag-region="deep">
      <div class="records-heading">
        <h1 class="records-title" data-role="header-clock"></h1>
        <p class="records-day" data-role="header-tagline">记得随便写点记录哟，这可是留存成长的过程呢~</p>
      </div>
      <div class="records-actions">
        <button type="button" class="records-button records-button-primary" data-role="save">保存</button>
        <button type="button" class="records-button" data-role="settings">设置</button>
        <button type="button" class="records-button" data-role="heading-presets">＋ 标题</button>
        <button type="button" class="records-button records-button-hide" data-role="hide" aria-label="隐藏到托盘" title="隐藏到托盘">—</button>
      </div>
    </header>
    <p class="records-error" role="status" aria-live="polite" hidden></p>
    <article
      class="doc"
      contenteditable="false"
      role="textbox"
      aria-multiline="true"
      aria-label="今天的记录文档"
      spellcheck="false"
    ></article>
    <div class="records-mood-menu" role="menu" aria-label="选择心情" hidden>
      <div class="records-mood-items"></div>
      <p class="records-mood-menu-error" role="status" hidden></p>
      <div class="records-mood-add-row">
        <button type="button" class="records-mood-add-button" data-role="mood-add">＋ 添加心情…</button>
        <div class="records-mood-add-form" hidden>
          <input
            class="records-mood-add-input"
            type="text"
            autocomplete="off"
            spellcheck="false"
            aria-label="新心情（最多 8 个字）"
            placeholder="新心情（≤8 字）"
          />
          <p class="records-mood-add-error" role="status" hidden></p>
        </div>
      </div>
    </div>
    <div class="records-heading-panel" role="dialog" aria-label="预设标题" hidden>
      <div class="records-preset-list"></div>
      <p class="records-preset-panel-error" role="status" hidden></p>
      <div class="records-preset-add-row">
        <button type="button" class="records-preset-add-button" data-role="preset-add">＋ 添加标题…</button>
        <div class="records-preset-add-form" hidden>
          <div class="records-preset-form-row">
            <input
              class="records-preset-text-input"
              type="text"
              autocomplete="off"
              spellcheck="false"
              aria-label="预设标题文字"
              placeholder="标题文字（≤20 字）"
            />
            <select class="records-preset-level-select" aria-label="预设标题级别">
              <option value="1">1 级</option>
              <option value="2" selected>2 级</option>
              <option value="3">3 级</option>
              <option value="4">4 级</option>
              <option value="5">5 级</option>
              <option value="6">6 级</option>
            </select>
          </div>
          <p class="records-preset-form-error" role="status" hidden></p>
        </div>
      </div>
    </div>
  `;

  return {
    page,
    clock: must<HTMLElement>(page, '[data-role="header-clock"]'),
    editor: must<HTMLElement>(page, ".doc"),
    error: must<HTMLParagraphElement>(page, ".records-error"),
    menu: must<HTMLDivElement>(page, ".records-mood-menu"),
    menuItems: must<HTMLDivElement>(page, ".records-mood-items"),
    menuError: must<HTMLParagraphElement>(page, ".records-mood-menu-error"),
    addButton: must<HTMLButtonElement>(page, '[data-role="mood-add"]'),
    addForm: must<HTMLDivElement>(page, ".records-mood-add-form"),
    addInput: must<HTMLInputElement>(page, ".records-mood-add-input"),
    addError: must<HTMLParagraphElement>(page, ".records-mood-add-error"),
    titleButton: must<HTMLButtonElement>(page, '[data-role="heading-presets"]'),
    presetPanel: must<HTMLDivElement>(page, ".records-heading-panel"),
    presetList: must<HTMLDivElement>(page, ".records-preset-list"),
    presetPanelError: must<HTMLParagraphElement>(page, ".records-preset-panel-error"),
    presetAddButton: must<HTMLButtonElement>(page, '[data-role="preset-add"]'),
    presetAddForm: must<HTMLDivElement>(page, ".records-preset-add-form"),
    presetAddText: must<HTMLInputElement>(page, ".records-preset-add-form .records-preset-text-input"),
    presetAddLevel: must<HTMLSelectElement>(page, ".records-preset-add-form .records-preset-level-select"),
    presetAddError: must<HTMLParagraphElement>(page, ".records-preset-add-form .records-preset-form-error"),
    saveButton: must<HTMLButtonElement>(page, '[data-role="save"]'),
    settingsButton: must<HTMLButtonElement>(page, '[data-role="settings"]'),
    hideButton: must<HTMLButtonElement>(page, '[data-role="hide"]'),
  };
}

function menuItems(menu: HTMLElement): HTMLElement[] {
  return Array.from(menu.querySelectorAll<HTMLElement>(".records-mood-item"));
}

export function mountRecords(root: HTMLElement): void {
  root.replaceChildren();
  const refs = buildPage();
  root.appendChild(refs.page);

  // 同一页面只允许一个页头时钟定时器（重挂载时先清掉旧的）。
  if (headerRefreshTimer !== null) {
    window.clearTimeout(headerRefreshTimer);
    headerRefreshTimer = null;
  }

  /** 文档是否已成功加载；未加载时禁止编辑/保存，避免用空文档覆盖文件。 */
  let ready = false;
  /** 06:00 切换时若旧一天保存失败，保持冻结，等用户点「保存」重试。 */
  let pendingDaySwitch = false;
  let loadSequence = 0;
  let saveChain: Promise<boolean> = Promise.resolve(true);
  let moodTarget: HTMLDivElement | null = null;
  let activeLine: HTMLDivElement | null = null;
  /** R16：心情列表缓存（挂载时 get_moods，增删后更新）。 */
  let moodsView: MoodsView | null = null;
  let moodsRequest: Promise<MoodsView | null> | null = null;
  let addingMood = false;
  let deletingMood = false;
  let menuAnchor = { x: 0, y: 0 };
  /** R17：预设标题列表缓存 / 面板状态。 */
  let headingPresets: HeadingPreset[] | null = null;
  let headingPresetsRequest: Promise<HeadingPreset[] | null> | null = null;
  let presetPanelRequested = false;
  let presetTargetLine: HTMLDivElement | null = null;
  let presetEditingIndex: number | null = null;
  let presetAdding = false;
  let presetBusy = false;
  /** R15：IME 组合态期间挂起标题识别/整篇 DOM 归一。 */
  let composing = false;
  /** R15：浏览器原生插入段落期间，忽略我们自己触发的 input 归一。 */
  let creatingLine = false;
  let stopListening: (() => void) | null = null;

  const getLines = (): HTMLDivElement[] =>
    Array.from(refs.editor.querySelectorAll<HTMLDivElement>(".doc-line"));

  /**
   * 记录/标题当前所在行：非光标所在的标题行会隐藏 `#` 标记，
   * 光标进入后显示（Typora 式，v1.7）。
   */
  const updateActiveLine = (line: HTMLDivElement | null): void => {
    activeLine = line;
    for (const candidate of getLines()) {
      candidate.classList.toggle("is-active", candidate === line);
    }
  };

  const setError = (message: string): void => {
    refs.error.textContent = message;
    refs.error.hidden = message.length === 0;
  };

  // ---------- R14：页头时钟（记录日按 06:00 边界，分钟级刷新） ----------

  const pad2 = (value: number): string => String(value).padStart(2, "0");

  const headerDateFor = (now: Date): { key: string; date: HeaderDate } => {
    const recordDate = new Date(now.getFullYear(), now.getMonth(), now.getDate());
    if (now.getHours() < 6) recordDate.setDate(recordDate.getDate() - 1);
    const month = recordDate.getMonth() + 1;
    const day = recordDate.getDate();
    return {
      key: `${recordDate.getFullYear()}-${pad2(month)}-${pad2(day)}`,
      date: { month, day, weekday: recordDate.getDay() },
    };
  };

  let headerDayKey = "";
  let headerDate: HeaderDate | null = null;

  const renderHeaderClock = (): void => {
    if (headerDate === null) return;
    const now = new Date();
    refs.clock.textContent = `${headerDate.month}月${headerDate.day}日 ${
      WEEKDAYS[headerDate.weekday]
    } ${pad2(now.getHours())}:${pad2(now.getMinutes())}`;
  };

  const refreshHeaderDate = (): void => {
    const current = headerDateFor(new Date());
    headerDayKey = current.key;
    headerDate = current.date;
    renderHeaderClock();
  };

  const tickHeaderClock = (): void => {
    if (pendingDaySwitch) return;
    const current = headerDateFor(new Date());
    if (headerDate === null || headerDayKey.length === 0) {
      headerDayKey = current.key;
      headerDate = current.date;
    } else if (current.key !== headerDayKey) {
      // 记录日只在 06:00 变；等 day-changed 完成文档切换后再一起更新日期。
      return;
    }
    renderHeaderClock();
  };

  const scheduleHeaderRefresh = (): void => {
    if (headerRefreshTimer !== null) window.clearTimeout(headerRefreshTimer);
    const now = new Date();
    const delay = 60_000 - (now.getSeconds() * 1000 + now.getMilliseconds());
    headerRefreshTimer = window.setTimeout(() => {
      tickHeaderClock();
      scheduleHeaderRefresh();
    }, Math.max(500, delay + 30));
  };

  const clearHeaderRefresh = (): void => {
    if (headerRefreshTimer !== null) {
      window.clearTimeout(headerRefreshTimer);
      headerRefreshTimer = null;
    }
  };

  const ensureTrailingDraft = (): void => {
    const lines = getLines();
    const last = lines[lines.length - 1];
    if (!last) {
      refs.editor.appendChild(createDraftLine());
      return;
    }
    if (last.dataset.kind === "draft" && readLineText(last).trim().length === 0) return;
    last.after(createDraftLine());
  };

  /**
   * R18：只把光标行从不可见拉回可视区，做最小滚动（避免每次输入强制滚底）。
   * 只改滚动位置，不重建节点、不动选区/焦点，因此不会打断输入法组合。
   */
  const ensureLineVisible = (line: HTMLElement): void => {
    if (!line.isConnected) return;
    const view = refs.editor.getBoundingClientRect();
    const rect = line.getBoundingClientRect();
    const margin = 2;
    let delta = 0;
    if (rect.bottom > view.bottom - margin) {
      delta = rect.bottom - (view.bottom - margin);
    } else if (rect.top < view.top + margin) {
      delta = rect.top - (view.top + margin);
    }
    if (delta === 0) return;

    const max = Math.max(0, refs.editor.scrollHeight - refs.editor.clientHeight);
    const target = Math.min(max, Math.max(0, refs.editor.scrollTop + delta));
    if (target !== refs.editor.scrollTop) refs.editor.scrollTop = target;
  };

  const focusLine = (line: HTMLElement, position: "start" | "end" = "end"): void => {
    if (refs.editor.contentEditable !== "true") return;
    refs.editor.focus();
    const range = document.createRange();
    range.selectNodeContents(line);
    range.collapse(position === "start");
    const selection = window.getSelection();
    if (!selection) return;
    selection.removeAllRanges();
    selection.addRange(range);
    ensureLineVisible(line);
  };

  const insertDraftAfter = (line: HTMLElement): HTMLDivElement => {
    const next = line.nextElementSibling;
    if (
      next instanceof HTMLDivElement &&
      next.classList.contains("doc-line") &&
      next.dataset.kind === "draft" &&
      readLineText(next).trim().length === 0
    ) {
      return next;
    }
    const draft = createDraftLine();
    line.after(draft);
    return draft;
  };

  const lineOfNode = (node: Node | null): HTMLDivElement | null => {
    if (node === null) return null;
    const element = node instanceof Element ? node : node.parentElement;
    if (element === null) return null;
    const line = element.closest<HTMLDivElement>(".doc-line");
    return line !== null && refs.editor.contains(line) ? line : null;
  };

  const closestLine = (target: EventTarget | null): HTMLDivElement | null =>
    target instanceof Node ? lineOfNode(target) : null;

  /** contenteditable 的 input/keydown 事件 target 是宿主 `.doc`，用选区反查当前行。 */
  const lineFromSelection = (): HTMLDivElement | null =>
    lineOfNode(window.getSelection()?.anchorNode ?? null);

  const currentLine = (target: EventTarget | null): HTMLDivElement | null =>
    closestLine(target) ?? lineFromSelection() ?? activeLine;

  /** 根据当前文本更新行类型：已带时间的行永远是记录；未落定行可变成标题。 */
  const refreshLine = (line: HTMLDivElement): void => {
    const text = readLineText(line);
    if (hasTime(line)) {
      if (text.trim().length === 0) {
        // 记录行内容被删空：变回空行，时间戳不再展示。
        delete line.dataset.time;
        delete line.dataset.mood;
        line.dataset.kind = "draft";
      } else {
        line.dataset.kind = "entry";
      }
      return;
    }

    const heading = parseHeading(text);
    if (heading !== null) {
      line.dataset.kind = "heading";
      line.dataset.level = String(heading.level);
      delete line.dataset.mood;
    } else {
      line.dataset.kind = "draft";
      delete line.dataset.level;
    }
  };

  /** 粘贴只保留纯文本（多行压成一行），避免 contenteditable 引入富文本/嵌套块。 */
  const insertPlainText = (text: string): void => {
    const selection = window.getSelection();
    if (!selection || selection.rangeCount === 0) return;
    const range = selection.getRangeAt(0);
    const line = lineOfNode(range.commonAncestorContainer);
    if (line === null) return;
    range.deleteContents();
    const node = document.createTextNode(text);
    range.insertNode(node);
    range.setStartAfter(node);
    range.collapse(true);
    selection.removeAllRanges();
    selection.addRange(range);
    refreshLine(line);
    // R18：粘贴多行会被压成单行/变高，确保光标行（行尾）仍在可视区。
    ensureLineVisible(line);
  };

  const collectDocument = (): DocumentSnapshot => {
    const snapshot: DocumentSnapshot = { lines: [], elements: [], texts: [] };
    for (const line of getLines()) {
      const raw = readLineText(line);
      if (raw.trim().length === 0) continue;

      // 只有标记为标题的行才会按标题写盘（拆行出来的后半部分按新记录处理）。
      const heading = line.dataset.kind === "heading" ? parseHeading(raw) : null;
      if (heading !== null) {
        snapshot.lines.push({ kind: "heading", level: heading.level, text: heading.text });
      } else {
        snapshot.lines.push({
          kind: "entry",
          time: hasTime(line) ? (line.dataset.time ?? null) : null,
          content: raw.trim(),
          mood: readMood(line),
        });
      }
      snapshot.elements.push(line);
      snapshot.texts.push(raw);
    }
    return snapshot;
  };

  const applySavedLines = (snapshot: DocumentSnapshot, saved: DocLine[]): void => {
    if (saved.length !== snapshot.lines.length) {
      setError("保存返回的文档与页面不一致，请重试");
      return;
    }

    saved.forEach((docLine, index) => {
      const element = snapshot.elements[index];
      if (!element.isConnected) return;
      // 保存期间用户改了这一行：本次跳过，下次保存会带上最新内容。
      if (readLineText(element) !== snapshot.texts[index]) return;

      if (docLine.kind === "heading") {
        element.dataset.kind = "heading";
        element.dataset.level = String(docLine.level);
        delete element.dataset.time;
        delete element.dataset.mood;
        const canonical = `${"#".repeat(docLine.level)} ${docLine.text}`;
        const mark = element.querySelector(".doc-heading-mark");
        if (!composing && (readLineText(element) !== canonical || mark === null)) {
          setHeadingLineText(element, docLine.level, docLine.text);
        }
      } else {
        element.dataset.kind = "entry";
        if (docLine.time !== null) element.dataset.time = docLine.time;
        else delete element.dataset.time;
        if (docLine.mood !== null) element.dataset.mood = docLine.mood;
        else delete element.dataset.mood;
        // 标题行被改成普通记录时，去掉标记 span，避免留下标题结构。
        if (!composing && element.querySelector(".doc-heading-mark") !== null) {
          setLineText(element, docLine.content);
        }
      }
    });

    if (!composing) ensureTrailingDraft();
  };

  const renderDocument = (lines: DocLine[]): void => {
    refs.editor.replaceChildren();
    // 旧 DOM 已经替换：清掉可能指向旧行的菜单/焦点状态。
    refs.menu.hidden = true;
    moodTarget = null;
    closePresetPanel();
    updateActiveLine(null);
    for (const line of lines) {
      if (line.kind === "heading") {
        refs.editor.appendChild(createHeadingLine(line.level, line.text));
      } else if (line.time !== null) {
        refs.editor.appendChild(createEntryLine(line.time, line.content, line.mood));
      } else {
        refs.editor.appendChild(createDraftLine(line.content));
      }
    }
    ensureTrailingDraft();

    if (document.querySelector(".modal-mask.is-open") === null) {
      const all = getLines();
      const last = all[all.length - 1];
      if (last) focusLine(last, "end");
    } else {
      // 设置面板打开时不能抢焦点，但载入长文档仍先滚到文末草稿，关闭面板即可续写。
      const all = getLines();
      const last = all[all.length - 1];
      if (last) ensureLineVisible(last);
    }
  };

  const setEditable = (editable: boolean): void => {
    ready = editable;
    refs.editor.contentEditable = editable ? "true" : "false";
  };

  const loadDocument = async (): Promise<boolean> => {
    const sequence = ++loadSequence;
    try {
      const lines = await invoke<DocLine[]>("get_today_document");
      if (sequence !== loadSequence) return false;
      setEditable(true);
      renderDocument(lines);
      setError("");
      return true;
    } catch (error) {
      if (sequence !== loadSequence) return false;
      setEditable(false);
      renderDocument([]);
      setError(describeError(error));
      return false;
    }
  };

  const enqueueSave = (day: SaveTargetDay): Promise<boolean> => {
    const task = async (): Promise<boolean> => {
      // 06:00 事件到达时，队列里可能还排着事件前触发的常规保存；
      // 这时它必须改存旧一天，避免写进新一天文件。
      const effectiveDay: SaveTargetDay =
        day === "today" && pendingDaySwitch ? "previous" : day;
      const snapshot = collectDocument();
      try {
        const saved = await invoke<DocLine[]>("save_today_document", {
          lines: snapshot.lines,
          day: effectiveDay,
        });
        applySavedLines(snapshot, saved);
        setError("");
        return true;
      } catch (error) {
        setError(describeError(error));
        return false;
      }
    };
    const result = saveChain.then(task, task);
    saveChain = result.then(() => true, () => false);
    return result;
  };

  async function switchToNewDay(): Promise<boolean> {
    pendingDaySwitch = true;
    setEditable(false);
    const saved = await enqueueSave("previous");
    if (!saved) {
      setError("跨天保存失败，内容仍保留；请点「保存」重试，成功后再切换新一天");
      return false;
    }
    pendingDaySwitch = false;
    await loadDocument();
    // 文档切到新一天后再更新日期，保证「日期与文档同步」。
    refreshHeaderDate();
    return true;
  }

  async function saveToday(): Promise<boolean> {
    if (pendingDaySwitch) return switchToNewDay();
    if (!ready) {
      const loaded = await loadDocument();
      if (!loaded) return false;
    }
    return enqueueSave("today");
  }

  const menuItemElements = (): HTMLElement[] => menuItems(refs.menuItems);

  const positionMenu = (): void => {
    if (refs.menu.hidden) return;
    const rect = refs.menu.getBoundingClientRect();
    const margin = 8;
    let left = menuAnchor.x + 4;
    if (left + rect.width > window.innerWidth - margin) {
      left = window.innerWidth - rect.width - margin;
    }
    left = Math.max(margin, left);

    let top = menuAnchor.y - rect.height - 6;
    if (top < margin) {
      top = Math.min(window.innerHeight - rect.height - margin, menuAnchor.y + 18);
    }
    top = Math.max(margin, top);

    refs.menu.style.left = `${Math.round(left)}px`;
    refs.menu.style.top = `${Math.round(top)}px`;
  };

  const clearAddError = (): void => {
    refs.addError.textContent = "";
    refs.addError.hidden = true;
  };

  const showAddError = (message: string): void => {
    refs.addError.textContent = message;
    refs.addError.hidden = false;
    positionMenu();
  };

  const clearMenuError = (): void => {
    refs.menuError.textContent = "";
    refs.menuError.hidden = true;
  };

  const showMenuError = (message: string): void => {
    refs.menuError.textContent = message;
    refs.menuError.hidden = false;
    positionMenu();
  };

  const resetAddForm = (): void => {
    refs.addForm.hidden = true;
    refs.addButton.hidden = false;
    refs.addInput.value = "";
    clearAddError();
  };

  const openAddForm = (): void => {
    refs.addForm.hidden = false;
    refs.addButton.hidden = true;
    refs.addInput.value = "";
    clearAddError();
    positionMenu();
    refs.addInput.focus();
  };

  const updateMenuSelection = (): void => {
    const selected = moodTarget === null ? null : readMood(moodTarget);
    for (const item of menuItemElements()) {
      const isSelected = item.dataset.mood === selected;
      item.classList.toggle("is-selected", isSelected);
      item.setAttribute("aria-checked", isSelected ? "true" : "false");
    }
  };

  const renderMoodMenu = (view: MoodsView): void => {
    refs.menuItems.replaceChildren();
    for (const word of view.visible) {
      const item = document.createElement("div");
      item.className = "records-mood-item";
      item.dataset.mood = word;
      item.setAttribute("role", "menuitemradio");
      item.setAttribute("aria-checked", "false");

      const label = document.createElement("button");
      label.type = "button";
      label.className = "records-mood-label";
      label.dataset.mood = word;
      label.textContent = word;

      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "records-mood-remove";
      remove.dataset.mood = word;
      remove.textContent = "×";
      remove.title = "删除心情";
      remove.setAttribute("aria-label", `删除心情「${word}」`);

      item.append(label, remove);
      refs.menuItems.appendChild(item);
    }
    updateMenuSelection();
  };

  const closeMenu = (): void => {
    refs.menu.hidden = true;
    moodTarget = null;
    resetAddForm();
    clearMenuError();
  };

  const fetchMoods = async (): Promise<MoodsView | null> => {
    if (moodsView !== null) return moodsView;
    if (moodsRequest !== null) return moodsRequest;
    moodsRequest = (async () => {
      try {
        const view = await invoke<MoodsView>("get_moods");
        moodsView = view;
        return view;
      } catch (error) {
        setError(describeError(error));
        return null;
      } finally {
        moodsRequest = null;
      }
    })();
    return moodsRequest;
  };

  const setLineMood = (line: HTMLDivElement, mood: Mood | null): void => {
    if (mood === null || mood.length === 0) delete line.dataset.mood;
    else line.dataset.mood = mood;
  };

  const submitAddMood = async (): Promise<void> => {
    if (addingMood) return;
    const word = refs.addInput.value.trim();
    if (word.length === 0) {
      showAddError("心情不能为空");
      refs.addInput.focus();
      return;
    }

    addingMood = true;
    clearAddError();
    try {
      const view = await invoke<MoodsView>("add_mood", { word });
      moodsView = view;
      const target = moodTarget;
      renderMoodMenu(view);
      resetAddForm();
      if (target !== null && target.isConnected) {
        // 成功刷新并自动选中新词
        setLineMood(target, word);
        updateMenuSelection();
        if (hasTime(target)) void saveToday();
      }
      if (!refs.menu.hidden) refs.addButton.focus();
      positionMenu();
    } catch (error) {
      // 失败：输入框保留、原地一行错误
      showAddError(describeError(error));
      refs.addInput.focus();
    } finally {
      addingMood = false;
    }
  };

  const deleteMoodWord = async (word: string): Promise<void> => {
    if (deletingMood) return;
    deletingMood = true;
    clearMenuError();
    try {
      const view = await invoke<MoodsView>("delete_mood", { word });
      moodsView = view;
      // 只刷新菜单；已有行尾标签保持不变（历史/隐藏词仍显示）
      renderMoodMenu(view);
      positionMenu();
    } catch (error) {
      showMenuError(describeError(error));
    } finally {
      deletingMood = false;
    }
  };

  const openMoodMenu = async (line: HTMLDivElement, x: number, y: number): Promise<void> => {
    const view = await fetchMoods();
    if (view === null || !line.isConnected) return;
    moodsView = view;
    moodTarget = line;
    menuAnchor = { x, y };
    resetAddForm();
    clearMenuError();
    renderMoodMenu(view);
    refs.menu.hidden = false;
    positionMenu();
  };

  // ---------- R17 预设标题 ----------

  const presetDisplayText = (preset: HeadingPreset): string =>
    `${"#".repeat(Math.min(6, Math.max(1, preset.level)))} ${preset.text}`;

  const clearPresetPanelError = (): void => {
    refs.presetPanelError.textContent = "";
    refs.presetPanelError.hidden = true;
  };

  const showPresetPanelError = (message: string): void => {
    refs.presetPanelError.textContent = message;
    refs.presetPanelError.hidden = false;
    positionPresetPanel();
  };

  const clearPresetFormError = (error: HTMLElement): void => {
    error.textContent = "";
    error.hidden = true;
  };

  const showPresetFormError = (error: HTMLElement, message: string): void => {
    error.textContent = message;
    error.hidden = false;
    positionPresetPanel();
  };

  function fillPresetLevels(select: HTMLSelectElement, selected: number): void {
    select.replaceChildren();
    for (let level = 1; level <= 6; level += 1) {
      const option = document.createElement("option");
      option.value = String(level);
      option.textContent = `${level} 级`;
      if (level === selected) option.selected = true;
      select.appendChild(option);
    }
  }

  function positionPresetPanel(): void {
    if (refs.presetPanel.hidden) return;
    const panelRect = refs.presetPanel.getBoundingClientRect();
    const anchor = refs.titleButton.getBoundingClientRect();
    const margin = 8;

    let left = anchor.right - panelRect.width;
    left = Math.max(margin, left);
    if (left + panelRect.width > window.innerWidth - margin) {
      left = window.innerWidth - panelRect.width - margin;
    }

    let top = anchor.bottom + 6;
    if (top + panelRect.height > window.innerHeight - margin) {
      top = Math.max(margin, anchor.top - panelRect.height - 6);
    }

    refs.presetPanel.style.left = `${Math.round(left)}px`;
    refs.presetPanel.style.top = `${Math.round(top)}px`;
  }

  function resetPresetAddForm(): void {
    refs.presetAddForm.hidden = true;
    refs.presetAddButton.hidden = false;
    refs.presetAddText.value = "";
    refs.presetAddLevel.value = "2";
    clearPresetFormError(refs.presetAddError);
  }

  function closePresetPanel(): void {
    presetPanelRequested = false;
    refs.presetPanel.hidden = true;
    refs.presetList.replaceChildren();
    presetEditingIndex = null;
    presetAdding = false;
    presetTargetLine = null;
    resetPresetAddForm();
    clearPresetPanelError();
  }

  const fetchHeadingPresets = async (): Promise<HeadingPreset[] | null> => {
    if (headingPresets !== null) return headingPresets;
    if (headingPresetsRequest !== null) return headingPresetsRequest;
    headingPresetsRequest = (async () => {
      try {
        const list = await invoke<HeadingPreset[]>("get_heading_presets");
        headingPresets = list;
        return list;
      } catch (error) {
        setError(describeError(error));
        return null;
      } finally {
        headingPresetsRequest = null;
      }
    })();
    return headingPresetsRequest;
  };

  function renderPresetPanel(): void {
    refs.presetList.replaceChildren();
    const presets = headingPresets ?? [];
    if (presets.length === 0) {
      const empty = document.createElement("p");
      empty.className = "records-preset-empty";
      empty.textContent = "还没有预设标题";
      refs.presetList.appendChild(empty);
    } else {
      presets.forEach((preset, index) => {
        refs.presetList.appendChild(
          presetEditingIndex === index
            ? createPresetEditRow(index, preset)
            : createPresetDisplayRow(index, preset),
        );
      });
    }
    refs.presetAddForm.hidden = !presetAdding;
    refs.presetAddButton.hidden = presetAdding;
  }

  function createPresetDisplayRow(index: number, preset: HeadingPreset): HTMLDivElement {
    const item = document.createElement("div");
    item.className = "records-preset-item";
    item.dataset.index = String(index);

    const insert = document.createElement("button");
    insert.type = "button";
    insert.className = "records-preset-insert";
    insert.textContent = presetDisplayText(preset);
    insert.title = "插入此标题";
    insert.addEventListener("click", () => {
      insertHeadingPreset(preset);
    });

    const edit = document.createElement("button");
    edit.type = "button";
    edit.className = "records-preset-edit";
    edit.textContent = "✎";
    edit.title = "编辑";
    edit.setAttribute("aria-label", `编辑预设标题「${preset.text}」`);
    edit.addEventListener("click", () => {
      presetEditingIndex = index;
      presetAdding = false;
      resetPresetAddForm();
      renderPresetPanel();
      positionPresetPanel();
    });

    const remove = document.createElement("button");
    remove.type = "button";
    remove.className = "records-preset-remove";
    remove.textContent = "×";
    remove.title = "删除";
    remove.setAttribute("aria-label", `删除预设标题「${preset.text}」`);
    remove.addEventListener("click", () => {
      void deleteHeadingPreset(index);
    });

    item.append(insert, edit, remove);
    return item;
  }

  function createPresetEditRow(index: number, preset: HeadingPreset): HTMLDivElement {
    const item = document.createElement("div");
    item.className = "records-preset-item is-editing";
    item.dataset.index = String(index);

    const form = document.createElement("div");
    form.className = "records-preset-form";

    const row = document.createElement("div");
    row.className = "records-preset-form-row";

    const text = document.createElement("input");
    text.className = "records-preset-text-input";
    text.type = "text";
    text.autocomplete = "off";
    text.spellcheck = false;
    text.value = preset.text;
    text.setAttribute("aria-label", "预设标题文字");

    const level = document.createElement("select");
    level.className = "records-preset-level-select";
    level.setAttribute("aria-label", "预设标题级别");
    fillPresetLevels(level, preset.level);

    const error = document.createElement("p");
    error.className = "records-preset-form-error";
    error.setAttribute("role", "status");
    error.hidden = true;

    row.append(text, level);
    form.append(row, error);
    item.appendChild(form);

    form.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        event.stopPropagation();
        void submitPresetEdit(index, text, level, error);
      } else if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        cancelPresetEdit();
      }
    });

    form.addEventListener("focusout", (event) => {
      if (presetBusy) return;
      const next = event.relatedTarget;
      if (next instanceof Node && form.contains(next)) return;
      cancelPresetEdit();
    });

    window.setTimeout(() => {
      if (!text.isConnected) return;
      text.focus();
      text.select();
    }, 0);

    return item;
  }

  function cancelPresetEdit(): void {
    if (presetEditingIndex === null) return;
    presetEditingIndex = null;
    renderPresetPanel();
    positionPresetPanel();
  }

  async function submitPresetEdit(
    index: number,
    textInput: HTMLInputElement,
    levelSelect: HTMLSelectElement,
    error: HTMLElement,
  ): Promise<void> {
    if (presetBusy) return;
    presetBusy = true;
    clearPresetFormError(error);
    try {
      const list = await invoke<HeadingPreset[]>("update_heading_preset", {
        index,
        text: textInput.value.trim(),
        level: Number(levelSelect.value),
      });
      headingPresets = list;
      presetEditingIndex = null;
      clearPresetPanelError();
      renderPresetPanel();
      positionPresetPanel();
    } catch (commandError) {
      showPresetFormError(error, describeError(commandError));
      if (textInput.isConnected) textInput.focus();
    } finally {
      presetBusy = false;
    }
  }

  function startPresetAdd(): void {
    presetAdding = true;
    presetEditingIndex = null;
    refs.presetAddForm.hidden = false;
    refs.presetAddButton.hidden = true;
    refs.presetAddText.value = "";
    refs.presetAddLevel.value = "2";
    clearPresetFormError(refs.presetAddError);
    renderPresetPanel();
    positionPresetPanel();
    refs.presetAddText.focus();
  }

  function cancelPresetAdd(): void {
    presetAdding = false;
    resetPresetAddForm();
    positionPresetPanel();
  }

  async function submitPresetAdd(): Promise<void> {
    if (presetBusy) return;
    presetBusy = true;
    clearPresetFormError(refs.presetAddError);
    try {
      const list = await invoke<HeadingPreset[]>("add_heading_preset", {
        text: refs.presetAddText.value.trim(),
        level: Number(refs.presetAddLevel.value),
      });
      headingPresets = list;
      presetAdding = false;
      clearPresetPanelError();
      resetPresetAddForm();
      renderPresetPanel();
      positionPresetPanel();
      if (!refs.presetPanel.hidden) refs.presetAddButton.focus();
    } catch (commandError) {
      showPresetFormError(refs.presetAddError, describeError(commandError));
      refs.presetAddText.focus();
    } finally {
      presetBusy = false;
    }
  }

  async function deleteHeadingPreset(index: number): Promise<void> {
    if (presetBusy) return;
    presetBusy = true;
    clearPresetPanelError();
    try {
      const list = await invoke<HeadingPreset[]>("delete_heading_preset", { index });
      headingPresets = list;
      presetEditingIndex = null;
      renderPresetPanel();
      positionPresetPanel();
    } catch (commandError) {
      showPresetPanelError(describeError(commandError));
    } finally {
      presetBusy = false;
    }
  }

  function insertHeadingPreset(preset: HeadingPreset): void {
    if (!ready || pendingDaySwitch) {
      closePresetPanel();
      return;
    }

    const level = Math.min(6, Math.max(1, preset.level)) as HeadingLevel;
    const heading = createHeadingLine(level, preset.text);
    const lines = getLines();
    const lastLine = lines.length > 0 ? lines[lines.length - 1] : null;
    const target =
      presetTargetLine !== null &&
      presetTargetLine.isConnected &&
      refs.editor.contains(presetTargetLine)
        ? presetTargetLine
        : null;

    let caretLine: HTMLDivElement;
    if (target === null) {
      if (
        lastLine !== null &&
        lastLine.dataset.kind === "draft" &&
        readLineText(lastLine).trim().length === 0
      ) {
        lastLine.before(heading);
        caretLine = lastLine;
      } else {
        const draft = createDraftLine();
        refs.editor.append(heading, draft);
        caretLine = draft;
      }
    } else if (
      target === lastLine &&
      target.dataset.kind === "draft" &&
      readLineText(target).trim().length === 0
    ) {
      // 当前是末尾空行：把标题插到它前面，该空行就是标题下方的新行。
      target.before(heading);
      caretLine = target;
    } else {
      const draft = createDraftLine();
      target.after(heading);
      heading.after(draft);
      caretLine = draft;
    }

    closePresetPanel();
    focusLine(caretLine, "start");
  }

  async function openPresetPanel(): Promise<void> {
    presetPanelRequested = true;
    closeMenu();
    const list = await fetchHeadingPresets();
    if (!presetPanelRequested) return;
    if (list === null) {
      presetPanelRequested = false;
      return;
    }
    headingPresets = list;
    presetEditingIndex = null;
    presetAdding = false;
    resetPresetAddForm();
    clearPresetPanelError();
    renderPresetPanel();
    refs.presetPanel.hidden = false;
    positionPresetPanel();
  }

  const caretOffsetWithin = (line: HTMLElement): number | null => {
    const selection = window.getSelection();
    if (!selection || selection.rangeCount === 0) return null;
    const range = selection.getRangeAt(0);
    if (!line.contains(range.startContainer)) return null;
    const before = document.createRange();
    before.selectNodeContents(line);
    before.setEnd(range.startContainer, range.startOffset);
    return before.toString().length;
  };

  const selectLineEnd = (line: HTMLElement): void => {
    const range = document.createRange();
    range.selectNodeContents(line);
    range.collapse(false);
    const selection = window.getSelection();
    if (!selection) return;
    selection.removeAllRanges();
    selection.addRange(range);
  };

  /** 光标所在的行元素（原生插入段落后，光标在新块里）。 */
  const directLineFromSelection = (): HTMLDivElement | null => {
    const selection = window.getSelection();
    if (!selection || selection.rangeCount === 0) return null;
    const node = selection.anchorNode;
    let element = node instanceof Element ? node : (node?.parentElement ?? null);
    while (element !== null && element.parentElement !== refs.editor) {
      element = element.parentElement;
    }
    if (element === null || element.parentElement !== refs.editor) return null;
    return element instanceof HTMLDivElement ? element : null;
  };

  /**
   * 把浏览器原生创建的新块收编成草稿行（不移动光标、不重建文本节点），
   * 这样 Windows IME 的编辑上下文仍挂在浏览器新建的块上。
   */
  const adoptNativeLine = (line: HTMLDivElement): void => {
    line.classList.add("doc-line");
    line.dataset.kind = "draft";
    delete line.dataset.time;
    delete line.dataset.level;
    delete line.dataset.mood;
    for (const mark of Array.from(line.querySelectorAll(".doc-heading-mark"))) {
      mark.remove();
    }
    if (line.childNodes.length === 0) line.appendChild(document.createElement("br"));
    updateActiveLine(line);
  };

  /**
   * R15：Enter 时让浏览器自己 insertParagraph，而不是同步新建节点再 focus()
   * （后者会让 Windows 输入法来不及挂到新节点，首字被当成英文直入）。
   */
  const insertNativeParagraph = (line: HTMLElement, atEnd: boolean): HTMLDivElement | null => {
    if (atEnd) selectLineEnd(line);
    if (document.activeElement !== refs.editor) refs.editor.focus();

    creatingLine = true;
    let inserted = false;
    try {
      inserted = document.execCommand("insertParagraph", false);
    } finally {
      creatingLine = false;
    }
    if (!inserted) return null;

    const newLine = directLineFromSelection();
    if (newLine === null || newLine === line || !newLine.isConnected) return null;
    adoptNativeLine(newLine);
    // R18：原生 insertParagraph 不会主动把新行滚进可视区；只滚动不改选区，保住 IME。
    ensureLineVisible(newLine);
    return newLine;
  };

  /** 仅在 execCommand 不可用的兜底路径使用：把聚焦延后到两帧之后。 */
  const focusLineSoon = (line: HTMLElement, position: "start" | "end"): void => {
    window.requestAnimationFrame(() => {
      window.requestAnimationFrame(() => focusLine(line, position));
    });
  };

  function handleEnter(line: HTMLDivElement): void {
    if (pendingDaySwitch || !ready || composing) return;
    refreshLine(line);
    const text = readLineText(line);

    if (hasTime(line)) {
      // 已落定记录行：浏览器原生按光标拆分；左保留原时间，右为草稿（时间由保存回填）。
      const originalTime = line.dataset.time ?? null;
      const originalMood = line.dataset.mood ?? null;
      const leftText = text.slice(0, caretOffsetWithin(line) ?? text.length);
      const newLine = insertNativeParagraph(line, false);
      if (newLine === null) {
        // 兜底：手工拆分 + 两帧后聚焦，给 IME 挂接时间。
        const offset = caretOffsetWithin(line) ?? text.length;
        const left = text.slice(0, offset);
        const right = text.slice(offset);
        setLineText(line, left);
        if (left.trim().length === 0) {
          delete line.dataset.time;
          delete line.dataset.mood;
          line.dataset.kind = "draft";
        } else {
          line.dataset.kind = "entry";
        }
        const rightLine = createDraftLine(right);
        line.after(rightLine);
        focusLineSoon(rightLine, "start");
      } else {
        const leftLine = line.isConnected
          ? line
          : (newLine.previousElementSibling as HTMLDivElement | null);
        if (leftLine !== null && leftLine.isConnected) {
          if (leftText.trim().length === 0) {
            delete leftLine.dataset.time;
            delete leftLine.dataset.mood;
            leftLine.dataset.kind = "draft";
          } else {
            leftLine.dataset.kind = "entry";
            if (originalTime !== null) leftLine.dataset.time = originalTime;
            if (originalMood !== null) leftLine.dataset.mood = originalMood;
            else delete leftLine.dataset.mood;
          }
        }
      }
      void saveToday();
      return;
    }

    if (line.dataset.kind === "heading" || text.trim().length > 0) {
      // 标题/有内容的新行：整行落定，浏览器原生建下一行。
      const newLine = insertNativeParagraph(line, true);
      if (newLine === null) {
        focusLineSoon(insertDraftAfter(line), "end");
      }
      void saveToday();
      return;
    }

    // 空行回车：不写盘，只让浏览器新建一行。
    if (insertNativeParagraph(line, true) === null) {
      focusLineSoon(insertDraftAfter(line), "end");
    }
  }

  // ---------- 事件 ----------

  refs.saveButton.addEventListener("click", () => {
    closeMenu();
    void saveToday();
  });

  refs.settingsButton.addEventListener("click", () => {
    closeMenu();
    closePresetPanel();
    openSettings();
  });

  refs.titleButton.addEventListener("mousedown", () => {
    // 点击页头按钮会让编辑器失焦：先记住当前行，插入时按它定位。
    presetTargetLine = lineFromSelection() ?? activeLine;
  });

  refs.titleButton.addEventListener("click", () => {
    if (!refs.presetPanel.hidden) {
      closePresetPanel();
      return;
    }
    if (presetTargetLine === null) {
      presetTargetLine = lineFromSelection() ?? activeLine;
    }
    void openPresetPanel();
  });

  refs.hideButton.addEventListener("click", () => {
    closeMenu();
    closePresetPanel();
    void (async () => {
      const saved = await saveToday();
      if (saved) hideToTray();
    })();
  });

  refs.editor.addEventListener("keydown", (event) => {
    if (event.key !== "Enter") return;
    // 输入法组合态（含旧 WebView 的 229）不落定，交给输入法。
    if (event.isComposing || event.keyCode === 229 || composing) return;
    event.preventDefault();

    const line = currentLine(event.target);
    if (line === null) {
      ensureTrailingDraft();
      const lines = getLines();
      const last = lines[lines.length - 1];
      if (last) focusLine(last, "end");
      return;
    }
    handleEnter(line);
  });

  refs.editor.addEventListener("input", (event) => {
    // R15：原生 insertParagraph 产生的 input 不归一；IME 组合态也不做标题识别。
    if (creatingLine || composing) return;
    const line = currentLine(event.target);
    if (line !== null) refreshLine(line);
    if (getLines().length === 0) {
      const draft = createDraftLine();
      refs.editor.appendChild(draft);
      focusLine(draft, "end");
    }
  });

  refs.editor.addEventListener("compositionstart", () => {
    composing = true;
  });

  refs.editor.addEventListener("compositionend", () => {
    composing = false;
    const line = currentLine(null);
    if (line !== null) refreshLine(line);
  });

  refs.editor.addEventListener("contextmenu", (event) => {
    const line = closestLine(event.target);
    if (line === null) return;
    event.preventDefault();
    closePresetPanel();
    if (pendingDaySwitch) return;
    if (line.dataset.kind === "heading") {
      // 标题行不能带心情：不弹菜单（选择「禁用」而不是显示灰色项）。
      closeMenu();
      return;
    }
    void openMoodMenu(line, event.clientX, event.clientY);
  });

  refs.menu.addEventListener("mousedown", (event) => {
    // 保持编辑区焦点，避免选择心情时提前触发整篇保存/失焦；
    // 但添加心情的输入框需要正常获得焦点。
    const target = event.target;
    if (target instanceof Element && target.closest(".records-mood-add-form") !== null) return;
    event.preventDefault();
  });

  refs.menu.addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof Element)) return;

    const remove = target.closest<HTMLButtonElement>(".records-mood-remove");
    if (remove !== null) {
      const word = remove.dataset.mood;
      if (word !== undefined) void deleteMoodWord(word);
      return;
    }

    const label = target.closest<HTMLButtonElement>(".records-mood-label");
    if (label === null) return;
    const word = label.dataset.mood;
    const line = moodTarget;
    if (word === undefined || line === null || !line.isConnected) {
      closeMenu();
      return;
    }

    setLineMood(line, readMood(line) === word ? null : word);
    closeMenu();
    focusLine(line, "end");
    if (hasTime(line)) void saveToday();
  });

  refs.addButton.addEventListener("click", () => {
    openAddForm();
  });

  refs.addInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      event.stopPropagation();
      void submitAddMood();
    } else if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      resetAddForm();
      refs.addButton.focus();
    }
  });

  refs.addInput.addEventListener("focusout", () => {
    // 失焦取消添加；提交中不打断（成功/失败由 submitAddMood 处理）。
    if (addingMood) return;
    if (!refs.addForm.hidden) resetAddForm();
  });

  refs.presetPanel.addEventListener("mousedown", (event) => {
    // 保持表单/按钮点击不触发编辑器失焦；表单内的输入控件正常聚焦。
    const target = event.target;
    if (target instanceof Element && target.closest(".records-preset-form") !== null) return;
    event.preventDefault();
  });

  refs.presetAddButton.addEventListener("click", startPresetAdd);

  refs.presetAddForm.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      event.stopPropagation();
      void submitPresetAdd();
    } else if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      cancelPresetAdd();
    }
  });

  document.addEventListener("mousedown", (event) => {
    const target = event.target;
    if (refs.presetPanel.hidden) return;
    if (
      target instanceof Node &&
      (refs.presetPanel.contains(target) || refs.titleButton.contains(target))
    ) {
      return;
    }
    closePresetPanel();
  });

  document.addEventListener("mousedown", (event) => {
    if (refs.menu.hidden) return;
    const target = event.target;
    if (target instanceof Node && refs.menu.contains(target)) return;
    closeMenu();
  });

  document.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    if (!refs.menu.hidden) closeMenu();
    if (!refs.presetPanel.hidden) closePresetPanel();
  });

  window.addEventListener("resize", () => {
    closeMenu();
    closePresetPanel();
  });
  window.addEventListener(
    "scroll",
    (event) => {
      // 面板/菜单内部列表滚动时不关闭；页面/文档滚动才关闭。
      const target = event.target;
      if (
        target instanceof Node &&
        (refs.menu.contains(target) || refs.presetPanel.contains(target))
      ) {
        return;
      }
      closeMenu();
      closePresetPanel();
    },
    true,
  );

  refs.editor.addEventListener("mousedown", (event) => {
    if (event.button !== 0) return;

    const clicked = closestLine(event.target);
    // 「行失焦」保存：从当前行点到另一行/空白处时，先把整篇写回。
    if (
      ready &&
      !pendingDaySwitch &&
      activeLine !== null &&
      clicked !== activeLine
    ) {
      void saveToday();
    }
    updateActiveLine(clicked);

    if (event.target !== refs.editor) return;
    event.preventDefault();
    ensureTrailingDraft();
    const lines = getLines();
    const last = lines[lines.length - 1];
    if (last) focusLine(last, "end");
  });

  refs.editor.addEventListener("focusin", (event) => {
    const line = closestLine(event.target) ?? lineFromSelection();
    if (line !== null) updateActiveLine(line);
  });

  document.addEventListener("selectionchange", () => {
    if (refs.editor.contentEditable !== "true") return;
    const selection = window.getSelection();
    if (!selection || selection.rangeCount === 0) return;
    if (!refs.editor.contains(selection.anchorNode)) return;
    updateActiveLine(lineOfNode(selection.anchorNode));
  });

  refs.editor.addEventListener("focusout", (event) => {
    updateActiveLine(null);
    if (!ready || pendingDaySwitch) return;
    const next = event.relatedTarget;
    if (next instanceof Node && refs.editor.contains(next)) return;
    void saveToday();
  });

  refs.editor.addEventListener("paste", (event) => {
    const text = event.clipboardData?.getData("text/plain");
    if (text === undefined || text.length === 0) return;
    event.preventDefault();
    insertPlainText(text.replace(/\r\n?/g, "\n").replace(/\n+/g, " "));
  });

  window.addEventListener("blur", () => {
    updateActiveLine(null);
    if (ready && !pendingDaySwitch) void saveToday();
  });

  document.addEventListener("visibilitychange", () => {
    if (document.hidden && ready && !pendingDaySwitch) void saveToday();
  });

  window.addEventListener("beforeunload", () => {
    if (ready && !pendingDaySwitch) void saveToday();
  });

  const subscribeDayChanged = async (): Promise<void> => {
    try {
      stopListening = await listen<string>("day-changed", () => {
        if (pendingDaySwitch) return;
        if (!ready) {
          // 初始文档还没加载成功：不去用空 DOM 覆盖前一天，直接加载新一天。
          void loadDocument();
          return;
        }
        void switchToNewDay();
      });
    } catch (error) {
      console.error("监听 day-changed 事件失败：", error);
    }
  };
  window.addEventListener("beforeunload", () => {
    stopListening?.();
  });
  window.addEventListener("beforeunload", clearHeaderRefresh);
  window.addEventListener("focus", tickHeaderClock);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) tickHeaderClock();
  });

  refreshHeaderDate();
  scheduleHeaderRefresh();
  void fetchMoods();
  void fetchHeadingPresets();
  void loadDocument();
  void subscribeDayChanged();
}
