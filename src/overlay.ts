import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

const BARS = 9;
const pill = document.getElementById("pill") as HTMLDivElement;

// One small clue for the mode: a dot while the keys are held, a padlock once
// the recording is locked hands-free.
const mode = document.createElement("div");
mode.className = "mode";
const LOCK = `<svg viewBox="0 0 12 12" width="12" height="12" aria-hidden="true">
  <rect x="2" y="5.25" width="8" height="5.5" rx="1.6" fill="currentColor"/>
  <path d="M3.9 5.25V3.9a2.1 2.1 0 0 1 4.2 0v1.35" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>
</svg>`;
function setLocked(locked: boolean): void {
  pill.dataset.locked = String(locked);
  mode.innerHTML = locked ? LOCK : '<span class="dot"></span>';
  mode.title = locked ? "Locked: press Ctrl+Alt to stop, Esc to cancel" : "Holding";
}
setLocked(false);

const container = document.createElement("div");
container.className = "bars";

// After recording: a spinner while transcribing, then a check or cross.
const spinner = document.createElement("div");
spinner.className = "spinner";
const result = document.createElement("div");
result.className = "result";
const CHECK = `<svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
  <path class="stroke" d="M3.6 8.4l3 3 5.8-6.4" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>
</svg>`;
const CROSS = `<svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
  <path class="stroke" d="M4.8 4.8l6.4 6.4M11.2 4.8l-6.4 6.4" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>
</svg>`;

// After a failure: a reload button that retries the saved clip.
const retry = document.createElement("button");
retry.type = "button";
retry.className = "retry";
retry.title = "Retry transcription";
retry.setAttribute("aria-label", "Retry transcription");
retry.innerHTML = `<svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
  <path d="M13 8a5 5 0 1 1-1.6-3.7" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/>
  <path d="M12.6 1.6v3.3H9.3" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>
</svg>`;
retry.addEventListener("click", () => {
  retry.disabled = true;
  void invoke("retry_last_failed").catch(() => undefined);
});

pill.append(mode, container, spinner, result, retry);

const bars: HTMLDivElement[] = [];
for (let i = 0; i < BARS; i++) {
  const bar = document.createElement("div");
  bar.className = "bar";
  container.appendChild(bar);
  bars.push(bar);
}

// Scrolling history of recent levels; speech RMS is small, so boost it.
const history = new Array<number>(BARS).fill(0);
function render(): void {
  history.forEach((value, i) => {
    bars[i].style.height = `${3 + value * 16}px`;
    bars[i].style.opacity = String(0.35 + 0.65 * Math.min(1, value * 1.6));
  });
}
render();

function setPhase(phase: string): void {
  if (phase === "recording") {
    history.fill(0);
    render();
    result.innerHTML = "";
  } else if (phase === "done") {
    result.innerHTML = CHECK;
  } else if (phase === "error") {
    result.innerHTML = CROSS;
  }
  retry.disabled = false;
  pill.dataset.phase = phase;
}
setPhase("recording");

void listen<number>("level", (event) => {
  history.shift();
  history.push(Math.min(1, Math.sqrt(event.payload) * 2.2));
  render();
});
void listen<boolean>("mode", (event) => setLocked(event.payload));
void listen<string>("phase", (event) => setPhase(event.payload));
