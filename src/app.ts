import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./theme.css";
import "./app.css";
// The settings view lives in this window; importing wires its form.
import "./settings";

interface DictationRecord {
  id: string;
  kind: string;
  started_at: number;
  provider_id: string;
  audio_ms: number;
  text: string;
  error: string | null;
}

interface Stats {
  total_words: number;
  dictations: number;
}

const PAGE = 200;
const MAX = 2000;

function must<T extends HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (element === null) throw new Error(`missing #${id}`);
  return element as T;
}

const list = must<HTMLOListElement>("list");
const empty = must<HTMLParagraphElement>("empty");
const search = must<HTMLInputElement>("search");
const more = must<HTMLButtonElement>("more");
const words = must<HTMLSpanElement>("stat-words");
const dictations = must<HTMLSpanElement>("stat-dictations");

let records: DictationRecord[] = [];
let limit = PAGE;

const number = new Intl.NumberFormat();
function showStats(stats: Stats): void {
  words.textContent = number.format(stats.total_words);
  dictations.textContent = number.format(stats.dictations);
}

function wordCount(text: string): number {
  // Same rule as stats.rs: whitespace-separated tokens.
  const trimmed = text.trim();
  return trimmed === "" ? 0 : trimmed.split(/\s+/).length;
}

const PROVIDERS: Record<string, string> = { openai: "OpenAI", xai: "xAI Grok", parakeet: "Parakeet" };

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

function entry(record: DictationRecord): HTMLLIElement {
  const item = document.createElement("li");
  item.className = record.error === null ? "entry" : "entry failed";

  const meta = document.createElement("div");
  meta.className = "entry-meta";
  const when = new Date(record.started_at).toLocaleString();
  const provider = PROVIDERS[record.provider_id] ?? record.provider_id;
  const parts = [when, provider, `${(record.audio_ms / 1000).toFixed(1)} s`];
  if (record.error === null) parts.push(`${number.format(wordCount(record.text))} words`);
  for (const part of parts) {
    const span = document.createElement("span");
    span.textContent = part;
    meta.append(span);
  }

  const text = document.createElement("p");
  text.className = "entry-text";
  text.textContent = record.error === null ? record.text : `Failed: ${record.error}`;
  item.append(meta, text);

  if (record.error === null && record.text.trim() !== "") {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "copy";
    button.textContent = "Copy";
    button.setAttribute("aria-label", "Copy transcript");
    let timer: number | undefined;
    button.addEventListener("click", () => {
      void copyText(record.text).then(
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
    item.append(button);
  }
  return item;
}

function render(): void {
  const query = search.value.trim().toLowerCase();
  const shown = query === "" ? records : records.filter((r) => r.text.toLowerCase().includes(query));
  list.replaceChildren(...shown.map(entry));
  empty.hidden = shown.length > 0;
  empty.textContent = query === "" ? "No dictations yet. Hold Ctrl+Alt and speak." : "No transcripts match.";
  more.hidden = records.length < limit || limit >= MAX;
}

async function load(): Promise<void> {
  records = await invoke<DictationRecord[]>("get_history", { limit });
  render();
}

search.addEventListener("input", render);
more.addEventListener("click", () => {
  limit = Math.min(MAX, limit + PAGE);
  void load();
});
const home = must<HTMLElement>("view-home");
const settingsView = must<HTMLElement>("view-settings");
function show(view: "home" | "settings"): void {
  home.hidden = view !== "home";
  settingsView.hidden = view !== "settings";
  window.scrollTo(0, 0);
  if (view === "home") void load();
}
must<HTMLButtonElement>("open-settings").addEventListener("click", () => show("settings"));
must<HTMLButtonElement>("back").addEventListener("click", () => show("home"));
window.addEventListener("keydown", (event) => {
  // Esc leaves settings, except while the hotkey recorder owns the keyboard.
  if (event.key === "Escape" && !settingsView.hidden && !event.defaultPrevented) {
    const capturing = document.getElementById("capture")?.classList.contains("capturing");
    if (!capturing) show("home");
  }
});
void listen<string>("navigate", (event) => show(event.payload === "settings" ? "settings" : "home"));

for (const button of document.querySelectorAll<HTMLButtonElement>("[data-open]")) {
  button.addEventListener("click", () => {
    void invoke("open_window", { name: button.dataset.open });
  });
}

void listen<DictationRecord>("dictation://recorded", (event) => {
  if (event.payload.kind !== "dictation") return;
  records = [event.payload, ...records].slice(0, limit);
  render();
});
void listen<Stats>("stats://updated", (event) => showStats(event.payload));

void invoke<Stats>("get_stats").then(showStats);
void load();
