import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./theme.css";
import "./settings.css";
import { engineStatus, type LocalModelStatus, type ProviderInfo, type Settings } from "./records";

interface SaveResult {
  warning: string | null;
}

interface ModelProgress {
  id?: string;
  downloaded?: number;
  total?: number;
}

interface FieldSpec {
  key: string;
  label: string;
  type: "text" | "password";
  placeholder?: string;
  envHint?: string;
  /** Used to prefill the field and when the input is left empty. */
  default?: string;
  /** Store an empty input as JSON null instead of "". */
  nullable?: boolean;
}

const PROVIDER_FIELDS: Record<string, FieldSpec[]> = {
  openai: [
    { key: "api_key", label: "API key", type: "password", envHint: "OPENAI_API_KEY" },
    { key: "model", label: "Model", type: "text", default: "gpt-transcribe" },
    { key: "streaming_model", label: "Live transcription model", type: "text", default: "gpt-live-transcribe" },
  ],
  xai: [
    { key: "api_key", label: "API key", type: "password", envHint: "XAI_API_KEY" },
    { key: "model", label: "Model", type: "text", default: "grok-voice-transcribe-2.0" },
  ],
};

/** Main keys the Windows listener accepts (mirrors hotkey.rs key_map!).
 *  Modifiers are handled separately; Escape stays reserved for cancelling. */
function supportedKeyCodes(): Set<string> {
  const keys = [
    "Space", "Tab", "Backspace", "Delete", "Insert", "Home", "End",
    "PageUp", "PageDown", "CapsLock", "NumLock", "PrintScreen", "ScrollLock",
    "Pause", "Enter", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight",
    "Backquote", "Minus", "Equal", "BracketLeft", "BracketRight", "Semicolon",
    "Quote", "Backslash", "IntlBackslash", "Comma", "Period", "Slash",
  ];
  for (let f = 1; f <= 12; f += 1) keys.push(`F${f}`);
  for (let c = 65; c <= 90; c += 1) keys.push(`Key${String.fromCharCode(c)}`);
  for (let d = 0; d <= 9; d += 1) keys.push(`Digit${d}`);
  return new Set(keys);
}

const SUPPORTED_KEYS = supportedKeyCodes();
const MODIFIER_CODES = new Set([
  "ControlLeft",
  "ControlRight",
  "ShiftLeft",
  "ShiftRight",
  "AltLeft",
  "AltRight",
  "MetaLeft",
  "MetaRight",
  "OSLeft",
  "OSRight",
]);

function byId<T extends HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (!element) throw new Error(`Missing element #${id}`);
  return element as T;
}

const els = {
  form: byId<HTMLFormElement>("settings-form"),
  loadError: byId<HTMLParagraphElement>("load-error"),
  providers: byId<HTMLDivElement>("providers"),
  providerStatus: byId<HTMLParagraphElement>("provider-status"),
  providerConfig: byId<HTMLDivElement>("provider-config"),
  hotkey: byId<HTMLInputElement>("hotkey"),
  capture: byId<HTMLButtonElement>("capture"),
  captureHint: byId<HTMLParagraphElement>("capture-hint"),
  delay: byId<HTMLInputElement>("delay"),
  pasteCompatibility: byId<HTMLInputElement>("paste-compatibility"),
  autoSend: byId<HTMLInputElement>("auto-send"),
  streaming: byId<HTMLInputElement>("streaming-enabled"),
  vocabulary: byId<HTMLTextAreaElement>("vocabulary"),
  liveCaptions: byId<HTMLInputElement>("live-captions"),
  dimAudio: byId<HTMLInputElement>("dim-audio"),
  dimLevel: byId<HTMLInputElement>("dim-level"),
  commandEnabled: byId<HTMLInputElement>("command-enabled"),
  commandKeyHint: byId<HTMLParagraphElement>("command-key-hint"),
  commandHotkey: byId<HTMLInputElement>("command-hotkey"),
  commandCapture: byId<HTMLButtonElement>("command-capture"),
  commandWarning: byId<HTMLParagraphElement>("command-hotkey-warning"),
  commandModel: byId<HTMLInputElement>("command-model"),
  commandEffort: byId<HTMLSelectElement>("command-effort"),
  status: byId<HTMLSpanElement>("status"),
  savebar: byId<HTMLDivElement>("savebar"),
  discard: byId<HTMLButtonElement>("discard"),
  save: byId<HTMLButtonElement>("save"),
};

const state: { settings: Settings | null; providers: ProviderInfo[]; baseline: string } = {
  settings: null,
  providers: [],
  baseline: "",
};

let saving = false;
let switchingProvider = false;
/** Re-read each provider field from the settings, after Discard. */
const fieldSyncs: Array<() => void> = [];
/** Local models as last listed (also names the active one in the rail). */
let models: LocalModelStatus[] = [];

/** What the rest of the window shows from the *saved* settings. */
export interface Applied {
  hotkey: string;
  engine: string;
  ready: boolean;
  reason: string | null;
}

let appliedListener: ((applied: Applied) => void) | null = null;

export function onApplied(listener: (applied: Applied) => void): void {
  appliedListener = listener;
  notifyApplied();
}

function notifyApplied(): void {
  if (!appliedListener || state.baseline === "") return;
  const saved: Settings = JSON.parse(state.baseline);
  appliedListener({ hotkey: saved.hotkey, ...engineStatus(saved, state.providers, models) });
}

function textEl(tag: string, text: string, className?: string): HTMLElement {
  const element = document.createElement(tag);
  element.textContent = text;
  if (className) element.className = className;
  return element;
}

let statusTimer: number | undefined;

function setStatus(text: string, kind: "success" | "warning" | "error" | "dirty" | "" = ""): void {
  els.status.textContent = text;
  els.status.className = kind ? `status status-${kind}` : "status";
  els.status.dataset.kind = kind;
  window.clearTimeout(statusTimer);
  // "Saved" confirms, it is not a state: let the bar go after a moment.
  if (kind === "success") {
    statusTimer = window.setTimeout(() => {
      setStatus("");
      updateDirty();
    }, 2500);
  }
}

// The provider applies on click (select_provider), not on Save, so its
// feedback sits in the provider card instead of next to the Save button.
function setProviderStatus(text: string, kind: "success" | "error" | "" = ""): void {
  els.providerStatus.textContent = text;
  els.providerStatus.className = kind ? `status provider-status status-${kind}` : "status provider-status";
}

function updateDirty(): void {
  if (!state.settings) return;
  // An OpenAI key typed on the Transcription page enables the toggle before saving.
  renderCommandAvailability();
  // Re-checks the command hotkey only when it or the dictation hotkey changed.
  void checkCommandHotkey();
  const dirty = JSON.stringify(state.settings) !== state.baseline;
  els.save.disabled = saving || switchingProvider || !dirty;
  els.discard.disabled = els.save.disabled;
  if (dirty) {
    if (els.status.dataset.kind !== "error" && els.status.dataset.kind !== "warning") {
      setStatus("Unsaved changes", "dirty");
    }
  } else if (els.status.dataset.kind === "dirty") {
    setStatus("");
  }
  // The bar sticks to the bottom only while there is something to save or report.
  const buttons = dirty || saving;
  els.save.hidden = !buttons;
  els.discard.hidden = !buttons;
  els.savebar.hidden = !buttons && els.status.textContent === "";
}

function providerSection(settings: Settings, id: string): Record<string, unknown> {
  const existing = settings.providers[id];
  if (existing && typeof existing === "object") return existing;
  const created: Record<string, unknown> = {};
  settings.providers[id] = created;
  return created;
}

function renderProviderRadios(): void {
  const settings = state.settings;
  if (!settings) return;
  els.providers.replaceChildren();
  if (state.providers.length === 0) {
    els.providers.append(textEl("p", "No providers are registered in this build.", "hint"));
    return;
  }
  for (const provider of state.providers) {
    const label = document.createElement("label");
    label.className = "provider-option";
    if (!provider.available) label.classList.add("unavailable");

    const radio = document.createElement("input");
    radio.type = "radio";
    radio.name = "active_provider";
    radio.value = provider.id;
    radio.checked = settings.active_provider === provider.id;
    radio.disabled = !provider.available || saving || switchingProvider;
    radio.addEventListener("change", () => {
      if (!radio.checked || !state.settings) return;
      void selectProvider(provider.id);
    });

    const body = document.createElement("span");
    body.className = "provider-option-body";
    body.append(textEl("span", provider.display_name, "provider-name"));
    if (!provider.available) {
      body.append(textEl("span", provider.reason ?? "Unavailable", "provider-reason"));
    }

    label.append(radio, body);
    els.providers.append(label);
  }
}

async function selectProvider(id: string): Promise<void> {
  if (!state.settings || saving || switchingProvider) return;
  const previous = state.settings.active_provider;
  state.settings.active_provider = id;
  switchingProvider = true;
  renderProviderRadios();
  updateDirty();
  setProviderStatus("Switching provider…");
  try {
    await invoke("select_provider", { id });
    const baseline: Settings = JSON.parse(state.baseline);
    baseline.active_provider = id;
    state.baseline = JSON.stringify(baseline);
    const name = state.providers.find((p) => p.id === id)?.display_name ?? id;
    setProviderStatus(`${name} is now active. Saved.`, "success");
    notifyApplied();
  } catch (error) {
    state.settings.active_provider = previous;
    setProviderStatus(`Could not switch provider: ${String(error)}`, "error");
  } finally {
    switchingProvider = false;
    renderProviderRadios();
    updateDirty();
  }
}

function revealToggle(input: HTMLInputElement): HTMLButtonElement {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "secondary reveal";
  button.textContent = "Show";
  button.setAttribute("aria-label", "Show API key");
  button.addEventListener("click", () => {
    const reveal = input.type === "password";
    input.type = reveal ? "text" : "password";
    button.textContent = reveal ? "Hide" : "Show";
    button.setAttribute("aria-label", reveal ? "Hide API key" : "Show API key");
  });
  return button;
}

function buildField(providerId: string, spec: FieldSpec): HTMLElement {
  // Looked up on each use: Discard replaces the whole settings object.
  const config = (): Record<string, unknown> => providerSection(state.settings as Settings, providerId);
  const wrapper = document.createElement("div");
  wrapper.className = "field";

  const inputId = `field-${providerId}-${spec.key}`;
  const label = textEl("label", spec.label) as HTMLLabelElement;
  label.htmlFor = inputId;

  const input = document.createElement("input");
  input.id = inputId;
  input.type = spec.type;
  input.autocomplete = "off";
  input.spellcheck = false;
  if (spec.placeholder) input.placeholder = spec.placeholder;
  const sync = (): void => {
    const current = config()[spec.key];
    input.value = typeof current === "string" && current.trim() !== "" ? current : (spec.default ?? "");
  };
  sync();
  fieldSyncs.push(sync);
  if (spec.default !== undefined && config()[spec.key] !== input.value) {
    config()[spec.key] = input.value;
  }
  input.addEventListener("input", () => {
    const empty = input.value.trim() === "";
    config()[spec.key] = empty ? (spec.default ?? (spec.nullable ? null : "")) : input.value;
    updateDirty();
  });

  wrapper.append(label);
  if (spec.type === "password") {
    const row = document.createElement("div");
    row.className = "password-row";
    row.append(input, revealToggle(input));
    wrapper.append(row);
  } else {
    wrapper.append(input);
  }
  if (spec.envHint) {
    wrapper.append(textEl("p", `${spec.envHint} overrides this value at request time.`, "hint"));
  }
  return wrapper;
}

interface ParakeetUi {
  list: HTMLDivElement;
  status: HTMLParagraphElement;
  dir: HTMLParagraphElement;
  bar: HTMLDivElement;
  fill: HTMLDivElement;
}

function formatSize(bytes: number): string {
  return `${Math.round(bytes / 1_000_000)} MB`;
}

// Set by setupParakeet so a successful save can re-check the model status.
let refreshParakeetStatus: (() => Promise<void>) | null = null;

async function setupParakeet(ui: ParakeetUi): Promise<void> {
  /** The model a click in this window is switching to (download included). */
  let switching: string | null = null;
  const percents = new Map<string, number>();
  // Invalidate in-flight list reads when a click or a terminal event starts,
  // so a slow initial list_models cannot clobber the download state.
  let statusToken = 0;
  const setMessage = (text: string, isError = false): void => {
    ui.status.textContent = text;
    ui.status.classList.toggle("error", isError);
  };
  const showProgress = (visible: boolean): void => {
    ui.bar.hidden = !visible;
  };
  const busy = (): boolean => switching !== null || models.some((m) => m.downloading);
  const selectedId = (): string => state.settings?.local_model ?? "parakeet-v3";

  const render = (): void => {
    ui.list.replaceChildren();
    for (const model of models) {
      const label = document.createElement("label");
      label.className = "provider-option model-option";

      const radio = document.createElement("input");
      radio.type = "radio";
      radio.name = "local_model";
      radio.value = model.id;
      radio.checked = selectedId() === model.id;
      radio.disabled = busy();
      radio.addEventListener("change", () => {
        if (radio.checked) void select(model.id);
      });

      const body = document.createElement("span");
      body.className = "provider-option-body";
      body.append(textEl("span", model.name, "provider-name"));
      const percent = percents.get(model.id);
      const progress = model.downloading || (switching === model.id && !model.available)
        ? `Downloading${percent === undefined ? "…" : ` ${percent}%`}`
        : model.available
          ? "Downloaded"
          : "Not downloaded";
      body.append(textEl("span", `${formatSize(model.size)} · ${progress}`, "model-meta"));
      label.append(radio, body);

      if (!model.available && !model.downloading && switching !== model.id) {
        const button = document.createElement("button");
        button.type = "button";
        button.className = "secondary model-download";
        button.textContent = "Download";
        button.disabled = busy();
        button.addEventListener("click", (event) => {
          event.preventDefault();
          void select(model.id);
        });
        label.append(button);
      }
      ui.list.append(label);
    }
    const selected = models.find((m) => m.id === selectedId());
    ui.dir.textContent = selected ? `Model directory: ${selected.model_dir}` : "";
  };

  const refresh = async (report = true): Promise<void> => {
    const token = ++statusToken;
    try {
      const next = await invoke<LocalModelStatus[]>("list_models");
      if (token !== statusToken) return;
      models = next;
      const downloading = models.find((m) => m.downloading);
      showProgress(Boolean(downloading));
      if (report && !switching) {
        const selected = models.find((m) => m.id === selectedId());
        if (downloading) setMessage(`Downloading ${downloading.name}…`);
        else if (selected?.available) setMessage(`${selected.name} is ready.`);
        else if (selected) setMessage(`${selected.name} is not downloaded yet.`);
      }
      render();
      notifyApplied();
    } catch (error) {
      if (token !== statusToken) return;
      setMessage(String(error), true);
    }
  };

  // Selecting a model that is not on disk downloads it first; the backend
  // switches only once every file is verified.
  const select = async (id: string): Promise<void> => {
    const model = models.find((m) => m.id === id);
    if (!model || busy()) {
      render();
      return;
    }
    statusToken += 1;
    switching = id;
    percents.delete(id);
    if (!model.available) {
      showProgress(true);
      ui.fill.style.width = "0%";
      setMessage(`Downloading ${model.name}…`);
    } else {
      setMessage(`Switching to ${model.name}…`);
    }
    render();
    try {
      const selected = await invoke<string>("select_model", { id });
      if (state.settings) state.settings.local_model = selected;
      const baseline: Settings = JSON.parse(state.baseline);
      baseline.local_model = selected;
      state.baseline = JSON.stringify(baseline);
      const name = models.find((m) => m.id === selected)?.name ?? selected;
      setMessage(selected === id ? `${name} selected.` : `${model.name} downloaded; ${name} stays selected.`);
      notifyApplied();
    } catch (error) {
      setMessage(String(error), true);
    } finally {
      switching = null;
      showProgress(false);
      await refresh(false);
      await refreshProviders();
      updateDirty();
    }
  };

  // Attach listeners before the first status read so no event is missed.
  await Promise.all([
    listen<ModelProgress>("model://progress", (event) => {
      const id = event.payload?.id;
      if (!id) return;
      showProgress(true);
      const { downloaded, total } = event.payload;
      if (typeof downloaded === "number" && typeof total === "number" && total > 0) {
        const percent = Math.min(100, Math.round((downloaded / total) * 100));
        ui.fill.style.width = `${percent}%`;
        if (percents.get(id) !== percent) {
          percents.set(id, percent);
          const model = models.find((m) => m.id === id);
          if (model) {
            model.downloading = true;
            setMessage(`Downloading ${model.name}… ${percent}%`);
          }
          render();
        }
      }
    }),
    listen<{ id?: string }>("model://done", () => {
      ui.fill.style.width = "100%";
      if (!switching) void refresh();
    }),
    listen<{ id?: string; error?: string }>("model://error", (event) => {
      if (switching) return; // select() reports its own failure.
      setMessage(event.payload?.error ?? "Model download failed.", true);
      showProgress(false);
      void refresh();
    }),
  ]);

  refreshParakeetStatus = refresh;
  await refresh();
}

function buildParakeet(providerId: string): HTMLElement {
  const wrapper = document.createElement("div");
  wrapper.className = "field";

  wrapper.append(textEl("label", "Local model"));
  const list = document.createElement("div");
  list.className = "provider-list model-list";
  const status = textEl("p", "Checking models…", "model-status") as HTMLParagraphElement;
  const dir = textEl("p", "", "hint") as HTMLParagraphElement;
  const bar = document.createElement("div");
  bar.className = "progress";
  bar.hidden = true;
  const fill = document.createElement("div");
  fill.className = "progress-fill";
  bar.append(fill);
  wrapper.append(list, bar, status, dir);

  const override = buildField(providerId, {
    key: "model_dir",
    label: "Parakeet v3 directory",
    type: "text",
    placeholder: "Default location",
    nullable: true,
  });
  override.classList.add("model-override");

  const container = document.createElement("div");
  container.append(wrapper, override);
  void setupParakeet({ list, status, dir, bar, fill });
  return container;
}

function renderProviderConfig(): void {
  const settings = state.settings;
  if (!settings) return;
  els.providerConfig.replaceChildren();
  for (const provider of state.providers) {
    const card = document.createElement("section");
    card.className = "card";
    card.append(textEl("h2", provider.display_name));

    if (provider.id === "parakeet") {
      card.append(buildParakeet(provider.id));
    } else if (PROVIDER_FIELDS[provider.id]) {
      for (const spec of PROVIDER_FIELDS[provider.id]) {
        card.append(buildField(provider.id, spec));
      }
    } else {
      card.append(textEl("p", "This provider has no options in the settings window.", "hint"));
    }
    els.providerConfig.append(card);
  }
}

async function refreshProviders(): Promise<void> {
  try {
    state.providers = await invoke<ProviderInfo[]>("list_providers");
    renderProviderRadios();
    notifyApplied();
  } catch {
    // Keep the previous list; a transient read failure is not fatal.
  }
}

const DEFAULT_COMMAND_MODEL = "gpt-6-luna";

/** Command mode needs an OpenAI key: the saved one or OPENAI_API_KEY (the
 *  OpenAI provider is available), or one typed in this form. */
function renderCommandAvailability(): void {
  const settings = state.settings;
  if (!settings) return;
  const typed = settings.providers.openai?.api_key;
  const keySet = state.providers.some((p) => p.id === "openai" && p.available)
    || (typeof typed === "string" && typed.trim() !== "");
  // Without a key it can only be turned off, never on.
  els.commandEnabled.disabled = !keySet && !settings.command.enabled;
  els.commandKeyHint.hidden = keySet;
}

/** The (command, dictation) hotkeys last checked, so edits elsewhere in the
 *  form cost no IPC; cleared to force a re-check after a capture hint. */
let checkedPair = "";
let commandCheck = 0;

/** Shows inline why the command hotkey would be rejected (the backend's rule,
 *  which save_settings enforces while command mode is enabled). */
async function checkCommandHotkey(): Promise<void> {
  const settings = state.settings;
  if (!settings) return;
  const pair = JSON.stringify([settings.command.hotkey, settings.hotkey]);
  if (pair === checkedPair) return;
  checkedPair = pair;
  const token = ++commandCheck;
  let warning: string | null;
  try {
    warning = await invoke<string | null>("check_command_hotkey", {
      command: settings.command.hotkey,
      dictation: settings.hotkey,
    });
  } catch (error) {
    warning = String(error);
  }
  if (token !== commandCheck) return;
  showCommandWarning(warning);
}

function showCommandWarning(text: string | null): void {
  els.commandWarning.textContent = text ?? "";
  els.commandWarning.hidden = text === null;
}

let commandCaptureActive = false;
/** Every modifier pressed since the hold began; recorded on the first release. */
const commandHeld = new Set<string>();

function stopCommandCapture(): void {
  if (!commandCaptureActive) return;
  commandCaptureActive = false;
  commandHeld.clear();
  els.commandCapture.textContent = "Record keys";
  els.commandCapture.classList.remove("capturing");
  window.removeEventListener("keydown", onCommandCaptureKey, true);
  window.removeEventListener("keyup", onCommandCaptureKey, true);
  // Replace any capture hint with the field's own check.
  checkedPair = "";
  updateDirty();
}

/** The chord as hotkey.rs parses it. Ctrl keeps its side (the default is left
 *  Ctrl only); Shift and Win are either side; Alt is left Alt. */
function commandChord(codes: Set<string>): string {
  const ctrl = codes.has("ControlLeft") && codes.has("ControlRight")
    ? "Ctrl"
    : codes.has("ControlLeft") ? "LCtrl" : codes.has("ControlRight") ? "RCtrl" : null;
  const shift = codes.has("ShiftLeft") || codes.has("ShiftRight") ? "Shift" : null;
  const alt = codes.has("AltLeft") ? "Alt" : null;
  const win = codes.has("MetaLeft") || codes.has("MetaRight") ? "Win" : null;
  return [ctrl, shift, alt, win].filter((part) => part !== null).join("+");
}

function onCommandCaptureKey(event: KeyboardEvent): void {
  event.preventDefault();
  event.stopPropagation();
  if (event.type === "keydown") {
    if (event.key === "Escape") {
      stopCommandCapture();
    } else if (!MODIFIER_CODES.has(event.code)) {
      showCommandWarning("The command hotkey takes modifiers only: hold them, then release.");
    } else if (event.code === "AltRight") {
      showCommandWarning("AltGr (right Alt) cannot be part of the command hotkey.");
    } else {
      commandHeld.add(event.code);
    }
    return;
  }
  if (!commandHeld.has(event.code)) return;
  if (commandHeld.size >= 2 && state.settings) {
    const chord = commandChord(commandHeld);
    els.commandHotkey.value = chord;
    state.settings.command.hotkey = chord;
    stopCommandCapture();
  } else {
    commandHeld.clear();
  }
}

function startCommandCapture(): void {
  if (commandCaptureActive) return;
  stopCapture();
  commandCaptureActive = true;
  els.commandCapture.textContent = "Hold modifiers… (Esc cancels)";
  els.commandCapture.classList.add("capturing");
  showCommandWarning(null);
  window.addEventListener("keydown", onCommandCaptureKey, true);
  window.addEventListener("keyup", onCommandCaptureKey, true);
}

let captureActive = false;

function showCaptureHint(text: string): void {
  els.captureHint.textContent = text;
  els.captureHint.hidden = false;
}

function clearCaptureHint(): void {
  els.captureHint.textContent = "";
  els.captureHint.hidden = true;
}

function stopCapture(): void {
  if (!captureActive) return;
  captureActive = false;
  els.capture.textContent = "Record keys";
  els.capture.classList.remove("capturing");
  clearCaptureHint();
  window.removeEventListener("keydown", onCaptureKey, true);
}

function onCaptureKey(event: KeyboardEvent): void {
  event.preventDefault();
  event.stopPropagation();
  if (event.key === "Escape") {
    stopCapture();
    return;
  }
  // Modifiers are not main keys; keep recording until a real key arrives.
  if (MODIFIER_CODES.has(event.code)) return;
  if (!SUPPORTED_KEYS.has(event.code)) {
    showCaptureHint(
      `“${event.code}” is not supported. Use letters, digits, F1–F12, arrows, or punctuation.`,
    );
    return;
  }
  const parts: string[] = [];
  if (event.ctrlKey) parts.push("Ctrl");
  if (event.shiftKey) parts.push("Shift");
  if (event.altKey) parts.push("Alt");
  if (event.metaKey) parts.push("Super");
  parts.push(event.code);
  const accelerator = parts.join("+");
  els.hotkey.value = accelerator;
  if (state.settings) state.settings.hotkey = accelerator;
  updateDirty();
  stopCapture();
}

function startCapture(): void {
  if (captureActive) return;
  stopCommandCapture();
  captureActive = true;
  els.capture.textContent = "Press keys… (Esc cancels)";
  els.capture.classList.add("capturing");
  clearCaptureHint();
  window.addEventListener("keydown", onCaptureKey, true);
}

async function save(): Promise<void> {
  const settings = state.settings;
  if (!settings || saving || switchingProvider) return;
  const delay = Number.parseInt(els.delay.value, 10);
  if (!Number.isFinite(delay) || delay < 50 || delay > 2000) {
    setStatus("Restore delay must be between 50 and 2000 ms.", "error");
    return;
  }
  if (settings.vocabulary.length > 100 || settings.vocabulary.some(term => [...term].length > 50 || /[<>\x00-\x1f\x7f-\x9f]/u.test(term))) {
    setStatus("Use up to 100 entries of 1-50 characters, without control characters or angle brackets.", "error");
    return;
  }
  settings.restore_delay_ms = delay;
  // Snapshot exactly what is submitted. Edits made while the save is in flight
  // are not part of it, so they stay "unsaved" instead of being marked persisted.
  const submitted: Settings = JSON.parse(JSON.stringify(settings));
  saving = true;
  renderProviderRadios();
  setStatus("Saving…");
  updateDirty();
  try {
    const result = await invoke<SaveResult>("save_settings", { settings: submitted });
    state.baseline = JSON.stringify(submitted);
    if (result.warning) {
      setStatus(result.warning, "warning");
    } else {
      setStatus("Settings saved.", "success");
    }
    await refreshProviders();
    await refreshParakeetStatus?.();
  } catch (error) {
    setStatus(String(error), "error");
  } finally {
    saving = false;
    renderProviderRadios();
    updateDirty();
  }
}

/** Puts the form back to the saved settings. Provider and model switches are
 *  saved at once, so they are part of the baseline and stay. */
function discard(): void {
  if (!state.settings || saving || switchingProvider) return;
  stopCapture();
  stopCommandCapture();
  state.settings = JSON.parse(state.baseline) as Settings;
  fillForm(state.settings);
  for (const sync of fieldSyncs) sync();
  setStatus("");
  updateDirty();
}

/** Another window (the tray flyout) saved settings: take every field this form
 *  has not touched, keep the unsaved edits, and rebase on what was saved. */
async function onSettingsChanged(): Promise<void> {
  if (!state.settings || state.baseline === "") return;
  let saved: Settings;
  try {
    saved = await invoke<Settings>("get_settings");
  } catch {
    return;
  }
  const form = state.settings as unknown as Record<string, unknown>;
  const baseline = JSON.parse(state.baseline) as Record<string, unknown>;
  const touched = new Set(Object.keys(form).filter((key) => JSON.stringify(form[key]) !== JSON.stringify(baseline[key])));
  for (const [key, value] of Object.entries(saved)) {
    if (!touched.has(key)) form[key] = value;
  }
  state.baseline = JSON.stringify(saved);
  // Text being edited keeps what was typed (spaces, blank lines), not its parsed value.
  const texts = [
    [els.hotkey, "hotkey"], [els.delay, "restore_delay_ms"], [els.vocabulary, "vocabulary"],
    [els.commandHotkey, "command"], [els.commandModel, "command"],
  ] as const;
  const typed = texts.map(([input]) => input.value);
  fillForm(state.settings);
  texts.forEach(([input, key], index) => {
    if (touched.has(key)) input.value = typed[index];
  });
  if (!touched.has("providers")) for (const sync of fieldSyncs) sync();
  renderProviderRadios();
  updateDirty();
  await refreshProviders();
  await refreshParakeetStatus?.();
}

function fillForm(settings: Settings): void {
  els.hotkey.value = settings.hotkey;
  els.delay.value = String(settings.restore_delay_ms);
  els.pasteCompatibility.checked = settings.paste_compatibility_mode;
  els.autoSend.checked = settings.auto_send;
  els.streaming.checked = settings.streaming_enabled;
  els.liveCaptions.checked = settings.live_captions;
  els.dimAudio.checked = settings.dim_other_audio;
  els.dimLevel.value = String(settings.dim_audio_level);
  els.dimLevel.disabled = !settings.dim_other_audio;
  els.vocabulary.value = settings.vocabulary.join("\n");
  els.commandEnabled.checked = settings.command.enabled;
  els.commandHotkey.value = settings.command.hotkey;
  els.commandModel.value = settings.command.model || DEFAULT_COMMAND_MODEL;
  els.commandEffort.value = settings.command.reasoning_effort;
}

function wireForm(): void {
  els.vocabulary.addEventListener("input", () => {
    if (!state.settings) return;
    state.settings.vocabulary = els.vocabulary.value.split("\n").map(term => term.trim()).filter(Boolean);
    updateDirty();
  });
  els.hotkey.addEventListener("input", () => {
    if (!state.settings) return;
    state.settings.hotkey = els.hotkey.value.trim();
    updateDirty();
  });
  els.commandEnabled.addEventListener("change", () => {
    if (!state.settings) return;
    state.settings.command.enabled = els.commandEnabled.checked;
    updateDirty();
  });
  els.commandHotkey.addEventListener("input", () => {
    if (!state.settings) return;
    state.settings.command.hotkey = els.commandHotkey.value.trim();
    updateDirty();
  });
  els.commandCapture.addEventListener("click", startCommandCapture);
  els.commandModel.addEventListener("input", () => {
    if (!state.settings) return;
    state.settings.command.model = els.commandModel.value.trim() || DEFAULT_COMMAND_MODEL;
    updateDirty();
  });
  // A cleared model shows the default again once the field is left.
  els.commandModel.addEventListener("change", () => {
    if (state.settings) els.commandModel.value = state.settings.command.model;
  });
  els.commandEffort.addEventListener("change", () => {
    if (!state.settings) return;
    state.settings.command.reasoning_effort = els.commandEffort.value;
    updateDirty();
  });
  els.delay.addEventListener("input", () => {
    if (!state.settings) return;
    const value = Number.parseInt(els.delay.value, 10);
    if (Number.isFinite(value)) state.settings.restore_delay_ms = value;
    updateDirty();
  });
  els.capture.addEventListener("click", startCapture);
  els.pasteCompatibility.addEventListener("change", () => {
    if (!state.settings) return;
    state.settings.paste_compatibility_mode = els.pasteCompatibility.checked;
    updateDirty();
  });
  els.autoSend.addEventListener("change", () => {
    if (!state.settings) return;
    state.settings.auto_send = els.autoSend.checked;
    updateDirty();
  });
  els.streaming.addEventListener("change", () => {
    if (!state.settings) return;
    state.settings.streaming_enabled = els.streaming.checked;
    updateDirty();
  });
  els.liveCaptions.addEventListener("change", () => {
    if (!state.settings) return;
    state.settings.live_captions = els.liveCaptions.checked;
    updateDirty();
  });
  els.dimAudio.addEventListener("change", () => {
    if (!state.settings) return;
    state.settings.dim_other_audio = els.dimAudio.checked;
    els.dimLevel.disabled = !els.dimAudio.checked;
    updateDirty();
  });
  els.dimLevel.addEventListener("input", () => {
    if (!state.settings) return;
    const value = Number.parseInt(els.dimLevel.value, 10);
    if (Number.isFinite(value)) state.settings.dim_audio_level = Math.min(90, Math.max(10, value));
    updateDirty();
  });
  els.form.addEventListener("submit", (event) => {
    event.preventDefault();
    void save();
  });
  els.discard.addEventListener("click", discard);
}

async function main(): Promise<void> {
  wireForm();
  try {
    const [settings, providers] = await Promise.all([
      invoke<Settings>("get_settings"),
      invoke<ProviderInfo[]>("list_providers"),
    ]);
    settings.vocabulary ??= [];
    state.settings = settings;
    state.providers = providers;
    fillForm(settings);
    renderProviderRadios();
    renderProviderConfig();
    // After the provider fields wrote their defaults, so a fresh install
    // (no provider sections yet) does not open with unsaved changes.
    state.baseline = JSON.stringify(settings);
    updateDirty();
    notifyApplied();
    void listen("settings://changed", () => void onSettingsChanged());
  } catch (error) {
    els.loadError.hidden = false;
    els.loadError.textContent = `Could not load settings: ${String(error)}`;
    els.save.disabled = true;
    appliedListener?.({ hotkey: "", engine: "settings not loaded", ready: false, reason: String(error) });
  }
}

void main();
