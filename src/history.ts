import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./history.css";

interface DictationRecord {
  id: string;
  kind: string;
  started_at: number;
  provider_id: string;
  key_up: number;
  audio_ready: number | null;
  provider_returned: number | null;
  pasted: number | null;
  audio_ms: number;
  text: string;
  error: string | null;
}

/// Keep the DOM bounded while the window stays open and records stream in.
const MAX_ROWS = 500;

function must(id: string): HTMLElement {
  const element = document.getElementById(id);
  if (element === null) throw new Error(`missing #${id}`);
  return element;
}

const table = must("history");
const body = must("rows");
const empty = must("empty");
const status = must("status");
const confirm = must("confirm");

/// Stage latencies mirror `timing::DictationRecord::stage_ms` on the Rust side.
/// A stage is null unless both of its endpoints are present, so a partial or
/// failed dictation never shows a bogus zero.
function stageMs(record: DictationRecord): {
  audioReady: number | null;
  transcribe: number | null;
  paste: number | null;
  total: number | null;
} {
  const between = (from: number | null, to: number | null): number | null =>
    from === null || to === null ? null : Math.max(0, to - from);
  return {
    audioReady: record.audio_ready,
    transcribe: between(record.audio_ready, record.provider_returned),
    paste: between(record.provider_returned, record.pasted),
    total: record.pasted,
  };
}

function textCell(value: string, className?: string): HTMLTableCellElement {
  const cell = document.createElement("td");
  cell.textContent = value;
  if (className !== undefined) cell.className = className;
  return cell;
}

function msCell(value: number | null): HTMLTableCellElement {
  return textCell(value === null ? "—" : `${value} ms`, "numeric");
}

function transcriptCell(text: string): HTMLTableCellElement {
  const cell = document.createElement("td");
  cell.className = "transcript";
  const clip = document.createElement("div");
  clip.className = "clip";
  // textContent (never innerHTML) so a transcript cannot inject markup.
  clip.textContent = text.length > 0 ? text : "—";
  clip.title = text;
  clip.addEventListener("click", () => clip.classList.toggle("expanded"));
  cell.append(clip);
  return cell;
}

function errorCell(error: string | null): HTMLTableCellElement {
  const cell = document.createElement("td");
  const clip = document.createElement("div");
  clip.className = "clip error-text";
  clip.textContent = error ?? "—";
  if (error !== null) clip.title = error;
  cell.append(clip);
  return cell;
}

function formatTime(startedAtMs: number): string {
  return new Date(startedAtMs).toLocaleString();
}

function row(record: DictationRecord): HTMLTableRowElement {
  const stages = stageMs(record);
  const row = document.createElement("tr");
  row.append(
    textCell(formatTime(record.started_at)),
    textCell(record.provider_id),
    textCell(record.audio_ms > 0 ? `${(record.audio_ms / 1000).toFixed(1)} s` : "—", "numeric"),
    msCell(stages.audioReady),
    msCell(stages.transcribe),
    msCell(stages.paste),
    msCell(stages.total),
    transcriptCell(record.text),
    errorCell(record.error),
  );
  if (record.error !== null) row.classList.add("failed");
  return row;
}

function trimRows(): void {
  while (body.childElementCount > MAX_ROWS) body.lastElementChild?.remove();
}

function showRecords(records: DictationRecord[]): void {
  body.replaceChildren(...records.map(row));
  table.hidden = records.length === 0;
  empty.hidden = records.length !== 0;
}

function addRecord(record: DictationRecord): void {
  body.prepend(row(record));
  trimRows();
  table.hidden = false;
  empty.hidden = true;
}

function setStatus(message: string | null): void {
  status.hidden = message === null;
  status.textContent = message ?? "";
}

async function load(): Promise<void> {
  setStatus(null);
  try {
    const records = await invoke<DictationRecord[]>("get_history", { limit: 200 });
    showRecords(records);
  } catch (error) {
    setStatus(`Could not load history: ${String(error)}`);
  }
}

function bindControls(): void {
  must("refresh").addEventListener("click", () => void load());
  must("clear").addEventListener("click", () => {
    confirm.hidden = false;
  });
  must("cancel-clear").addEventListener("click", () => {
    confirm.hidden = true;
  });
  must("confirm-clear").addEventListener("click", () => {
    void (async () => {
      try {
        await invoke("clear_history");
        showRecords([]);
        setStatus("History cleared.");
      } catch (error) {
        setStatus(`Could not clear history: ${String(error)}`);
      } finally {
        confirm.hidden = true;
      }
    })();
  });
}

bindControls();
void load();
void listen<DictationRecord>("dictation://recorded", (event) => {
  addRecord(event.payload);
});
