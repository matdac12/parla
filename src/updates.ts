import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { must } from "./records";
import "./updates.css";

/** What `updater::Status` sends (see src-tauri/src/updater.rs). */
interface UpdateStatus {
  /** A public key is configured; without one every control is off. */
  enabled: boolean;
  current: string;
  portable: boolean;
  auto: boolean;
  /** Unix seconds. */
  last_check: number | null;
  /** The newest version, when it is newer than this one. */
  available: string | null;
  dismissed: boolean;
  error: string | null;
}

const els = {
  version: must<HTMLParagraphElement>("updates-version"),
  state: must<HTMLParagraphElement>("updates-state"),
  check: must<HTMLButtonElement>("updates-check"),
  act: must<HTMLButtonElement>("updates-act"),
  auto: must<HTMLInputElement>("updates-auto"),
  dot: must<HTMLSpanElement>("update-dot"),
  banner: must<HTMLDivElement>("update-banner"),
  bannerText: must<HTMLSpanElement>("update-banner-text"),
  bannerAct: must<HTMLButtonElement>("update-banner-act"),
  bannerLater: must<HTMLButtonElement>("update-banner-later"),
};

let current: UpdateStatus | null = null;
/** What to show instead of the status line: "Checking…", progress, a failure. */
let note = "";
let busy = false;

function describe(status: UpdateStatus): string {
  const { available } = status;
  if (!status.enabled) return "Updates are not set up in this build.";
  if (available) {
    return status.portable
      ? `Version ${available} is available. A portable copy cannot update itself: download the new zip from the release page.`
      : `Version ${available} is available.`;
  }
  if (status.error) return status.error;
  if (status.last_check === null) return "Parla has not checked yet.";
  return `Up to date. Last checked ${new Date(status.last_check * 1000).toLocaleString()}.`;
}

function render(status: UpdateStatus | null = current): void {
  if (!status) return;
  current = status;
  const { available } = status;
  els.version.textContent = `Parla ${status.current}${status.portable ? " (portable)" : ""}`;
  els.state.textContent = note || describe(status);
  els.check.disabled = busy || !status.enabled;
  els.auto.disabled = !status.enabled;
  els.auto.checked = status.auto;
  const actLabel = status.portable ? "Open the release page" : "Install and restart";
  els.act.hidden = !available;
  els.act.textContent = actLabel;
  els.act.disabled = busy;
  els.dot.hidden = !available;

  const showBanner = available !== null && !status.dismissed;
  els.banner.hidden = !showBanner;
  if (showBanner) {
    els.bannerText.textContent = status.portable
      ? `Parla ${available} is available.`
      : `Parla ${available} is ready to install. Parla restarts when it is done.`;
    els.bannerAct.textContent = status.portable ? "Release page" : "Install";
    els.bannerAct.disabled = busy;
  }
}

function say(text: string): void {
  note = text;
  render();
}

async function run(job: () => Promise<UpdateStatus | void>, working: string): Promise<void> {
  busy = true;
  say(working);
  let status: UpdateStatus | void = undefined;
  try {
    status = await job();
    note = "";
  } catch (error) {
    note = String(error);
  }
  busy = false;
  render(status || current);
}

/** Install, or on a portable copy open the release page. */
function act(): void {
  if (current?.portable) void invoke("open_release_page");
  else void run(() => invoke<void>("install_update"), "Downloading the update…");
}

export function mountUpdates(): void {
  els.check.addEventListener("click", () => {
    void run(() => invoke<UpdateStatus>("check_for_update"), "Checking…");
  });
  els.act.addEventListener("click", act);
  els.bannerAct.addEventListener("click", act);
  els.auto.addEventListener("change", () => {
    void invoke<UpdateStatus>("set_auto_update", { auto: els.auto.checked }).then(render);
  });
  els.bannerLater.addEventListener("click", () => {
    void invoke<UpdateStatus>("dismiss_update").then(render);
  });

  void listen<UpdateStatus>("update://changed", (event) => {
    note = "";
    render(event.payload);
  });
  void listen("update://waiting", () => say("Finishing your dictation…"));
  void listen<number>("update://progress", (event) => say(`Downloading the update… ${event.payload}%`));
  void invoke<UpdateStatus>("update_status").then(render);
}
