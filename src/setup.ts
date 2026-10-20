import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./theme.css";
import "./app.css";
import "./settings.css";
import "./setup.css";
import { openStartupFolder, readAutostart, renderAutostart, writeAutostart, type AutostartStatus } from "./autostart";
import {
  combo,
  el,
  engineStatus,
  must,
  parseShortcut,
  type DictationRecord,
  type LocalModelStatus,
  type ProviderInfo,
  type Settings,
} from "./records";

// The first-run guide (PARLA-100). Every step is skippable, and a step changes
// a setting only when the user changed something in it: the Settings commands
// do the work, so a rerun on a configured Parla leaves everything as it is.

type StepId = "welcome" | "mic" | "engine" | "try" | "start" | "done";
const STEPS: StepId[] = ["welcome", "mic", "engine", "try", "start", "done"];
/** Opened by itself on a new install (#first), or by Run setup again (#again). */
const firstRun = location.hash === "#first";

const ENV_KEYS: Record<string, string> = { openai: "OPENAI_API_KEY", xai: "XAI_API_KEY" };
/** Louder than this on the meter (the pill's scale) counts as hearing the user. */
const HEARD = 0.2;

const sections = Object.fromEntries(STEPS.map((id) => [id, must<HTMLElement>(`step-${id}`)])) as Record<StepId, HTMLElement>;
const stepLabel = must<HTMLSpanElement>("step-label");
const progressFill = must<HTMLDivElement>("wiz-fill");
const backButton = must<HTMLButtonElement>("back");
const skipButton = must<HTMLButtonElement>("skip");
const nextButton = must<HTMLButtonElement>("next");
const skipAllButton = must<HTMLButtonElement>("skip-all");
const openAppButton = must<HTMLButtonElement>("open-app");

let index = 0;
let busy = false;

const sleep = (ms: number): Promise<void> => new Promise((resolve) => window.setTimeout(resolve, ms));

/** A status line under a card, in the Settings colours. */
function say(element: HTMLElement, text: string, kind: "success" | "error" | "" = ""): void {
  element.textContent = text;
  element.className = kind ? `status provider-status status-${kind}` : "status provider-status";
}

const getSettings = (): Promise<Settings> => invoke<Settings>("get_settings");

/** What the active engine needs, from the saved settings. */
async function engineNow(): Promise<{ settings: Settings; providers: ProviderInfo[]; models: LocalModelStatus[] }> {
  const [settings, providers, models] = await Promise.all([
    getSettings(),
    invoke<ProviderInfo[]>("list_providers"),
    invoke<LocalModelStatus[]>("list_models"),
  ]);
  return { settings, providers, models };
}

/** Same rows as the Home page's shortcut box. */
function renderShortcut(target: HTMLElement, hotkey: string): void {
  const { keys, modifiers, hold } = parseShortcut(hotkey);
  const rows: [string, Node[]][] = hold
    ? [
        ["Push to talk", [document.createTextNode("Hold "), ...combo(modifiers)]],
        ["Hands-free", [document.createTextNode("Tap "), ...combo(keys)]],
        ["Cancel", combo(["Esc"])],
      ]
    : [
        ["Start / stop", [document.createTextNode("Tap "), ...combo(keys)]],
        ["Cancel", combo(["Esc"])],
      ];
  target.replaceChildren(
    ...rows.flatMap(([label, parts]) => {
      const dd = el("dd");
      dd.append(...parts);
      return [el("dt", undefined, label), dd];
    }),
  );
}

// --- Welcome ---------------------------------------------------------------

async function enterWelcome(): Promise<void> {
  renderShortcut(must("how"), (await getSettings()).hotkey);
}

// --- Microphone ------------------------------------------------------------

const micName = must<HTMLParagraphElement>("mic-name");
const micStatus = must<HTMLParagraphElement>("mic-status");
const micAgain = must<HTMLButtonElement>("mic-again");
const meterFill = must<HTMLDivElement>("meter-fill");
let heard = false;

async function startMeter(): Promise<void> {
  micAgain.hidden = true;
  heard = false;
  meterFill.classList.remove("heard");
  say(micStatus, "Say something: the bar should move.");
  try {
    await invoke("mic_meter_start");
  } catch (error) {
    say(micStatus, String(error), "error");
    micAgain.hidden = false;
  }
}

async function stopMeter(): Promise<void> {
  meterFill.style.width = "0%";
  await invoke("mic_meter_stop").catch(() => undefined);
}

async function showMicName(): Promise<void> {
  const name = await invoke<string | null>("mic_info").catch(() => null);
  micName.textContent = name ?? "No microphone found";
}

async function enterMic(): Promise<void> {
  await showMicName();
  await startMeter();
}

// Same scale as the recording pill.
void listen<number>("setup://level", (event) => {
  if (STEPS[index] !== "mic") return;
  const level = Math.min(1, Math.sqrt(event.payload) * 2.2);
  meterFill.style.width = `${Math.round(level * 100)}%`;
  if (level > HEARD && !heard) {
    heard = true;
    meterFill.classList.add("heard");
    say(micStatus, "Parla hears you.", "success");
  }
});
void listen("setup://meter-stopped", () => {
  if (STEPS[index] !== "mic") return;
  meterFill.style.width = "0%";
  say(micStatus, "The meter paused after a minute.");
  micAgain.hidden = false;
});
micAgain.addEventListener("click", () => void startMeter());
must<HTMLButtonElement>("mic-sound").addEventListener("click", () => void invoke("open_sound_settings"));
// Back from Windows' sound settings: the default may have changed, and the
// recorder only re-checks it when a recording starts.
window.addEventListener("focus", () => {
  if (STEPS[index] !== "mic" || busy) return;
  void stopMeter().then(enterMic);
});

// --- Engine ----------------------------------------------------------------

const engineRadios = document.querySelectorAll<HTMLInputElement>('input[name="engine"]');
const cloudPanel = must<HTMLElement>("engine-cloud");
const localPanel = must<HTMLElement>("engine-local");
const cloudList = must<HTMLDivElement>("cloud-providers");
const keyInput = must<HTMLInputElement>("cloud-key");
const keyLabel = must<HTMLLabelElement>("cloud-key-label");
const keyHint = must<HTMLParagraphElement>("cloud-key-hint");
const reveal = must<HTMLButtonElement>("cloud-reveal");
const localStatus = must<HTMLParagraphElement>("local-status");
const localHint = must<HTMLParagraphElement>("local-hint");
const localBar = must<HTMLDivElement>("local-bar");
const localFill = must<HTMLDivElement>("local-fill");
const engineResult = must<HTMLParagraphElement>("engine-result");

let engine: "cloud" | "local" = "cloud";
let cloudId = "openai";
let engineState: Awaited<ReturnType<typeof engineNow>> | null = null;
let downloading = false;

const formatSize = (bytes: number): string => `${Math.round(bytes / 1_000_000)} MB`;
const cloudProviders = (): ProviderInfo[] => (engineState?.providers ?? []).filter((p) => p.id !== "parakeet");
const providerLabel = (id: string): string => engineState?.providers.find((p) => p.id === id)?.display_name ?? id;
/** The local model Settings has selected (v3 unless the user picked Ultra). */
const localModel = (): LocalModelStatus | undefined =>
  engineState?.models.find((m) => m.id === engineState?.settings.local_model) ?? engineState?.models[0];

function renderEngine(): void {
  if (!engineState) return;
  for (const radio of engineRadios) radio.checked = radio.value === engine;
  cloudPanel.hidden = engine !== "cloud";
  localPanel.hidden = engine !== "local";

  cloudList.replaceChildren();
  for (const provider of cloudProviders()) {
    const label = el("label", "provider-option model-option");
    const radio = el("input");
    radio.type = "radio";
    radio.name = "cloud-provider";
    radio.value = provider.id;
    radio.checked = provider.id === cloudId;
    radio.addEventListener("change", () => {
      cloudId = provider.id;
      keyInput.value = "";
      renderEngine();
    });
    const body = el("span", "provider-option-body");
    body.append(el("span", "provider-name", provider.display_name));
    body.append(el("span", "model-meta", provider.available ? "API key set" : "No API key yet"));
    label.append(radio, body);
    cloudList.append(label);
  }
  const provider = engineState.providers.find((p) => p.id === cloudId);
  const name = providerLabel(cloudId);
  keyLabel.textContent = `${name} API key`;
  keyInput.placeholder = provider?.available ? "Saved. Leave empty to keep it" : "";
  keyHint.textContent = provider?.available
    ? `A key is already set. Type one only to replace it. ${ENV_KEYS[cloudId] ?? "An environment variable"} overrides the saved key.`
    : `Create a key in your ${name} account and paste it here. It is stored on this PC, in Parla's settings file.`;

  const model = localModel();
  if (model) {
    localStatus.textContent = model.available
      ? `${model.name} is downloaded and ready.`
      : `${model.name}: ${formatSize(model.size)} to download, once. It starts when you press Continue.`;
    localHint.textContent = downloading
      ? "You can leave this window: the download finishes in the background, then choose Local in Settings."
      : "Needs about 1.4 GB of free memory while you dictate. Slower than the cloud on long clips.";
  }
  localBar.hidden = !downloading;
}

async function enterEngine(): Promise<void> {
  say(engineResult, "");
  engineState = await engineNow();
  const active = engineState.settings.active_provider;
  engine = active === "parakeet" ? "local" : "cloud";
  if (active !== "parakeet" && cloudProviders().some((p) => p.id === active)) cloudId = active;
  keyInput.value = "";
  keyInput.type = "password";
  reveal.textContent = "Show";
  renderEngine();
}

for (const radio of engineRadios) {
  radio.addEventListener("change", () => {
    if (!radio.checked || downloading) return;
    engine = radio.value as "cloud" | "local";
    say(engineResult, "");
    renderEngine();
  });
}
reveal.addEventListener("click", () => {
  const show = keyInput.type === "password";
  keyInput.type = show ? "text" : "password";
  reveal.textContent = show ? "Hide" : "Show";
  reveal.setAttribute("aria-label", `${show ? "Hide" : "Show"} API key`);
});

void listen<{ id?: string; downloaded?: number; total?: number }>("model://progress", (event) => {
  const { downloaded, total } = event.payload;
  if (!downloading || typeof downloaded !== "number" || typeof total !== "number" || total <= 0) return;
  const percent = Math.min(100, Math.round((downloaded / total) * 100));
  localFill.style.width = `${percent}%`;
  localStatus.textContent = `Downloading… ${percent}%`;
  nextButton.textContent = `Downloading… ${percent}%`;
});

async function applyEngine(): Promise<boolean> {
  if (!engineState) return true;
  say(engineResult, "");
  const current = await getSettings();
  if (engine === "cloud") {
    const key = keyInput.value.trim();
    const provider = engineState.providers.find((p) => p.id === cloudId);
    if (key === "" && !provider?.available) {
      say(engineResult, `Paste your ${providerLabel(cloudId)} API key, or choose Skip.`, "error");
      return false;
    }
    try {
      if (key !== "") {
        // The whole settings go back, as from the Settings page; only the key differs.
        const section = current.providers[cloudId] ?? {};
        current.providers[cloudId] = { ...section, api_key: key };
        await invoke("save_settings", { settings: current });
      }
      if (current.active_provider !== cloudId) await invoke("select_provider", { id: cloudId });
    } catch (error) {
      say(engineResult, String(error), "error");
      return false;
    }
    return true;
  }
  const model = localModel();
  if (!model) return true;
  try {
    if (!model.available) {
      downloading = true;
      localFill.style.width = "0%";
      nextButton.textContent = "Downloading…";
      renderEngine();
      await invoke("select_model", { id: model.id });
    }
    if (current.active_provider !== "parakeet") await invoke("select_provider", { id: "parakeet" });
  } catch (error) {
    say(engineResult, String(error), "error");
    return false;
  } finally {
    downloading = false;
    nextButton.textContent = "Continue";
  }
  return true;
}

// --- Try it ----------------------------------------------------------------

const tryLede = must<HTMLParagraphElement>("try-lede");
const tryBox = must<HTMLTextAreaElement>("try-box");
const tryStatus = must<HTMLParagraphElement>("try-status");

async function enterTry(): Promise<void> {
  const { settings, providers, models } = await engineNow();
  const { modifiers, keys, hold } = parseShortcut(settings.hotkey);
  tryLede.replaceChildren(
    ...(hold
      ? ["Click the box, hold ", ...combo(modifiers), " and say something, then let go."]
      : ["Click the box, tap ", ...combo(keys), ", say something, then tap it again."]),
  );
  const status = engineStatus(settings, providers, models);
  if (status.ready) say(tryStatus, "");
  else say(tryStatus, `${status.engine} isn't ready yet: ${status.reason ?? "no reason given"}. Go back to set it up, or skip.`, "error");
  tryBox.value = "";
  tryBox.focus();
}

// A real dictation: the hotkey, the recorder, the engine and the paste, as
// anywhere else. The box is simply where the paste lands.
void listen<DictationRecord>("dictation://recorded", async (event) => {
  const record = event.payload;
  if (STEPS[index] !== "try" || record.kind !== "dictation") return;
  if (record.error !== null) {
    say(tryStatus, `That didn't work: ${record.error}`, "error");
    return;
  }
  await sleep(400);
  if (tryBox.value.trim() === "") {
    say(tryStatus, "Parla heard you, but the box wasn't selected. Click inside it and try again.", "error");
    return;
  }
  const took = record.pasted === null ? "" : `, ${(record.pasted / 1000).toFixed(1)} s after you let go`;
  say(tryStatus, `Pasted${took}.`, "success");
});

// --- Start with Windows ----------------------------------------------------

const autostart = {
  box: must<HTMLInputElement>("autostart"),
  note: must<HTMLParagraphElement>("autostart-note"),
  folder: must<HTMLButtonElement>("autostart-folder"),
  repair: must<HTMLButtonElement>("autostart-repair"),
};
const startResult = must<HTMLParagraphElement>("start-result");
let autostartNow: AutostartStatus | null = null;

async function enterStart(): Promise<void> {
  say(startResult, "");
  try {
    autostartNow = await readAutostart();
  } catch (error) {
    say(startResult, `Could not read the setting: ${String(error)}`, "error");
    return;
  }
  renderAutostart(autostart, autostartNow);
  // On a new install the answer starts at yes; on a rerun it is what it is.
  const open = autostartNow.supported && !autostartNow.enabled && autostartNow.shortcut === null;
  if (firstRun && open) autostart.box.checked = true;
}

async function applyStart(): Promise<boolean> {
  if (!autostartNow?.supported || autostart.box.disabled) return true;
  const want = autostart.box.checked;
  if (want === autostartNow.enabled) return true;
  try {
    autostartNow = await writeAutostart(want);
    renderAutostart(autostart, autostartNow);
    return true;
  } catch (error) {
    say(startResult, String(error), "error");
    return false;
  }
}

autostart.folder.addEventListener("click", () => void openStartupFolder());
autostart.repair.addEventListener("click", () => {
  void writeAutostart(true).then(
    (status) => {
      autostartNow = status;
      renderAutostart(autostart, status);
    },
    (error) => say(startResult, String(error), "error"),
  );
});
window.addEventListener("focus", () => {
  // The shortcut may have just been deleted in Explorer.
  if (STEPS[index] !== "start" || busy) return;
  const wanted = autostart.box.checked;
  void readAutostart().then((status) => {
    autostartNow = status;
    renderAutostart(autostart, status);
    if (!status.enabled && !autostart.box.disabled) autostart.box.checked = wanted;
  }, () => undefined);
});

// --- Done ------------------------------------------------------------------

async function enterDone(): Promise<void> {
  const { settings, providers, models } = await engineNow();
  const engineLine = engineStatus(settings, providers, models);
  const start = await readAutostart().catch(() => null);
  const startText = start === null || !start.supported
    ? "Not available"
    : start.enabled ? "On" : start.shortcut !== null ? "Through a Startup shortcut" : "Off";
  const rows: [string, string][] = [
    ["Engine", engineLine.ready ? engineLine.engine : `${engineLine.engine} (not ready)`],
    ["Start with Windows", startText],
  ];
  must("summary").replaceChildren(...rows.flatMap(([label, value]) => [el("dt", undefined, label), el("dd", undefined, value)]));
}

// --- Steps -----------------------------------------------------------------

interface Step {
  enter?: () => Promise<void>;
  leave?: () => Promise<void>;
  /** Continue was pressed: apply the step's change. `false` keeps the user here. */
  apply?: () => Promise<boolean>;
}

const STEP_HOOKS: Record<StepId, Step> = {
  welcome: { enter: enterWelcome },
  mic: { enter: enterMic, leave: stopMeter },
  engine: { enter: enterEngine, apply: applyEngine },
  try: { enter: enterTry },
  start: { enter: enterStart, apply: applyStart },
  done: { enter: enterDone },
};

function setBusy(value: boolean): void {
  busy = value;
  for (const button of [backButton, skipButton, nextButton, skipAllButton, openAppButton]) button.disabled = value;
}

async function show(next: number): Promise<void> {
  setBusy(true);
  try {
    await STEP_HOOKS[STEPS[index]].leave?.();
    sections[STEPS[index]].hidden = true;
    index = next;
    const id = STEPS[index];
    sections[id].hidden = false;
    const middle = id !== "welcome" && id !== "done";
    stepLabel.textContent = middle ? `Step ${index} of ${STEPS.length - 2}` : "";
    progressFill.style.width = `${Math.round((index / (STEPS.length - 1)) * 100)}%`;
    backButton.hidden = !middle;
    skipButton.hidden = !middle;
    skipAllButton.hidden = id === "done";
    openAppButton.hidden = id !== "done";
    nextButton.textContent = id === "welcome" ? "Get started" : id === "done" ? "Finish" : "Continue";
    await STEP_HOOKS[id].enter?.();
  } catch (error) {
    console.error(error);
  } finally {
    setBusy(false);
    if (STEPS[index] === "try") tryBox.focus();
    else nextButton.focus();
  }
}

async function finish(openApp: boolean): Promise<void> {
  setBusy(true);
  await STEP_HOOKS[STEPS[index]].leave?.();
  await invoke("finish_setup", { openApp }).catch((error) => {
    console.error(error);
    setBusy(false);
  });
}

nextButton.addEventListener("click", () => {
  if (STEPS[index] === "done") return void finish(false);
  void (async () => {
    setBusy(true);
    let proceed = true;
    try {
      proceed = (await STEP_HOOKS[STEPS[index]].apply?.()) ?? true;
    } catch (error) {
      console.error(error);
    }
    setBusy(false);
    if (proceed) await show(index + 1);
  })();
});
skipButton.addEventListener("click", () => void show(index + 1));
backButton.addEventListener("click", () => void show(index - 1));
skipAllButton.addEventListener("click", () => void finish(false));
openAppButton.addEventListener("click", () => void finish(true));

// Closing the window by its X needs nothing here: Parla stops the meter and
// writes the settings file when the window goes.
void show(0);
