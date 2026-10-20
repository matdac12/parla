/** Shared by the app window (Home, History, Timings, settings) and the tray flyout. */
export interface DictationRecord {
  id: string;
  kind: string;
  started_at: number;
  provider_id: string;
  /** Stage offsets in ms from key-up (see `timing::DictationRecord`). */
  key_up: number;
  audio_ready: number | null;
  provider_returned: number | null;
  pasted: number | null;
  audio_ms: number;
  /** A command run's instruction (`kind: "command"`). */
  text: string;
  error: string | null;
  /** Command runs, and dictations shown in the Card instead of pasted
   *  (`shown`, PARLA-80); absent on other rows. */
  outcome?: CommandOutcome;
  /** The page an `opened` run opened (PARLA-65), or the To-do a `created`
   *  run created (PARLA-86). */
  url?: string;
  llm_model?: string;
  tokens_in?: number;
  tokens_out?: number;
  llm_returned?: number;
  /** The LLM's text reply, kept for Copy. */
  reply?: string;
  /** A `shown` command reply's kind (PARLA-81); `kind` is the row's origin. */
  reply_kind?: "edit" | "answer";
}

/** How a command run ended. `opened` is the open_url tool (PARLA-65);
 *  `created` is a To-do the create_reminder tool made in Traccia (PARLA-86);
 *  `shown` is a reply in the Card (PARLA-81), or a dictation the Safety Net
 *  sent there (PARLA-80). `pasted` is a reply from before PARLA-81. */
export type CommandOutcome = "pasted" | "opened" | "created" | "failed" | "cancelled" | "shown";

export function must<T extends HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (element === null) throw new Error(`missing #${id}`);
  return element as T;
}

export function el<K extends keyof HTMLElementTagNameMap>(tag: K, className?: string, text?: string): HTMLElementTagNameMap[K] {
  const element = document.createElement(tag);
  if (className !== undefined) element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
}

/** Makes `parent`'s children exactly `nodes`, moving only what changed, so
 *  cached rows are not detached and re-attached on every render. */
export function reconcile(parent: Node, nodes: Node[]): void {
  let cursor = parent.firstChild;
  for (const node of nodes) {
    if (node === cursor) cursor = cursor.nextSibling;
    else parent.insertBefore(node, cursor);
  }
  while (cursor) {
    const next = cursor.nextSibling;
    cursor.remove();
    cursor = next;
  }
}

const PROVIDERS: Record<string, string> = { openai: "OpenAI", xai: "xAI Grok", parakeet: "Parakeet" };

export function providerName(id: string): string {
  return PROVIDERS[id] ?? id;
}

export function seconds(ms: number): string {
  return `${(ms / 1000).toFixed(1)} s`;
}

// One formatter each: toLocale*String with options builds a new one per call.
const timeFormat = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit" });
const fullFormat = new Intl.DateTimeFormat(undefined, { dateStyle: "short", timeStyle: "medium" });

export function clock(ms: number): string {
  return timeFormat.format(ms);
}

export function fullDate(ms: number): string {
  return fullFormat.format(ms);
}

/** `get_settings` / `save_settings`. Saves send back the whole object. */
export interface Settings {
  active_provider: string;
  hotkey: string;
  restore_delay_ms: number;
  paste_compatibility_mode: boolean;
  auto_send: boolean;
  safety_net: boolean;
  streaming_enabled: boolean;
  live_captions: boolean;
  vocabulary: string[];
  local_model: string;
  dim_other_audio: boolean;
  dim_audio_level: number;
  /** Command mode (the Settings "Command mode" card). `hotkey` is a modifier
   *  hold such as "LCtrl+Shift"; `reasoning_effort` is "none" or "low";
   *  `reminders` is the create_reminder tool, which needs `providers.traccia`
   *  (`base_url`, `token`). */
  command: { enabled: boolean; hotkey: string; model: string; reasoning_effort: string; reminders: boolean };
  providers: Record<string, Record<string, unknown>>;
}

/** `list_providers`. */
export interface ProviderInfo {
  id: string;
  display_name: string;
  available: boolean;
  reason: string | null;
}

/** `list_models` (PARLA-37): one entry per local Parakeet export. */
export interface LocalModelStatus {
  id: string;
  name: string;
  size: number;
  model_dir: string;
  available: boolean;
  downloading: boolean;
  selected: boolean;
}

/** The "Ready · <engine>" line: the active provider, or the local model's name. */
export function engineStatus(
  settings: Settings,
  providers: ProviderInfo[],
  models: LocalModelStatus[],
): { engine: string; ready: boolean; reason: string | null } {
  const provider = providers.find((p) => p.id === settings.active_provider);
  const model = models.find((m) => m.id === settings.local_model);
  return {
    engine: settings.active_provider === "parakeet" && model
      ? model.name
      : provider?.display_name ?? settings.active_provider,
    ready: provider?.available ?? false,
    reason: provider ? provider.reason : "Provider not registered",
  };
}

const MODIFIERS = new Set(["ctrl", "control", "alt", "option", "shift", "super", "meta", "cmd", "command", "cmdorctrl", "commandorcontrol"]);

/** Mirrors hotkey.rs: two or more modifiers make holding them push-to-talk and
 *  tapping the full shortcut lock hands-free; otherwise it is tap on, tap off. */
export function parseShortcut(hotkey: string): { keys: string[]; modifiers: string[]; hold: boolean } {
  const keys = hotkey.split("+").map((key) => key.trim()).filter(Boolean);
  const modifiers = keys.filter((key) => MODIFIERS.has(key.toLowerCase()));
  return { keys, modifiers, hold: modifiers.length >= 2 };
}

/** `keys` as <kbd> chips joined by "+", with key codes shown as their key. */
export function combo(keys: string[]): Node[] {
  return keys.flatMap((key, index) => {
    const label = key.replace(/^Key(?=[A-Z]$)/, "").replace(/^Digit(?=\d$)/, "");
    const kbd = el("kbd", undefined, label);
    return index === 0 ? [kbd] : [el("span", "plus", "+"), kbd];
  });
}

async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // Fallback for contexts where the async clipboard API is unavailable.
    const area = document.createElement("textarea");
    area.value = text;
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.appendChild(area);
    area.select();
    const ok = document.execCommand("copy");
    area.remove();
    if (!ok) throw new Error("copy failed");
  }
}

export function copyButton(text: string, label = "Copy transcript"): HTMLButtonElement {
  const button = el("button", "small", "Copy");
  button.type = "button";
  button.setAttribute("aria-label", label);
  let timer: number | undefined;
  button.addEventListener("click", () => {
    void copyText(text).then(
      () => {
        button.textContent = "Copied";
        button.classList.add("copied");
        window.clearTimeout(timer);
        timer = window.setTimeout(() => {
          button.textContent = "Copy";
          button.classList.remove("copied");
        }, 1400);
      },
      () => {
        button.textContent = "Failed";
      },
    );
  });
  return button;
}

/** "670 MB": a download's size, in the decimal units the model sizes use. */
export function formatSize(bytes: number): string {
  return `${Math.round(bytes / 1_000_000)} MB`;
}

/** A status line inside a card (the provider switch, Test connection, the
 *  wizard's steps). */
export function setStatusLine(element: HTMLElement, text: string, kind: "success" | "error" | "" = ""): void {
  element.textContent = text;
  element.className = kind ? `status provider-status status-${kind}` : "status provider-status";
}
