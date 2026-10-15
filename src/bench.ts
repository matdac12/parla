import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./theme.css";
import "./bench.css";

interface BenchResult {
  provider_id: string;
  display_name: string;
  request_ms: number | null;
  total_ms: number | null;
  text: string | null;
  error: string | null;
}

interface BenchRun {
  id: string;
  started_at: number;
  audio_ms: number;
  results: BenchResult[];
}

interface Progress {
  run_id: string;
  provider_id: string;
  display_name: string;
  state: string;
  result: BenchResult | null;
}

interface ClipInfo {
  audio_ms: number;
}

interface AutoStopped {
  audio_ms: number | null;
  error: string | null;
}

function must(id: string): HTMLElement {
  const element = document.getElementById(id);
  if (element === null) throw new Error(`missing #${id}`);
  return element;
}

const recordButton = must("record") as HTMLButtonElement;
const runButton = must("run") as HTMLButtonElement;
const clipLine = must("clip");
const status = must("status");
const table = must("results");
const body = must("rows");
const runsTable = must("runs");
const runsBody = must("runs-body");
const runsEmpty = must("runs-empty");

let recording = false;
let running = false;
let hasClip = false;
/// Current table rows by provider id, and their latest known result.
const rows = new Map<string, HTMLTableRowElement>();
const results = new Map<string, BenchResult>();

function setStatus(message: string): void {
  status.textContent = message;
  status.hidden = message === "";
}

function seconds(ms: number): string {
  return `${(ms / 1000).toFixed(1)} s`;
}

function refreshControls(): void {
  recordButton.textContent = recording ? "Stop recording" : "Record clip";
  recordButton.classList.toggle("recording", recording);
  recordButton.disabled = running;
  runButton.disabled = recording || running || !hasClip;
  runButton.textContent = running ? "Running…" : hasClip ? "Run again" : "Run benchmark";
}

function setClip(audioMs: number): void {
  hasClip = true;
  clipLine.textContent = `Clip ready: ${seconds(audioMs)}. “Run” re-runs this same audio; record again to replace it.`;
  refreshControls();
}

function textCell(value: string, className?: string): HTMLTableCellElement {
  const cell = document.createElement("td");
  // textContent (never innerHTML) so a transcript or error cannot inject markup.
  cell.textContent = value;
  if (className !== undefined) cell.className = className;
  return cell;
}

function msCell(value: number | null): HTMLTableCellElement {
  return textCell(value === null ? "—" : `${value} ms`, "numeric");
}

function ensureRow(id: string): HTMLTableRowElement {
  let row = rows.get(id);
  if (row === undefined) {
    row = document.createElement("tr");
    rows.set(id, row);
    body.append(row);
  }
  return row;
}

function renderRow(id: string, name: string, state: string, result: BenchResult | null): void {
  const row = ensureRow(id);
  row.className = state === "error" || state === "skipped" ? `${state} failed` : state;
  row.replaceChildren(
    textCell(name),
    msCell(result?.request_ms ?? null),
    msCell(result?.total_ms ?? null),
    textCell(result?.text ?? (state === "running" ? "Transcribing…" : state === "warming" ? "Loading model…" : "—")),
    textCell(result?.error ?? "", "error-text"),
  );
}

function highlightFastest(): void {
  let best: string | null = null;
  let bestMs = Infinity;
  for (const [id, result] of results) {
    if (result.error === null && result.request_ms !== null && result.request_ms < bestMs) {
      best = id;
      bestMs = result.request_ms;
    }
  }
  for (const [id, row] of rows) row.classList.toggle("fastest", id === best);
}

function showRun(run: BenchRun): void {
  rows.clear();
  results.clear();
  body.replaceChildren();
  for (const result of run.results) {
    results.set(result.provider_id, result);
    renderRow(
      result.provider_id,
      result.display_name,
      result.error === null ? "done" : result.request_ms === null ? "skipped" : "error",
      result,
    );
  }
  table.hidden = false;
  highlightFastest();
}

function renderRuns(runs: BenchRun[]): void {
  runsEmpty.hidden = runs.length > 0;
  runsTable.hidden = runs.length === 0;
  runsBody.replaceChildren();
  for (const run of runs) {
    const row = document.createElement("tr");
    const summary = run.results
      .map((r) => `${r.display_name}: ${r.error === null && r.request_ms !== null ? `${r.request_ms} ms` : "failed"}`)
      .join("   ·   ");
    row.append(textCell(new Date(run.started_at).toLocaleString()), textCell(seconds(run.audio_ms)), textCell(summary));
    runsBody.append(row);
  }
}

async function loadRuns(): Promise<void> {
  try {
    renderRuns(await invoke<BenchRun[]>("bench_runs", { limit: 20 }));
  } catch (error) {
    setStatus(`Could not load previous runs: ${String(error)}`);
  }
}

recordButton.addEventListener("click", () => {
  void (async () => {
    setStatus("");
    try {
      if (recording) {
        const info = await invoke<ClipInfo>("bench_stop_recording");
        recording = false;
        setClip(info.audio_ms);
      } else {
        await invoke("bench_start_recording");
        recording = true;
        clipLine.textContent = "Recording… press Stop (max 30 s).";
      }
    } catch (error) {
      recording = false;
      setStatus(String(error));
    }
    refreshControls();
  })();
});

runButton.addEventListener("click", () => {
  void (async () => {
    setStatus("");
    running = true;
    refreshControls();
    try {
      showRun(await invoke<BenchRun>("bench_run"));
      await loadRuns();
    } catch (error) {
      setStatus(String(error));
    } finally {
      running = false;
      refreshControls();
    }
  })();
});

let currentRun: string | null = null;

void listen<Progress>("bench://progress", (event) => {
  const p = event.payload;
  if (p.run_id !== currentRun) {
    // First event of a new run: start a fresh table.
    currentRun = p.run_id;
    rows.clear();
    results.clear();
    body.replaceChildren();
  }
  table.hidden = false;
  renderRow(p.provider_id, p.display_name, p.state, p.result);
  if (p.result !== null) results.set(p.provider_id, p.result);
  highlightFastest();
});

void listen<AutoStopped>("bench://recording-stopped", (event) => {
  recording = false;
  if (event.payload.audio_ms !== null) setClip(event.payload.audio_ms);
  else setStatus(event.payload.error ?? "Recording stopped.");
  refreshControls();
});

void (async () => {
  try {
    const clip = await invoke<ClipInfo | null>("bench_clip");
    if (clip !== null) setClip(clip.audio_ms);
  } catch (error) {
    setStatus(String(error));
  }
  refreshControls();
  await loadRuns();
})();
