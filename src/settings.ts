import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./style.css";
import "./settings.css";

interface Settings {
  active_provider: string;
  hotkey: string;
  restore_delay_ms: number;
  providers: Record<string, Record<string, unknown>>;
}

interface ProviderInfo {
  id: string;
  display_name: string;
  available: boolean;
  reason: string | null;
}

interface SaveResult {
  warning: string | null;
}

interface ModelProgress {
  id?: string;
  downloaded?: number;
  total?: number;
}

/** Shape of `model_status` from PARLA-7. */
interface ModelStatus {
  id: string;
  model_dir: string;
  available: boolean;
  downloading: boolean;
}

interface FieldSpec {
  key: string;
  label: string;
  type: "text" | "password";
  placeholder?: string;
  envHint?: string;
  /** Store an empty input as JSON null instead of "". */
  nullable?: boolean;
}

const PROVIDER_FIELDS: Record<string, FieldSpec[]> = {
  openai: [
    { key: "api_key", label: "API key", type: "password", envHint: "OPENAI_API_KEY" },
    { key: "model", label: "Model", type: "text", placeholder: "gpt-transcribe" },
  ],
  xai: [
    { key: "api_key", label: "API key", type: "password", envHint: "XAI_API_KEY" },
    { key: "model", label: "Model", type: "text" },
  ],
};

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
  providerConfig: byId<HTMLDivElement>("provider-config"),
  hotkey: byId<HTMLInputElement>("hotkey"),
  capture: byId<HTMLButtonElement>("capture"),
  delay: byId<HTMLInputElement>("delay"),
  status: byId<HTMLSpanElement>("status"),
  save: byId<HTMLButtonElement>("save"),
};

const state: { settings: Settings | null; providers: ProviderInfo[]; baseline: string } = {
  settings: null,
  providers: [],
  baseline: "",
};

function textEl(tag: string, text: string, className?: string): HTMLElement {
  const element = document.createElement(tag);
  element.textContent = text;
  if (className) element.className = className;
  return element;
}

function setStatus(text: string, kind: "success" | "warning" | "error" | "dirty" | "" = ""): void {
  els.status.textContent = text;
  els.status.className = kind ? `status status-${kind}` : "status";
  els.status.dataset.kind = kind;
}

function updateDirty(): void {
  if (!state.settings) return;
  const dirty = JSON.stringify(state.settings) !== state.baseline;
  els.save.disabled = !dirty;
  if (dirty) {
    if (els.status.dataset.kind !== "error" && els.status.dataset.kind !== "warning") {
      setStatus("Unsaved changes.", "dirty");
    }
  } else if (els.status.dataset.kind === "dirty") {
    setStatus("");
  }
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
    radio.disabled = !provider.available;
    radio.addEventListener("change", () => {
      if (!radio.checked || !state.settings) return;
      state.settings.active_provider = provider.id;
      updateDirty();
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

function buildField(providerId: string, spec: FieldSpec, config: Record<string, unknown>): HTMLElement {
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
  const current = config[spec.key];
  input.value = typeof current === "string" ? current : "";
  input.addEventListener("input", () => {
    config[spec.key] = spec.nullable && input.value.trim() === "" ? null : input.value;
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
  status: HTMLParagraphElement;
  dir: HTMLParagraphElement;
  bar: HTMLDivElement;
  fill: HTMLDivElement;
  button: HTMLButtonElement;
}

/** Missing-command rejection: the command exists only once PARLA-7 is merged. */
function isMissingCommand(error: unknown): boolean {
  const text = String(error).toLowerCase();
  return (
    text.includes("not found") || text.includes("unknown command") || text.includes("command_not_found")
  );
}

// Set by setupParakeet so a successful save can re-check the model status.
let refreshParakeetStatus: (() => Promise<void>) | null = null;

function setupParakeet(providerId: string, ui: ParakeetUi): void {
  let unavailable = false;
  const setMessage = (text: string, isError = false): void => {
    ui.status.textContent = text;
    ui.status.classList.toggle("error", isError);
  };
  const setDir = (dir: unknown): void => {
    ui.dir.textContent = typeof dir === "string" && dir ? `Model directory: ${dir}` : "";
  };
  const showProgress = (visible: boolean): void => {
    ui.bar.hidden = !visible;
  };

  const refresh = async (): Promise<void> => {
    try {
      const info = await invoke<ModelStatus>("model_status", { id: providerId });
      setDir(info.model_dir);
      showProgress(Boolean(info.downloading));
      if (info.downloading) {
        setMessage("Downloading…");
        ui.button.disabled = true;
      } else if (info.available) {
        setMessage("Model ready.");
        ui.fill.style.width = "100%";
        ui.button.disabled = true;
      } else {
        setMessage("Model not downloaded yet.");
        ui.button.disabled = false;
      }
    } catch (error) {
      if (isMissingCommand(error)) {
        unavailable = true;
        setMessage("Model download is not available in this build.", true);
        ui.button.disabled = true;
      } else {
        setMessage(String(error), true);
      }
    }
  };

  ui.button.addEventListener("click", async () => {
    if (unavailable) return;
    ui.button.disabled = true;
    showProgress(true);
    ui.fill.style.width = "0%";
    setMessage("Downloading…");
    try {
      // Resolves after completion, rejects on failure / already downloading / unknown id.
      await invoke("download_model", { id: providerId });
      await refresh();
    } catch (error) {
      if (isMissingCommand(error)) {
        unavailable = true;
        setMessage("Model download is not available in this build.", true);
      } else {
        setMessage(String(error), true);
        ui.button.disabled = false;
      }
      showProgress(false);
    }
  });

  void listen<ModelProgress>("model://progress", (event) => {
    if (event.payload?.id !== providerId) return;
    showProgress(true);
    const { downloaded, total } = event.payload;
    if (typeof downloaded === "number" && typeof total === "number" && total > 0) {
      const percent = Math.min(100, Math.round((downloaded / total) * 100));
      ui.fill.style.width = `${percent}%`;
      setMessage(`Downloading… ${percent}%`);
    } else {
      setMessage("Downloading…");
    }
  });
  void listen<{ id?: string }>("model://done", (event) => {
    if (event.payload?.id && event.payload.id !== providerId) return;
    ui.fill.style.width = "100%";
    setMessage("Model downloaded.");
    showProgress(false);
    ui.button.disabled = true;
    void refresh();
  });
  void listen<{ id?: string; error?: string }>("model://error", (event) => {
    if (event.payload?.id && event.payload.id !== providerId) return;
    setMessage(event.payload?.error ?? "Model download failed.", true);
    showProgress(false);
    ui.button.disabled = false;
  });

  refreshParakeetStatus = refresh;
  void refresh();
}

function buildParakeet(providerId: string, settings: Settings): HTMLElement {
  const wrapper = document.createElement("div");
  wrapper.className = "field";

  const config = providerSection(settings, providerId);
  wrapper.append(
    buildField(
      providerId,
      {
        key: "model_dir",
        label: "Model directory",
        type: "text",
        placeholder: "Default location",
        nullable: true,
      },
      config,
    ),
  );

  const status = textEl("p", "Checking model…", "model-status") as HTMLParagraphElement;
  const dir = textEl("p", "", "hint") as HTMLParagraphElement;
  const bar = document.createElement("div");
  bar.className = "progress";
  bar.hidden = true;
  const fill = document.createElement("div");
  fill.className = "progress-fill";
  bar.append(fill);

  const button = document.createElement("button");
  button.type = "button";
  button.className = "secondary";
  button.textContent = "Download model";

  wrapper.append(status, dir, bar, button);
  setupParakeet(providerId, { status, dir, bar, fill, button });
  return wrapper;
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
      card.append(buildParakeet(provider.id, settings));
    } else if (PROVIDER_FIELDS[provider.id]) {
      const config = providerSection(settings, provider.id);
      for (const spec of PROVIDER_FIELDS[provider.id]) {
        card.append(buildField(provider.id, spec, config));
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
  } catch {
    // Keep the previous list; a transient read failure is not fatal.
  }
}

let captureActive = false;

function stopCapture(): void {
  if (!captureActive) return;
  captureActive = false;
  els.capture.textContent = "Record keys";
  els.capture.classList.remove("capturing");
  window.removeEventListener("keydown", onCaptureKey, true);
}

function onCaptureKey(event: KeyboardEvent): void {
  event.preventDefault();
  event.stopPropagation();
  if (event.key === "Escape") {
    stopCapture();
    return;
  }
  if (MODIFIER_CODES.has(event.code)) return;
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
  captureActive = true;
  els.capture.textContent = "Press keys… (Esc cancels)";
  els.capture.classList.add("capturing");
  window.addEventListener("keydown", onCaptureKey, true);
}

async function save(): Promise<void> {
  const settings = state.settings;
  if (!settings) return;
  const delay = Number.parseInt(els.delay.value, 10);
  if (!Number.isFinite(delay) || delay < 50 || delay > 2000) {
    setStatus("Restore delay must be between 50 and 2000 ms.", "error");
    return;
  }
  settings.restore_delay_ms = delay;
  els.save.disabled = true;
  setStatus("Saving…");
  try {
    const result = await invoke<SaveResult>("save_settings", { settings });
    state.baseline = JSON.stringify(settings);
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
    updateDirty();
  }
}

function wireForm(): void {
  els.hotkey.addEventListener("input", () => {
    if (!state.settings) return;
    state.settings.hotkey = els.hotkey.value.trim();
    updateDirty();
  });
  els.delay.addEventListener("input", () => {
    if (!state.settings) return;
    const value = Number.parseInt(els.delay.value, 10);
    if (Number.isFinite(value)) state.settings.restore_delay_ms = value;
    updateDirty();
  });
  els.capture.addEventListener("click", startCapture);
  els.form.addEventListener("submit", (event) => {
    event.preventDefault();
    void save();
  });
}

async function main(): Promise<void> {
  wireForm();
  try {
    const [settings, providers] = await Promise.all([
      invoke<Settings>("get_settings"),
      invoke<ProviderInfo[]>("list_providers"),
    ]);
    state.settings = settings;
    state.providers = providers;
    state.baseline = JSON.stringify(settings);
    els.hotkey.value = settings.hotkey;
    els.delay.value = String(settings.restore_delay_ms);
    renderProviderRadios();
    renderProviderConfig();
    updateDirty();
  } catch (error) {
    els.loadError.hidden = false;
    els.loadError.textContent = `Could not load settings: ${String(error)}`;
    els.save.disabled = true;
  }
}

void main();
