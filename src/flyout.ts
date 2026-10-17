/** The tray flyout (PARLA-60): created per left-click, destroyed on blur, so
 *  everything here is loaded fresh each time it opens. */
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { clock, el, must, providerName, type DictationRecord } from "./records";
import "./theme.css";
import "./flyout.css";

type Toggle = "live_captions" | "dim_other_audio" | "streaming_enabled";

/** Only the fields the flyout reads; saves send back the whole object. */
interface Settings extends Record<Toggle, boolean> {
  active_provider: string;
  hotkey: string;
  local_model: string;
  dim_audio_level: number;
}

interface ProviderInfo {
  id: string;
  display_name: string;
  available: boolean;
  reason: string | null;
}

interface LocalModel {
  id: string;
  name: string;
}

const SHOWN = 4;

const main = document.querySelector<HTMLElement>(".flyout")!;
const status = must<HTMLElement>("status");
const statusText = must<HTMLSpanElement>("status-text");
const hotkey = must<HTMLSpanElement>("hotkey");
const error = must<HTMLParagraphElement>("error");
const last = must<HTMLDivElement>("last");
const recent = must<HTMLUListElement>("recent");
const empty = must<HTMLParagraphElement>("empty");
const providerButtons = [...document.querySelectorAll<HTMLButtonElement>("[data-provider]")];
const tiles = [...document.querySelectorAll<HTMLButtonElement>("[data-toggle]")];

let settings: Settings | null = null;
let providers: ProviderInfo[] = [];
let models: LocalModel[] = [];
let busy = false;

const MODIFIERS = new Set(["ctrl", "control", "alt", "option", "shift", "super", "meta", "cmd", "command", "cmdorctrl", "commandorcontrol"]);

/** Same rule and key names as the Home page (hotkey.rs): with two or more
 *  modifiers, holding them is push-to-talk; otherwise the shortcut toggles. */
function showHotkey(shortcut: string): void {
  const keys = shortcut.split("+").map((key) => key.trim()).filter(Boolean);
  const modifiers = keys.filter((key) => MODIFIERS.has(key.toLowerCase()));
  const hold = modifiers.length >= 2;
  const shown = (hold ? modifiers : keys).flatMap((key, index) => {
    const kbd = el("kbd", undefined, key.replace(/^Key(?=[A-Z]$)/, "").replace(/^Digit(?=\d$)/, ""));
    return index === 0 ? [kbd] : [el("span", "plus", "+"), kbd];
  });
  hotkey.replaceChildren(hold ? "Hold " : "Tap ", ...shown);
}

function renderControls(): void {
  if (settings === null) return;
  const active = providers.find((p) => p.id === settings!.active_provider);
  const model = models.find((m) => m.id === settings!.local_model);
  const engine = settings.active_provider === "parakeet" && model
    ? model.name
    : active?.display_name ?? settings.active_provider;
  const ready = active?.available ?? false;
  statusText.textContent = `${ready ? "Ready" : "Not ready"} · ${engine}`;
  status.classList.toggle("not-ready", !ready);
  status.title = ready ? "" : (active ? (active.reason ?? "") : "Provider not registered");
  for (const button of providerButtons) {
    const provider = providers.find((p) => p.id === button.dataset.provider);
    button.setAttribute("aria-checked", String(button.dataset.provider === settings.active_provider));
    button.disabled = busy || !(provider?.available ?? false);
    button.title = provider?.available ? "" : (provider?.reason ?? "Unavailable");
  }
  for (const tile of tiles) {
    const key = tile.dataset.toggle as Toggle;
    const on = settings[key];
    tile.setAttribute("aria-pressed", String(on));
    tile.querySelector("small")!.textContent = !on ? "Off" : key === "dim_other_audio" ? `${settings.dim_audio_level} %` : "On";
    tile.disabled = busy;
  }
}

function showError(message: unknown): void {
  error.textContent = String(message);
  error.hidden = false;
  void fit();
}

/** Runs one settings change with the controls locked, then re-renders. */
async function change(apply: () => Promise<void>): Promise<void> {
  if (busy || settings === null) return;
  busy = true;
  error.hidden = true;
  renderControls();
  try {
    await apply();
  } catch (problem) {
    showError(problem);
  } finally {
    busy = false;
    renderControls();
  }
}

for (const button of providerButtons) {
  button.addEventListener("click", () => void change(async () => {
    const id = button.dataset.provider!;
    await invoke("select_provider", { id });
    settings!.active_provider = id;
  }));
}

for (const tile of tiles) {
  tile.addEventListener("click", () => void change(async () => {
    const key = tile.dataset.toggle as Toggle;
    // Re-read first: the app window may have saved other changes since open.
    const fresh = await invoke<Settings>("get_settings");
    fresh[key] = !fresh[key];
    await invoke("save_settings", { settings: fresh });
    settings = fresh;
  }));
}

const relative = new Intl.RelativeTimeFormat("en", { numeric: "auto" });

function ago(ms: number): string {
  const minutes = Math.round((Date.now() - ms) / 60_000);
  if (minutes < 1) return "just now";
  if (minutes < 60) return relative.format(-minutes, "minute");
  const hours = Math.round(minutes / 60);
  return hours < 24 ? relative.format(-hours, "hour") : relative.format(-Math.round(hours / 24), "day");
}

function textOf(record: DictationRecord): string {
  return record.error === null ? record.text : `Failed: ${record.error}`;
}

function copyButton(text: string): HTMLButtonElement {
  const button = el("button", "small", "Copy");
  button.type = "button";
  button.setAttribute("aria-label", "Copy transcript");
  let timer: number | undefined;
  button.addEventListener("click", () => {
    navigator.clipboard.writeText(text).then(
      () => {
        button.textContent = "Copied";
        window.clearTimeout(timer);
        timer = window.setTimeout(() => { button.textContent = "Copy"; }, 1400);
      },
      () => { button.textContent = "Failed"; },
    );
  });
  return button;
}

/** Retries the saved clip into history, as the app window does. */
function retryButton(): HTMLButtonElement {
  const button = el("button", "small danger", "Retry");
  button.type = "button";
  button.setAttribute("aria-label", "Retry this dictation");
  button.addEventListener("click", () => {
    button.disabled = true;
    button.textContent = "Retrying…";
    invoke("retry_to_history").then(() => loadHistory().then(fit), (problem) => {
      button.disabled = false;
      button.textContent = "Retry";
      showError(problem);
    });
  });
  return button;
}

function action(record: DictationRecord, retryable: boolean): HTMLButtonElement[] {
  if (retryable) return [retryButton()];
  return record.error === null && record.text.trim() !== "" ? [copyButton(record.text)] : [];
}

function renderHistory(records: DictationRecord[], canRetry: boolean): void {
  // Only the newest failure has its clip saved.
  const retryable = canRetry ? records.find((r) => r.error !== null) : undefined;
  const [newest, ...older] = records;
  empty.hidden = newest !== undefined;
  last.hidden = newest === undefined;
  if (newest !== undefined) {
    last.classList.toggle("failed", newest.error !== null);
    must("last-when").textContent = `Last dictation · ${ago(newest.started_at)}`;
    must("last-provider").textContent = providerName(newest.provider_id);
    must("last-text").textContent = textOf(newest);
    must("last-actions").replaceChildren(...action(newest, newest === retryable));
  }
  recent.replaceChildren(...older.map((record) => {
    const item = el("li", record.error === null ? undefined : "failed");
    const text = el("span", undefined, textOf(record));
    text.title = textOf(record);
    item.append(el("small", undefined, clock(record.started_at)), text, ...action(record, record === retryable));
    return item;
  }));
}

async function loadHistory(): Promise<void> {
  const [records, canRetry] = await Promise.all([
    invoke<DictationRecord[]>("get_history", { limit: SHOWN }),
    invoke<boolean>("has_failed_clip"),
  ]);
  renderHistory(records, canRetry);
}

/** Sizes the (still hidden, on first call) window to the page and shows it. */
function fit(): Promise<void> {
  return invoke("flyout_ready", { height: Math.ceil(main.getBoundingClientRect().height) });
}

for (const button of document.querySelectorAll<HTMLButtonElement>("[data-open]")) {
  // The app window takes focus, which closes the flyout.
  button.addEventListener("click", () => void invoke("open_window", { name: button.dataset.open }).catch(showError));
}

const testMic = must<HTMLButtonElement>("test-mic");
testMic.addEventListener("click", () => {
  testMic.disabled = true;
  testMic.textContent = "Recording 3 s…";
  // When the clip is saved, Explorer takes focus and the flyout closes.
  invoke("test_mic").catch(showError);
});

must<HTMLButtonElement>("quit").addEventListener("click", () => void invoke("quit"));

document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") void getCurrentWindow().destroy();
});

// A hidden webview runs no animation frames: render straight from the data,
// then report the height without waiting for a frame.
Promise.all([
  invoke<Settings>("get_settings"),
  invoke<ProviderInfo[]>("list_providers"),
  invoke<LocalModel[]>("list_models"),
  loadHistory(),
]).then(([loaded, listed, local]) => {
  settings = loaded;
  providers = listed;
  models = local;
  showHotkey(loaded.hotkey);
  renderControls();
}).then(fit).catch(showError);
