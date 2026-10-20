import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./theme.css";
import "./app.css";
import { clock, combo, copyButton, el, fullDate, must, parseShortcut, providerName, reconcile, seconds, type DictationRecord } from "./records";
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
  unpriced_llm_tokens: number;
}

const PAGE = 200;
const MAX = 2000;

const PAGES = ["home", "history", "timings", "transcription", "dictation", "audio", "vocabulary", "clipboard", "updates"] as const;
type Page = (typeof PAGES)[number];
/** Pages that show history rows; the others are settings pages. */
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
  const unpriced: string[] = [];
  if (stats.unpriced_audio_ms > 0) unpriced.push(formatDuration(stats.unpriced_audio_ms));
  if (stats.unpriced_llm_tokens > 0) unpriced.push(`${number.format(stats.unpriced_llm_tokens)} LLM tokens`);
  costNote.hidden = unpriced.length === 0;
  costNote.textContent = `excludes ${unpriced.join(" and ")} with no known price`;
  if (stats.first_used_at === null) {
    projected.textContent = `vs $${WISPR_FLOW_PER_YEAR}/yr for Wispr Flow`;
    return;
  }
  const used = (Date.now() - stats.first_used_at) / DAY_MS;
  projected.textContent = used < PROJECTION_DAYS
    ? `yearly estimate from ${new Date(stats.first_used_at + PROJECTION_DAYS * DAY_MS).toLocaleDateString()}`
    : `≈ ${formatCost((stats.cost_usd / used) * 365)}/yr vs $${WISPR_FLOW_PER_YEAR} Wispr Flow`;
}

function showShortcut(hotkey: string): void {
  const { keys, modifiers, hold } = parseShortcut(hotkey);
  const rows: [string, ...(string | Node[])[]][] = hold
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

const isCommand = (record: DictationRecord): boolean => record.kind === "command";
const isDictation = (record: DictationRecord): boolean => record.kind === "dictation";

/** What a command run did. A missing or unreadable `url` still reads well. */
function commandOutcome(record: DictationRecord): string {
  switch (record.outcome) {
    case "pasted":
      return "Reply pasted";
    case "shown":
      return record.reply_kind === "answer" ? "Answer shown in the Card" : "Edit shown in the Card";
    case "opened": {
      try {
        return `Opened ${new URL(record.url ?? "").host}`;
      } catch {
        return "Opened a page";
      }
    }
    case "created":
      return "Reminder set";
    case "cancelled":
      return "Cancelled";
    default:
      return record.error === null ? "Failed" : `Failed: ${record.error}`;
  }
}

/** The text, meta line and Copy text of a row. A command row shows its
 *  instruction under an orange tag; Copy copies the reply whenever there is
 *  one, even if pasting it failed. */
function rowParts(record: DictationRecord): { text: string; meta: HTMLDivElement; copy: string | undefined } {
  const ok = record.error === null;
  const meta = el("div", "mt");
  if (!isCommand(record)) {
    const parts = [providerName(record.provider_id), seconds(record.audio_ms)];
    if (ok) parts.push(`${number.format(wordCount(record.text))} words`);
    // Caught by the Safety Net: nothing was pasted (PARLA-80).
    if (record.outcome === "shown") parts.push("Not pasted, shown in the Card");
    meta.textContent = parts.join(" · ");
    return { text: ok ? record.text : `Failed: ${record.error}`, meta, copy: ok ? record.text : undefined };
  }
  const rest = [record.llm_model, seconds(record.audio_ms)].filter((part) => part !== undefined);
  meta.append(
    el("span", "tag", "Command"),
    el("span", ok ? undefined : "bad", commandOutcome(record)),
    ` · ${rest.join(" · ")}`,
  );
  return { text: record.text.trim() || "No instruction", meta, copy: record.reply };
}

/** `today` is set on Home, where rows are not grouped by day: older rows
 *  then show their date instead of their time. */
function row(record: DictationRecord, retryable: boolean, today?: number): HTMLLIElement {
  const item = el("li", record.error === null ? undefined : "failed");
  const when = el("span", "tm", today !== undefined && startOfDay(record.started_at) !== today
    ? shortDayFormat.format(record.started_at)
    : clock(record.started_at));
  when.title = fullDate(record.started_at);

  const { text: shown, meta, copy } = rowParts(record);
  const text = el("p", "tx", shown);
  // Click expands the two-line clamp, unless the click ended a text selection.
  text.addEventListener("click", () => {
    if (window.getSelection()?.isCollapsed !== false) text.classList.toggle("expanded");
  });
  const body = el("div");
  body.append(text, meta);

  const actions = el("div", "act");
  if (retryable) {
    const retry = el("button", "small danger", "Retry");
    retry.type = "button";
    retry.setAttribute("aria-label", "Retry this dictation");
    wireRetry(retry);
    actions.append(retry);
  }
  if (copy !== undefined && copy.trim() !== "") {
    actions.append(copyButton(copy, isCommand(record) ? "Copy reply" : undefined));
  }
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
    const retryable = canRetry ? records.find((r) => isDictation(r) && r.error !== null) : undefined;
    if (page === "home") renderHome(retryable);
    else if (page === "history") renderHistory(retryable);
    else if (page === "timings") renderTimings(records.filter(isDictation));
  });
}

function renderHome(retryable: DictationRecord | undefined): void {
  // The banner is about dictation: a failed command run already notified.
  const newest = records.find(isDictation);
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
  const commands = records.filter(isCommand).length;
  const kept = [plural(count - commands, "dictation")];
  if (commands > 0) kept.push(plural(commands, "command run"));
  const what = kept.join(" and ");
  lede.textContent = count >= limit ? `The newest ${what} kept on this PC.` : `${what} kept on this PC.`;
  empty.hidden = shown.length > 0;
  empty.textContent = count === 0
    ? "No dictations yet."
    : query === "" && failedOnly ? "Nothing failed." : "Nothing matches.";
  more.hidden = count < limit || limit >= MAX;
  clear.disabled = count === 0;
}

function plural(count: number, noun: string): string {
  return `${number.format(count)} ${noun}${count === 1 ? "" : "s"}`;
}

async function refreshRetry(): Promise<void> {
  canRetry = await invoke<boolean>("has_failed_clip");
  render();
}

async function load(): Promise<void> {
  const sequence = ++loadSequence;
  const before = new Set(records.map((record) => record.id));
  const [fetched, retry] = await Promise.all([
    invoke<DictationRecord[]>("get_history", { limit, commands: true }),
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
  if (!isDictation(event.payload) && !isCommand(event.payload)) return;
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
