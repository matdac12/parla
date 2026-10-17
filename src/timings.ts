// Timings page: the latency log as tiles, one stacked bar per
// dictation and the old table behind a disclosure. Rendered only while visible.
import { clock, el, fullDate, must, providerName, reconcile, seconds, type DictationRecord } from "./records";

interface Stages {
  ready: number | null;
  transcribe: number | null;
  paste: number | null;
  total: number | null;
}

/** Mirrors `timing::DictationRecord::stage_ms`: a stage is null unless both
 *  of its endpoints are present, so a partial dictation never shows a zero. */
function stageMs(record: DictationRecord): Stages {
  const between = (from: number | null, to: number | null): number | null =>
    from === null || to === null ? null : Math.max(0, to - from);
  return {
    ready: record.audio_ready,
    transcribe: between(record.audio_ready, record.provider_returned),
    paste: between(record.provider_returned, record.pasted),
    total: record.pasted,
  };
}

const STAGES = [
  ["ready", "Ready"],
  ["transcribe", "Transcribe"],
  ["paste", "Paste"],
] as const;

/** Bar length: the stages that completed, so unpasted rows still show. */
function barMs(stages: Stages): number {
  return STAGES.reduce((sum, [key]) => sum + (stages[key] ?? 0), 0);
}

const median = must<HTMLSpanElement>("t-median");
const fastest = must<HTMLSpanElement>("t-fastest");
const slowest = must<HTMLSpanElement>("t-slowest");
const chart = must<HTMLDivElement>("tchart");
const empty = must<HTMLParagraphElement>("timings-empty");
const tableView = must<HTMLDetailsElement>("table-view");
const tableRows = must<HTMLTableSectionElement>("timing-rows");

// Rows are rebuilt only when their record (or, for bars, the scale) changes.
const chartCache = new WeakMap<DictationRecord, { scale: number; row: HTMLDivElement }>();
const tableCache = new WeakMap<DictationRecord, HTMLTableRowElement>();
let tableRecords: DictationRecord[] | null = null;

function showMs(target: HTMLElement, ms: number | undefined): void {
  if (ms === undefined) {
    target.textContent = "—";
    return;
  }
  target.replaceChildren(String(ms), el("small", undefined, "ms"));
}

function track(stages: Stages, scale: number): HTMLDivElement {
  const bar = el("div", "track");
  bar.style.width = `${Math.max(4, (barMs(stages) / scale) * 100)}%`;
  for (const [key, name] of STAGES) {
    const ms = stages[key];
    if (ms === null || ms === 0) continue;
    const segment = el("div", `seg s-${key}`);
    segment.style.flexGrow = String(ms);
    segment.title = `${name}: ${ms} ms`;
    bar.append(segment);
  }
  return bar;
}

function chartRow(record: DictationRecord, stages: Stages, scale: number): HTMLDivElement {
  const row = el("div", "trow");
  const who = el("span", "who", `${clock(record.started_at)} · ${providerName(record.provider_id)}`);
  who.title = fullDate(record.started_at);
  const body = record.error === null ? track(stages, scale) : el("span", "fail", `Failed · ${record.error}`);
  if (record.error !== null) body.title = record.error;
  row.append(who, body, el("span", "tot", stages.total === null ? "—" : `${stages.total} ms`));
  return row;
}

function clipCell(text: string, className = "clip"): HTMLTableCellElement {
  const td = el("td");
  const clip = el("div", className, text === "" ? "—" : text);
  clip.title = text;
  clip.addEventListener("click", () => clip.classList.toggle("expanded"));
  td.append(clip);
  return td;
}

function tableRow(record: DictationRecord): HTMLTableRowElement {
  const stages = stageMs(record);
  const ms = (value: number | null): HTMLTableCellElement => el("td", "numeric", value === null ? "—" : `${value} ms`);
  const row = el("tr", record.error === null ? undefined : "failed");
  row.append(
    el("td", undefined, fullDate(record.started_at)),
    el("td", undefined, providerName(record.provider_id)),
    el("td", "numeric", record.audio_ms > 0 ? seconds(record.audio_ms) : "—"),
    ms(stages.ready),
    ms(stages.transcribe),
    ms(stages.paste),
    ms(stages.total),
    clipCell(record.text),
    clipCell(record.error ?? "", "clip error-text"),
  );
  return row;
}

function renderTable(): void {
  if (!tableView.open || tableRecords === null) return;
  reconcile(tableRows, tableRecords.map((record) => {
    let row = tableCache.get(record);
    if (row === undefined) {
      row = tableRow(record);
      tableCache.set(record, row);
    }
    return row;
  }));
  tableRecords = null;
}

tableView.addEventListener("toggle", renderTable);

export function renderTimings(records: DictationRecord[]): void {
  const rows = records.map((record) => ({ record, stages: stageMs(record) }));
  const totals = rows
    .filter(({ record, stages }) => record.error === null && stages.total !== null)
    .map(({ stages }) => stages.total as number)
    .sort((a, b) => a - b);
  const middle = Math.floor(totals.length / 2);
  showMs(median, totals.length === 0 ? undefined : totals.length % 2 === 1
    ? totals[middle]
    : Math.round((totals[middle - 1] + totals[middle]) / 2));
  showMs(fastest, totals[0]);
  showMs(slowest, totals[totals.length - 1]);

  // Scale bars to the longest one, partial (unpasted) rows included.
  const scale = Math.max(1, ...rows.map(({ stages }) => barMs(stages)));
  reconcile(chart, rows.map(({ record, stages }) => {
    let cached = chartCache.get(record);
    if (cached === undefined || cached.scale !== scale) {
      cached = { scale, row: chartRow(record, stages, scale) };
      chartCache.set(record, cached);
    }
    return cached.row;
  }));
  chart.hidden = records.length === 0;
  empty.hidden = records.length !== 0;
  tableView.hidden = records.length === 0;
  // The table is the heaviest DOM here: build it only while it is open.
  tableRecords = records;
  renderTable();
}
