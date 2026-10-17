import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./theme.css";
import "./app.css";
import { clock, el, fullDate, must, providerName, reconcile, seconds, type DictationRecord } from "./records";
// The settings pages live in this window; importing wires their form.
import { onApplied } from "./settings";
import { renderTimings } from "./timings";

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

const PAGES = ["home", "history", "timings", "transcription", "dictation", "audio", "vocabulary", "clipboard"] as const;
type Page = (typeof PAGES)[number];
/** Pages that show dictation records; the others are settings pages. */
const RECORD_PAGES: ReadonlySet<Page> = new Set(["home", "history", "timings"]);

/** "settings" is the tray's name for the first settings page. */
function toPage(name: string): Page {
  if (name === "settings") return "transcription";
  return (PAGES as readonly string[]).includes(name) ? (name as Page) : "home";
}

const main = must<HTMLElement>("main");
const form = must<HTMLFormElement>("settings-form");
const pages = new Map(PAGES.map((id) => [id, must<HTMLElement>(`page-${id}`)]));
const navButtons = document.querySelectorAll<HTMLButtonElement>(".nav [data-page]");
const readyLine = must<HTMLParagraphElement>("ready");
const readyText = must<HTMLSpanElement>("ready-text");
const historyCount = must<HTMLSpanElement>("history-count");

const how = must<HTMLDListElement>("how");
const testMic = must<HTMLButtonElement>("test-mic");
const failure = must<HTMLDivElement>("failure");
const failureText = must<HTMLSpanElement>("failure-text");
const failureRetry = must<HTMLButtonElement>("failure-retry");
const words = must<HTMLSpanElement>("stat-words");
const dictations = must<HTMLSpanElement>("stat-dictations");
const audio = must<HTMLSpanElement>("stat-audio");
const cost = must<HTMLSpanElement>("stat-cost");
const costNote = must<HTMLSpanElement>("stat-cost-note");
const projected = must<HTMLSpanElement>("stat-projected");
const recent = must<HTMLUListElement>("recent");
const recentEmpty = must<HTMLParagraphElement>("recent-empty");

const lede = must<HTMLParagraphElement>("history-lede");
const search = must<HTMLInputElement>("search");
const chips = document.querySelectorAll<HTMLButtonElement>("[data-filter]");
const days = must<HTMLDivElement>("days");
const empty = must<HTMLParagraphElement>("empty");
const more = must<HTMLButtonElement>("more");
const clear = must<HTMLButtonElement>("clear");
const confirmBox = must<HTMLSpanElement>("confirm");
const clearStatus = must<HTMLSpanElement>("clear-status");

let page: Page = "home";
let records: DictationRecord[] = [];
let canRetry = false;
let limit = PAGE;
let loaded = false;
let loadSequence = 0;
let renderFrame: number | undefined;
let failedOnly = false;
const entries = new Map<string, { record: DictationRecord; retryable: boolean; element: HTMLLIElement }>();
const dayGroups = new Map<string, { head: HTMLHeadingElement; list: HTMLUListElement }>();

// Dismiss hides the banner for that failure only, across window reopenings.
const DISMISSED_KEY = "parla.dismissedFailure";
let dismissed = localStorage.getItem(DISMISSED_KEY);
/** The failure the banner shows; its Retry button resets for a new one. */
let bannerFor: string | null = null;

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
  costNote.textContent = `excludes ${formatDuration(stats.unpriced_audio_ms)} with no known price`;
  if (stats.first_used_at === null) {
    projected.textContent = `vs $${WISPR_FLOW_PER_YEAR}/yr for Wispr Flow`;
    return;
  }
  const used = (Date.now() - stats.first_used_at) / DAY_MS;
  projected.textContent = used < PROJECTION_DAYS
    ? `yearly estimate from ${new Date(stats.first_used_at + PROJECTION_DAYS * DAY_MS).toLocaleDateString()}`
    : `≈ ${formatCost((stats.cost_usd / used) * 365)}/yr vs $${WISPR_FLOW_PER_YEAR} Wispr Flow`;
}

const MODIFIERS = new Set(["ctrl", "control", "alt", "option", "shift", "super", "meta", "cmd", "command", "cmdorctrl", "commandorcontrol"]);

function combo(keys: string[]): Node[] {
  return keys.flatMap((key, index) => {
    const label = key.replace(/^Key(?=[A-Z]$)/, "").replace(/^Digit(?=\d$)/, "");
    const kbd = el("kbd", undefined, label);
    return index === 0 ? [kbd] : [el("span", "plus", "+"), kbd];
  });
}

/** Mirrors hotkey.rs: two or more modifiers make holding them push-to-talk and
 *  tapping the full shortcut lock hands-free; otherwise it is tap on, tap off. */
function showShortcut(hotkey: string): void {
  const keys = hotkey.split("+").map((key) => key.trim()).filter(Boolean);
  const modifiers = keys.filter((key) => MODIFIERS.has(key.toLowerCase()));
  const rows: [string, ...(string | Node[])[]][] = modifiers.length >= 2
    ? [
        ["Push to talk", "Hold ", combo(modifiers)],
        ["Hands-free", "Tap ", combo(keys), [el("span", "muted", " · "), ...combo(modifiers), el("span", "muted", " stops")]],
        ["Cancel", combo(["Esc"])],
      ]
    : [
        ["Start / stop", "Tap ", combo(keys)],
        ["Cancel", combo(["Esc"])],
      ];
  how.replaceChildren(...rows.flatMap(([label, ...parts]) => {
    const dd = el("dd");
    for (const part of parts) dd.append(...(typeof part === "string" ? [part] : part));
    return [el("dt", undefined, label), dd];
  }));
}

function wordCount(text: string): number {
  // Same rule as stats.rs: whitespace-separated tokens.
  const trimmed = text.trim();
  return trimmed === "" ? 0 : trimmed.split(/\s+/).length;
}

const dayFormat = new Intl.DateTimeFormat(undefined, { weekday: "long", day: "numeric", month: "long" });
const dayYearFormat = new Intl.DateTimeFormat(undefined, { weekday: "long", day: "numeric", month: "long", year: "numeric" });
const shortDayFormat = new Intl.DateTimeFormat(undefined, { day: "numeric", month: "short" });

function startOfDay(ms: number): number {
  const date = new Date(ms);
  date.setHours(0, 0, 0, 0);
  return date.getTime();
}

function dayLabel(day: number, today: number): string {
  if (day === today) return "Today";
  if (day === startOfDay(today - 1)) return "Yesterday";
  const sameYear = new Date(day).getFullYear() === new Date(today).getFullYear();
  return (sameYear ? dayFormat : dayYearFormat).format(day);
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

/** Retries the saved clip into history (nothing is pasted: Parla has focus). */
function wireRetry(button: HTMLButtonElement): void {
  button.addEventListener("click", () => {
    button.disabled = true;
    button.textContent = "Retrying…";
    invoke("retry_to_history").then(
      () => void refreshRetry(),
      () => {
        button.disabled = false;
        button.textContent = "Retry failed — try again";
      },
    );
  });
}

function copyButton(text: string): HTMLButtonElement {
  const button = el("button", "small", "Copy");
  button.type = "button";
  button.setAttribute("aria-label", "Copy transcript");
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

/** `today` is set on Home, where rows are not grouped by day: older rows
 *  then show their date instead of their time. */
function row(record: DictationRecord, retryable: boolean, today?: number): HTMLLIElement {
  const item = el("li", record.error === null ? undefined : "failed");
  const ok = record.error === null;
  const when = el("span", "tm", today !== undefined && startOfDay(record.started_at) !== today
    ? shortDayFormat.format(record.started_at)
    : clock(record.started_at));
  when.title = fullDate(record.started_at);

  const text = el("p", "tx", ok ? record.text : `Failed: ${record.error}`);
  // Click expands the two-line clamp, unless the click ended a text selection.
  text.addEventListener("click", () => {
    if (window.getSelection()?.isCollapsed !== false) text.classList.toggle("expanded");
  });
  const meta = [providerName(record.provider_id), seconds(record.audio_ms)];
  if (ok) meta.push(`${number.format(wordCount(record.text))} words`);
  const body = el("div");
  body.append(text, el("div", "mt", meta.join(" · ")));

  const actions = el("div", "act");
  if (retryable) {
    const retry = el("button", "small danger", "Retry");
    retry.type = "button";
    retry.setAttribute("aria-label", "Retry this dictation");
    wireRetry(retry);
    actions.append(retry);
  }
  if (ok && record.text.trim() !== "") actions.append(copyButton(record.text));
  item.append(when, body, actions);
  return item;
}

function render(): void {
  // Keep receiving data while minimized or on a settings page, but do no list
  // DOM work until it is visible. Coalesce bursts/search input into one frame.
  if (document.hidden || renderFrame !== undefined) return;
  renderFrame = requestAnimationFrame(() => {
    renderFrame = undefined;
    if (document.hidden) return;
    historyCount.textContent = !loaded ? "" : records.length >= limit ? `${records.length}+` : String(records.length);
    // Only the newest failed dictation can be retried (one clip is kept).
    const retryable = canRetry ? records.find((r) => r.error !== null) : undefined;
    if (page === "home") renderHome(retryable);
    else if (page === "history") renderHistory(retryable);
    else if (page === "timings") renderTimings(records);
  });
}

function renderHome(retryable: DictationRecord | undefined): void {
  const newest = records[0];
  const failed = newest !== undefined && newest.error !== null && newest.id !== dismissed;
  failure.hidden = !failed;
  if (failed) {
    failureText.textContent = `Last dictation failed: ${newest.error}`;
    failureRetry.hidden = !canRetry;
    if (newest.id !== bannerFor) {
      bannerFor = newest.id;
      failureRetry.disabled = false;
      failureRetry.textContent = "Retry";
    }
  }
  const today = startOfDay(Date.now());
  recent.replaceChildren(...records.slice(0, 5).map((record) => row(record, record === retryable, today)));
  recent.hidden = records.length === 0;
  recentEmpty.hidden = !loaded || records.length > 0;
}

function renderHistory(retryable: DictationRecord | undefined): void {
  const query = search.value.trim().toLowerCase();
  const shown = records.filter((r) =>
    (!failedOnly || r.error !== null)
    && (query === "" || `${r.text}\n${r.error ?? ""}`.toLowerCase().includes(query)));
  const retained = new Set(records.map((record) => record.id));
  for (const id of entries.keys()) if (!retained.has(id)) entries.delete(id);

  // Day groups and rows are cached; only what changed moves in the DOM.
  const today = startOfDay(Date.now());
  const groups: { head: HTMLHeadingElement; list: HTMLUListElement; rows: HTMLLIElement[] }[] = [];
  const used = new Set<string>();
  let day: number | undefined;
  for (const record of shown) {
    const start = startOfDay(record.started_at);
    if (start !== day) {
      day = start;
      const label = dayLabel(start, today);
      // The label is part of the key so "Today" becomes "Yesterday" at midnight.
      const key = `${start} ${label}`;
      let group = dayGroups.get(key);
      if (group === undefined) {
        group = { head: el("h2", "day", label), list: el("ul", "rows") };
        dayGroups.set(key, group);
      }
      used.add(key);
      groups.push({ ...group, rows: [] });
    }
    const isRetryable = record === retryable;
    let cached = entries.get(record.id);
    if (!cached || cached.record !== record || cached.retryable !== isRetryable) {
      cached = { record, retryable: isRetryable, element: row(record, isRetryable) };
      entries.set(record.id, cached);
    }
    groups[groups.length - 1].rows.push(cached.element);
  }
  for (const key of dayGroups.keys()) if (!used.has(key)) dayGroups.delete(key);
  for (const group of groups) reconcile(group.list, group.rows);
  reconcile(days, groups.flatMap((group) => [group.head, group.list]));

  const count = records.length;
  lede.textContent = count >= limit
    ? `The newest ${number.format(count)} dictations kept on this PC.`
    : `${number.format(count)} ${count === 1 ? "dictation" : "dictations"} kept on this PC.`;
  empty.hidden = shown.length > 0;
  empty.textContent = count === 0
    ? "No dictations yet."
    : query === "" && failedOnly ? "No failed dictations." : "No transcripts match.";
  more.hidden = count < limit || limit >= MAX;
  clear.disabled = count === 0;
}

async function refreshRetry(): Promise<void> {
  canRetry = await invoke<boolean>("has_failed_clip");
  render();
}

async function load(): Promise<void> {
  const sequence = ++loadSequence;
  const before = new Set(records.map((record) => record.id));
  const [fetched, retry] = await Promise.all([
    invoke<DictationRecord[]>("get_history", { limit }),
    invoke<boolean>("has_failed_clip"),
  ]);
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

function show(next: Page): void {
  page = next;
  for (const [id, element] of pages) element.hidden = id !== next;
  form.hidden = RECORD_PAGES.has(next);
  for (const button of navButtons) {
    const on = button.dataset.page === next;
    button.classList.toggle("on", on);
    if (on) button.setAttribute("aria-current", "page");
    else button.removeAttribute("aria-current");
  }
  history.replaceState(null, "", `#${next}`);
  main.scrollTo(0, 0);
  // Live events keep the list current; a reopened window loads afresh.
  if (RECORD_PAGES.has(next) && loaded) render();
}

for (const button of document.querySelectorAll<HTMLButtonElement>("[data-page]")) {
  button.addEventListener("click", () => show(toPage(button.dataset.page ?? "")));
}
for (const button of document.querySelectorAll<HTMLButtonElement>("[data-open]")) {
  button.addEventListener("click", () => {
    void invoke("open_window", { name: button.dataset.open });
  });
}

search.addEventListener("input", render);
for (const chip of chips) {
  chip.addEventListener("click", () => {
    failedOnly = chip.dataset.filter === "failed";
    for (const other of chips) other.setAttribute("aria-pressed", String(other === chip));
    render();
  });
}
more.addEventListener("click", () => {
  limit = Math.min(MAX, limit + PAGE);
  void load();
});

clear.addEventListener("click", () => {
  confirmBox.hidden = false;
  clearStatus.textContent = "";
});
must<HTMLButtonElement>("cancel-clear").addEventListener("click", () => {
  confirmBox.hidden = true;
});
must<HTMLButtonElement>("confirm-clear").addEventListener("click", () => {
  invoke("clear_history").then(
    () => {
      loadSequence += 1; // An in-flight read predates the clear.
      records = [];
      clearStatus.textContent = "History cleared.";
      render();
    },
    (error: unknown) => {
      clearStatus.textContent = `Could not clear history: ${String(error)}`;
    },
  ).finally(() => {
    confirmBox.hidden = true;
  });
});

wireRetry(failureRetry);
must<HTMLButtonElement>("failure-dismiss").addEventListener("click", () => {
  dismissed = records[0]?.id ?? null;
  if (dismissed !== null) localStorage.setItem(DISMISSED_KEY, dismissed);
  failure.hidden = true;
});

testMic.addEventListener("click", () => {
  testMic.disabled = true;
  testMic.textContent = "Recording 3 s…";
  // The command returns at once; the clip is saved and revealed after 3 s.
  const done = window.setTimeout(() => {
    testMic.disabled = false;
    testMic.textContent = "Test mic";
  }, 3500);
  invoke("test_mic").catch(() => {
    window.clearTimeout(done);
    testMic.disabled = false;
    testMic.textContent = "Test mic failed";
  });
});

onApplied((applied) => {
  readyText.textContent = `${applied.ready ? "Ready" : "Not ready"} · ${applied.engine}`;
  readyLine.classList.toggle("not-ready", !applied.ready);
  readyLine.title = applied.ready ? "" : (applied.reason ?? "");
  if (applied.hotkey !== "") showShortcut(applied.hotkey);
});

document.addEventListener("visibilitychange", () => render());
void getVersion().then((version) => {
  must<HTMLSpanElement>("version").textContent = `v${version}`;
}, () => undefined);

const listeners = [listen<string>("navigate", (event) => show(toPage(event.payload))),
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
void Promise.all(listeners).then(() => {
  show(toPage(location.hash.slice(1)));
  // Loaded on every page, settings included, so the rail count is filled in.
  void load();
});
