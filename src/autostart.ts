import { invoke } from "@tauri-apps/api/core";

/** `autostart_status` / `set_autostart` (PARLA-52): the HKCU Run value, plus
 *  any Parla shortcut in the Startup folder. Shared by Settings and the wizard. */
export interface AutostartStatus {
  supported: boolean;
  enabled: boolean;
  /** The Run value starts some other copy of Parla. */
  stale: boolean;
  command: string | null;
  /** File name of a Parla shortcut in the Startup folder. */
  shortcut: string | null;
}

export function readAutostart(): Promise<AutostartStatus> {
  return invoke<AutostartStatus>("autostart_status");
}

export function writeAutostart(enabled: boolean): Promise<AutostartStatus> {
  return invoke<AutostartStatus>("set_autostart", { enabled });
}

export function openStartupFolder(): Promise<void> {
  return invoke("open_startup_folder");
}

/** The controls that show the setting. */
export interface AutostartControls {
  box: HTMLInputElement;
  note: HTMLElement;
  /** "Open Startup folder", shown when a shortcut needs deleting. */
  folder: HTMLButtonElement;
  /** "Use this copy", shown when the entry starts another copy. */
  repair: HTMLButtonElement;
}

/** Shows `status`: the checkbox, and a note on what else starts Parla. The
 *  checkbox is locked while a Startup shortcut makes turning it on unsafe:
 *  that would start Parla twice. */
export function renderAutostart(controls: AutostartControls, status: AutostartStatus): void {
  const { box, note, folder, repair } = controls;
  box.checked = status.enabled;
  box.disabled = !status.supported || (status.shortcut !== null && !status.enabled);
  folder.hidden = status.shortcut === null;
  repair.hidden = !(status.enabled && status.stale);
  note.classList.remove("capture-hint");
  if (!status.supported) {
    note.textContent = "Only available on Windows.";
  } else if (status.shortcut !== null && status.enabled) {
    note.textContent = `Parla will start twice at login: this setting and “${status.shortcut}” in your Startup folder both start it. Delete the shortcut.`;
    note.classList.add("capture-hint");
  } else if (status.shortcut !== null) {
    note.textContent = `“${status.shortcut}” in your Startup folder already starts Parla at login. To manage it here, delete the shortcut, then turn this on.`;
  } else if (status.enabled && status.stale) {
    note.textContent = `Windows starts ${status.command ?? "another copy"}, not this copy of Parla.`;
    note.classList.add("capture-hint");
  } else {
    note.textContent = "Parla starts hidden in the tray when you sign in to Windows.";
  }
}
