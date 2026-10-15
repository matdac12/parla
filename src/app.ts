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
  audio_ms: number;
  first_used_at: number | null;
  cost_usd: number;
  unpriced_audio_ms: number;
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
const audio = must<HTMLSpanElement>("stat-audio");
const cost = must<HTMLSpanElement>("stat-cost");
const costNote = must<HTMLSpanElement>("stat-cost-note");
const projected = must<HTMLSpanElement>("stat-projected");
const projectedNote = must<HTMLSpanElement>("stat-projected-note");
const home = must<HTMLElement>("view-home");
const settingsView = must<HTMLElement>("view-settings");

let records: DictationRecord[] = [];
let canRetry = false;
let limit = PAGE;
let loaded = false;
let loadSequence = 0;
let renderFrame: number | undefined;
const entries = new Map<string, { record: DictationRecord; retryable: boolean; element: HTMLLIElement }>();

const number = new Intl.NumberFormat();
const DAY_MS = 86_400_000;
// The per-year projection waits for a week of use so one busy day cannot skew it.
const PROJECTION_DAYS = 7;
const WISPR_FLOW_PER_YEAR = 144;

function formatDuration(ms: number): string {
  const seconds = Math.round(ms / 1000);
  if (seconds > 0 && seconds < 60) return `${seconds} s`;
  const minutes = Math.round(ms / 60_000);
  const hours = Math.floor(minutes / 60);
  return hours > 0 ? `${hours} h ${minutes % 60} m` : `${minutes} m`;
}

const dollars = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" });
function formatCost(usd: number): string {
  return usd > 0 && usd < 0.005 ? "< $0.01" : dollars.format(usd);
}

function showStats(stats: Stats): void {
  words.textContent = number.format(stats.total_words);
  dictations.textContent = number.format(stats.dictations);
  audio.textContent = formatDuration(stats.audio_ms);
  cost.textContent = formatCost(stats.cost_usd);
  costNote.hidden = stats.unpriced_audio_ms === 0;
  costNote.textContent = `Excludes ${formatDuration(stats.unpriced_audio_ms)} with no known price`;
  const comparison = `vs $${WISPR_FLOW_PER_YEAR} for Wispr Flow`;
  const days = stats.first_used_at === null ? 0 : (Date.now() - stats.first_used_at) / DAY_MS;
  if (stats.first_used_at === null || days < PROJECTION_DAYS) {
    projected.textContent = "—";
    projectedNote.textContent = stats.first_used_at === null
      ? comparison
      : `From ${new Date(stats.first_used_at + PROJECTION_DAYS * DAY_MS).toLocaleDateString()}, ${comparison}`;
    return;
  }
  projected.textContent = formatCost((stats.cost_usd / days) * 365);
  projectedNote.textContent = comparison;
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

function entry(record: DictationRecord, retryable: boolean): HTMLLIElement {
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

  if (retryable) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "copy retry";
    button.textContent = "Retry";
    button.setAttribute("aria-label", "Retry this dictation");
    button.addEventListener("click", () => {
      button.disabled = true;
      button.classList.add("busy");
      button.textContent = "Retrying…";
      invoke("retry_to_history").then(
        () => void refreshRetry(),
        () => {
          button.disabled = false;
          button.classList.remove("busy");
          button.textContent = "Retry failed — try again";
        },
      );
    });
    item.append(button);
  }

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
  // Keep receiving data while minimized or in Settings, but do no list DOM
  // work until it is visible. Coalesce bursts/search input into one frame.
  if (document.hidden || home.hidden || renderFrame !== undefined) return;
  renderFrame = requestAnimationFrame(() => {
    renderFrame = undefined;
    if (!document.hidden && !home.hidden) renderRows();
  });
}

function renderRows(): void {
  const query = search.value.trim().toLowerCase();
  const shown = query === "" ? records : records.filter((r) => r.text.toLowerCase().includes(query));
  // Only the newest failed dictation can be retried (one clip is kept).
  const newestFailed = records.find((r) => r.error !== null);
  const retained = new Set(records.map((record) => record.id));
  for (const id of entries.keys()) if (!retained.has(id)) entries.delete(id);
  let cursor = list.firstChild;
  for (const record of shown) {
    const retryable = canRetry && record === newestFailed;
    let cached = entries.get(record.id);
    if (!cached || cached.record !== record || cached.retryable !== retryable) {
      cached = { record, retryable, element: entry(record, retryable) };
      entries.set(record.id, cached);
    }
    if (cached.element === cursor) cursor = cursor.nextSibling;
    else list.insertBefore(cached.element, cursor);
  }
  while (cursor) {
    const next = cursor.nextSibling;
    cursor.remove();
    cursor = next;
  }
  empty.hidden = shown.length > 0;
  empty.textContent = query === "" ? "No dictations yet. Hold Ctrl+Alt and speak." : "No transcripts match.";
  more.hidden = records.length < limit || limit >= MAX;
}

async function refreshRetry(): Promise<void> {
  canRetry = await invoke<boolean>("has_failed_clip");
  render();
}

async function load(): Promise<void> {
  const sequence = ++loadSequence;
  const before = new Set(records.map((record) => record.id));
  const fetched = await invoke<DictationRecord[]>("get_history", { limit });
  const retry = await invoke<boolean>("has_failed_clip");
  if (sequence !== loadSequence) return;
  // A history read can race a new dictation. Preserve arrivals since the read
  // began, and deduplicate rows already included in the persisted snapshot.
  const arrivals = records.filter((record) => !before.has(record.id));
  const arrivalIds = new Set(arrivals.map((record) => record.id));
  const previous = new Map(records.map((record) => [record.id, record]));
  records = [...arrivals, ...fetched.filter((record) => !arrivalIds.has(record.id)).map((record) => {
    const old = previous.get(record.id);
    return old && JSON.stringify(old) === JSON.stringify(record) ? old : record;
  })].slice(0, limit);
  canRetry = retry;
  loaded = true;
  render();
}

search.addEventListener("input", render);
more.addEventListener("click", () => {
  limit = Math.min(MAX, limit + PAGE);
  void load();
});
function show(view: "home" | "settings"): void {
  home.hidden = view !== "home";
  settingsView.hidden = view !== "settings";
  window.scrollTo(0, 0);
  if (view === "home") {
    if (loaded) render();
    // Refresh after returning from Settings/tray navigation as another window
    // may have cleared history. Unchanged entries keep their cached DOM nodes.
    void load();
  }
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
document.addEventListener("visibilitychange", () => render());

for (const button of document.querySelectorAll<HTMLButtonElement>("[data-open]")) {
  button.addEventListener("click", () => {
    void invoke("open_window", { name: button.dataset.open });
  });
}

const listeners = [listen<string>("navigate", (event) => show(event.payload === "settings" ? "settings" : "home")),
listen<DictationRecord>("dictation://recorded", (event) => {
  if (event.payload.kind !== "dictation") return;
  records = [event.payload, ...records.filter((record) => record.id !== event.payload.id)].slice(0, limit);
  render();
}),
listen<boolean>("failure://changed", (event) => {
  canRetry = event.payload;
  render();
}),
listen<Stats>("stats://updated", (event) => showStats(event.payload))];

void invoke<Stats>("get_stats").then(showStats);
// Listeners must be installed before taking the initial history snapshot.
void Promise.all(listeners).then(() => show(location.hash === "#settings" ? "settings" : "home"));
