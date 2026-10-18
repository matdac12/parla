import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

const BARS = 9;
const pill = document.getElementById("pill") as HTMLDivElement;
const captions = document.getElementById("captions") as HTMLDivElement;
const captionText = captions.querySelector(".caption-text") as HTMLDivElement;
const stable = captions.querySelector(".caption-stable") as HTMLSpanElement;
const provisional = captions.querySelector(".caption-provisional") as HTMLSpanElement;
let generation = 0;
let captionsEnabled = false;
let captionTimer: number | undefined;
let pendingCaption: { stable: string; provisional: string } | null = null;
let phase = "recording";

function clearCaptions(): void {
  window.clearTimeout(captionTimer);
  captionTimer = undefined;
  pendingCaption = null;
  stable.textContent = "";
  provisional.textContent = "";
  captions.hidden = true;
}

interface CaptionUpdate { generation: number; stable: string; provisional: string }
function queueCaption(update: CaptionUpdate): void {
  if (update.generation !== generation || !captionsEnabled || !["recording", "processing"].includes(phase)) return;
  if (update.stable === "" && update.provisional === "") { clearCaptions(); return; }
  // Backend payloads are already bounded; retain a defensive Unicode-safe cap.
  const soft = Array.from(update.provisional).slice(-320).join("");
  const remaining = 320 - Array.from(soft).length;
  const settled = remaining > 0 ? Array.from(update.stable).slice(-remaining).join("") : "";
  pendingCaption = { stable: settled, provisional: soft };
  if (captionTimer !== undefined) return;
  captionTimer = window.setTimeout(() => {
    captionTimer = undefined;
    if (!pendingCaption) return;
    stable.textContent = pendingCaption.stable;
    provisional.textContent = pendingCaption.provisional;
    pendingCaption = null;
    captions.hidden = false;
    // The preview shows the newest two lines, not the start of a long segment.
    captionText.scrollTop = captionText.scrollHeight;
  }, 80);
}

// One small clue for the mode: a dot while the keys are held, a padlock once
// the recording is locked hands-free. Command mode turns the pill orange.
type Mode = "hold" | "locked" | "command";
const mode = document.createElement("div");
mode.className = "mode";
const LOCK = `<svg viewBox="0 0 12 12" width="12" height="12" aria-hidden="true">
  <rect x="2" y="5.25" width="8" height="5.5" rx="1.6" fill="currentColor"/>
  <path d="M3.9 5.25V3.9a2.1 2.1 0 0 1 4.2 0v1.35" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>
</svg>`;
const MODE_TITLES: Record<Mode, string> = {
  hold: "Holding",
  locked: "Locked: press Ctrl+Alt to stop, Esc to cancel",
  command: "Command: release when done, Esc to cancel",
};
function setMode(next: Mode): void {
  pill.dataset.mode = next;
  mode.innerHTML = next === "locked" ? LOCK : '<span class="dot"></span>';
  mode.title = MODE_TITLES[next];
}
setMode("hold");

const container = document.createElement("div");
container.className = "bars";

// After recording: a spinner while transcribing, then a check or cross. The
// cross is a button: it dismisses the failed pill.
const spinner = document.createElement("div");
spinner.className = "spinner";
const result = document.createElement("button");
result.type = "button";
result.className = "result";
result.addEventListener("click", () => {
  if (phase === "error") void invoke("dismiss_failure_pill").catch(() => undefined);
});
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

// Beside the check when there is something to say: "Opened <host>" after
// command mode opened a page, "Reminder set" after it created a To-do.
const label = document.createElement("span");
label.className = "label";

pill.append(mode, container, spinner, result, label, retry);

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

function setPhase(nextPhase: string): void {
  phase = nextPhase;
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
  const failed = phase === "error";
  result.disabled = !failed;
  result.title = failed ? "Dismiss" : "";
  if (failed) result.setAttribute("aria-label", "Dismiss");
  else result.removeAttribute("aria-label");
  // The label arrives just before "done" and lasts until the next phase.
  if (phase !== "done") {
    label.textContent = "";
    delete pill.dataset.labelled;
  }
  pill.dataset.phase = phase;
  if (!["recording", "processing"].includes(phase)) clearCaptions();
}
setPhase("recording");

void listen<number>("level", (event) => {
  history.shift();
  history.push(Math.min(1, Math.sqrt(event.payload) * 2.2));
  render();
});
void listen<Mode>("mode", (event) => setMode(event.payload));
void listen<string>("label", (event) => {
  label.textContent = event.payload;
  pill.dataset.labelled = "";
});
// Before an "error" phase: whether its cross comes with Retry (PARLA-51).
void listen<boolean>("retry", (event) => pill.toggleAttribute("data-no-retry", !event.payload));
void listen<string>("phase", (event) => setPhase(event.payload));
void listen<{ generation: number; enabled: boolean }>("caption-session", (event) => {
  if (event.payload.generation < generation) return;
  generation = event.payload.generation;
  captionsEnabled = event.payload.enabled;
  clearCaptions();
});
void listen<CaptionUpdate>("caption", (event) => queueCaption(event.payload));

// The Card (PARLA-80): text Parla did not paste, above the check, until a
// button closes it or the next session starts. Rust says what it shows; the
// window takes clicks only over the box reported here.
/** The footer's buttons; Dismiss is always the ✕. */
type CardAction = "copy" | "paste" | "replace";
interface CardView {
  tag: "not_pasted" | "command";
  header: string | null;
  text: string;
  actions: CardAction[];
  primary: CardAction;
}
const TAGS: Record<CardView["tag"], string> = { not_pasted: "Not pasted · no text field", command: "Command" };
const ACTIONS: Record<CardAction, string> = { copy: "Copy", paste: "Paste", replace: "Replace" };
const card = document.getElementById("card") as HTMLElement;
const cardClose = card.querySelector(".card-close") as HTMLButtonElement;
const cardTag = card.querySelector(".card-tag span") as HTMLSpanElement;
const cardHeader = card.querySelector(".card-header") as HTMLSpanElement;
const cardBody = card.querySelector(".card-body") as HTMLDivElement;
const cardFoot = card.querySelector(".card-foot") as HTMLDivElement;
let cardGeneration = 0;

function reportCardBounds(): void {
  if (card.hidden) return;
  const box = card.getBoundingClientRect();
  void invoke("card_bounds", {
    generation: cardGeneration,
    bounds: { left: box.left, top: box.top, width: box.width, height: box.height },
  }).catch(() => undefined);
}

function showCard(generation: number, view: CardView): void {
  cardGeneration = generation;
  card.setAttribute("aria-label", TAGS[view.tag]);
  card.dataset.tag = view.tag;
  cardTag.textContent = TAGS[view.tag];
  cardHeader.textContent = view.header === null ? "" : `“${view.header}”`;
  cardBody.textContent = view.text;
  cardBody.scrollTop = 0;
  cardFoot.replaceChildren(...view.actions.map((action) => {
    const button = document.createElement("button");
    button.type = "button";
    button.className = action === view.primary ? "card-button primary" : "card-button";
    button.dataset.action = action;
    button.textContent = ACTIONS[action];
    return button;
  }));
  cardClose.disabled = false;
  card.hidden = false;
  // Laid out already; again once the window has grown and the entrance ends.
  reportCardBounds();
}

card.addEventListener("click", (event) => {
  const button = (event.target as Element).closest<HTMLButtonElement>("button[data-action]");
  if (button === null || button.disabled) return;
  // One action per Card: Rust also takes it only once.
  card.querySelectorAll("button").forEach((other) => { other.disabled = true; });
  void invoke("card_action", { generation: cardGeneration, action: button.dataset.action }).catch(() => undefined);
});
card.addEventListener("animationend", reportCardBounds);
window.addEventListener("resize", reportCardBounds);
void listen<{ generation: number; card: CardView } | null>("card", (event) => {
  if (event.payload === null) card.hidden = true;
  else showCard(event.payload.generation, event.payload.card);
});
