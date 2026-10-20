// Run with Vite at /tests/frontend.html. Uses the actual app DOM and modules;
// only desktop IPC/clipboard boundaries are replaced by Tauri's official mocks.
import { mockIPC } from "@tauri-apps/api/mocks";
import { emit as emitEvent } from "@tauri-apps/api/event";

const html = new DOMParser().parseFromString(await (await fetch("/app.html")).text(), "text/html");
document.body.replaceChildren(...html.body.childNodes);
const records = Array.from({ length: 2000 }, (_, i) => ({
  id: `saved-${i}`, kind: "dictation", started_at: Date.now() - i * 1000,
  provider_id: "openai", audio_ms: 2000, text: `Saved transcript ${i}`, error: null,
}));
let savedSettings;
let selectedProvider = "openai";
let rejectProviderSelection = false;
let holdHistory = false;
let finishHistory;
async function emit(name, payload) {
  if (name === "dictation://recorded") records.unshift(payload);
  await emitEvent(name, payload);
}
mockIPC((command, args) => {
  switch (command) {
    case "get_settings": return {
      active_provider: "openai", hotkey: "Ctrl+Alt+Space", restore_delay_ms: 250, paste_compatibility_mode: false,
      auto_send: false, safety_net: false, streaming_enabled: true, live_captions: true, local_model: "",
      dim_other_audio: false, dim_audio_level: 50,
      command: { enabled: false, hotkey: "LCtrl+Shift", model: "", reasoning_effort: "none", reminders: false },
      providers: {},
    };
    case "list_providers": return [{ id: "openai", display_name: "OpenAI", available: true, reason: null }, { id: "xai", display_name: "xAI Grok", available: true, reason: null }];
    case "select_provider": {
      if (rejectProviderSelection) throw new Error("disk full");
      selectedProvider = args.id;
      return null;
    }
    case "get_history": {
      const snapshot = records.slice(0, args.limit);
      return holdHistory ? new Promise((resolve) => { finishHistory = () => { holdHistory = false; resolve(snapshot); }; }) : snapshot;
    }
    case "has_failed_clip": return false;
    case "clear_history": records.length = 0; return null;
    case "check_command_hotkey": return null;
    case "get_stats": return { total_words: 100, dictations: 10, audio_ms: (4 * 60 + 12) * 60_000, first_used_at: Date.now() - 30 * 86_400_000, cost_usd: 0.84, unpriced_audio_ms: 0, unpriced_llm_tokens: 0 };
    case "save_settings": savedSettings = structuredClone(args.settings); return { warning: null };
    default: throw new Error(`Unexpected IPC: ${command}`);
  }
}, { shouldMockEvents: true });
Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText: async () => {} } });
const initialSettings = new URLSearchParams(location.search).has("settings");
location.hash = initialSettings ? "settings" : "home";
await import("../app.ts");
const settle = () => new Promise((resolve) => setTimeout(resolve, 100));
await settle();
window.frontendTestResults = [];
async function check(name, test) {
  try { await test(); window.frontendTestResults.push({ name, passed: true }); }
  catch (error) { window.frontendTestResults.push({ name, passed: false, error: String(error) }); }
}
function assert(value, message) { if (!value) throw new Error(message); }
// History rows sit in per-day lists inside #days; Home shows only the newest five.
const days = document.getElementById("days");
const rows = () => days.querySelectorAll("li");
const firstRow = () => days.querySelector("li");
const go = (page) => document.querySelector(`.nav [data-page="${page}"]`).click();
await check("initial tray Settings route opens the first settings page", () => {
  if (!initialSettings) return;
  assert(!document.getElementById("page-transcription").hidden, "Settings route was missed");
  assert(!document.getElementById("settings-form").hidden, "Settings form stayed hidden");
  assert(document.getElementById("page-home").hidden, "Home shown alongside Settings");
});
go("history");
await settle();
await check("usage cards show audio time, cost and a projection after a week", async () => {
  const text = (id) => document.getElementById(id).textContent;
  assert(text("stat-words") === "100" && text("stat-dictations") === "10", "Counters missing");
  assert(text("stat-audio") === "4 h 12 m", `Audio time was ${text("stat-audio")}`);
  assert(text("stat-cost") === "$0.84", `Cost was ${text("stat-cost")}`);
  assert(document.getElementById("stat-cost-note").hidden, "Unpriced note shown with no unpriced audio");
  assert(text("stat-projected").includes("$10.22/yr"), `Projection was ${text("stat-projected")}`);
  assert(text("stat-projected").includes("$144"), "Wispr Flow comparison missing");
  await emit("stats://updated", { total_words: 3, dictations: 1, audio_ms: 42_000, first_used_at: Date.now() - 86_400_000, cost_usd: 0.002, unpriced_audio_ms: 90_000 });
  await settle();
  assert(text("stat-audio") === "42 s", `Short audio was ${text("stat-audio")}`);
  assert(text("stat-cost") === "< $0.01", `Tiny cost was ${text("stat-cost")}`);
  assert(!document.getElementById("stat-cost-note").hidden && text("stat-cost-note").includes("2 m"), "Unpriced note missing");
  assert(text("stat-projected").startsWith("yearly estimate from "), `Projection before a week of use was ${text("stat-projected")}`);
});
await check("new dictation preserves existing rows and Copy feedback", async () => {
  const existing = firstRow();
  const copy = existing.querySelector("button");
  copy.click();
  await settle();
  await emit("dictation://recorded", { ...records[0], id: "live-1", text: "New words" });
  await settle();
  assert(days.contains(existing), "Existing row was rebuilt");
  assert(copy.textContent === "Copied", "Copy feedback was reset");
  assert(firstRow().textContent.includes("New words"), "New row missing");
  assert(rows().length === 200, "Page limit was not preserved");
});
await check("unchanged retry availability does not rebuild rows", async () => {
  const existing = firstRow();
  await emit("failure://changed", false);
  await settle();
  assert(firstRow() === existing, "Retry event rebuilt unchanged rows");
});
await check("hidden history defers DOM updates and catches up on return", async () => {
  const existing = firstRow();
  Object.defineProperty(document, "hidden", { configurable: true, value: true });
  document.dispatchEvent(new Event("visibilitychange"));
  await emit("dictation://recorded", { ...records[0], id: "live-hidden", text: "Received while hidden" });
  await settle();
  assert(firstRow() === existing, "Hidden window rendered an update");
  Object.defineProperty(document, "hidden", { configurable: true, value: false });
  document.dispatchEvent(new Event("visibilitychange"));
  await settle();
  assert(firstRow().textContent.includes("Received while hidden"), "Deferred update was lost");
});
await check("search keeps matching row identity and handles no matches", async () => {
  const existing = firstRow();
  const search = document.getElementById("search");
  search.value = "Received while";
  search.dispatchEvent(new Event("input"));
  await settle();
  assert(firstRow() === existing && rows().length === 1, "Search rebuilt or lost its matching row");
  search.value = "no-such-transcript";
  search.dispatchEvent(new Event("input"));
  await settle();
  assert(rows().length === 0 && !document.getElementById("empty").hidden, "Empty search state is incorrect");
  search.value = "";
  search.dispatchEvent(new Event("input"));
  await settle();
});
await check("history stays deferred while browsing Settings", async () => {
  go("transcription");
  const existing = firstRow();
  await emit("dictation://recorded", { ...records[0], id: "live-settings", text: "During settings" });
  await settle();
  assert(firstRow() === existing, "History rendered while Settings was open");
  go("history");
  await settle();
  assert(firstRow().textContent.includes("During settings"), "Returning to History lost a live update");
});
await check("history reads do not overwrite dictations arriving during the read", async () => {
  holdHistory = true;
  document.getElementById("more").click();
  await emit("dictation://recorded", { ...records[0], id: "live-race", text: "Arrived during read" });
  finishHistory();
  await settle();
  assert(firstRow().textContent.includes("Arrived during read"), "History read overwrote the live dictation");
  assert(rows().length === 400, "Expanded page limit was not preserved");
});
await check("a full 2000-row history only adds one DOM row per dictation", async () => {
  while (!document.getElementById("more").hidden) {
    document.getElementById("more").click();
    await settle();
  }
  assert(rows().length === 2000, "Full history page was not loaded");
  const retained = firstRow();
  let added = 0;
  const observer = new MutationObserver((mutations) => {
    for (const mutation of mutations) for (const node of mutation.addedNodes) if (node.nodeName === "LI") added++;
  });
  observer.observe(days, { childList: true, subtree: true });
  await emit("dictation://recorded", { ...records[0], id: "live-full", text: "Newest in full history" });
  await settle();
  observer.disconnect();
  assert(added === 1 && days.contains(retained), `Expected one inserted row, got ${added}`);
  assert(rows().length === 2000, "Full list exceeded its cap");
});
await check("provider choice saves immediately and preserves unrelated draft edits", async () => {
  go("transcription");
  const delay = document.getElementById("delay");
  delay.value = "350";
  delay.dispatchEvent(new Event("input"));
  document.querySelector('input[name="active_provider"][value="xai"]').click();
  await settle();
  assert(selectedProvider === "xai", "Provider choice was not persisted immediately");
  assert(!savedSettings, "Provider selection saved unrelated form edits");
  assert(delay.value === "350", "Provider selection discarded draft edits");
  rejectProviderSelection = true;
  document.querySelector('input[name="active_provider"][value="openai"]').click();
  await settle();
  assert(document.querySelector('input[name="active_provider"][value="xai"]').checked, "Failed provider save did not restore selection");
  assert(selectedProvider === "xai", "Failed provider save changed the active provider");
  rejectProviderSelection = false;
});
await check("compatibility preference saves without modifying language detection", async () => {
  go("clipboard");
  const checkbox = document.getElementById("paste-compatibility");
  checkbox.checked = true;
  checkbox.dispatchEvent(new Event("change"));
  for (const id of ["streaming-enabled", "live-captions"]) {
    const control = document.getElementById(id);
    assert(control.checked, `${id} did not load its default`);
    control.checked = false;
    control.dispatchEvent(new Event("change"));
  }
  document.getElementById("settings-form").requestSubmit();
  await settle();
  assert(savedSettings?.paste_compatibility_mode === true, "Compatibility preference was not saved");
  assert(savedSettings.streaming_enabled === false && savedSettings.live_captions === false, "Live preferences were not saved independently");
  assert(!savedSettings.providers.openai?.language, "Language detection was changed");
});
// Only this window clears history since the History window was retired
// (PARLA-59), so the check clears it here.
await check("clearing history removes every row and shows the empty state", async () => {
  go("history");
  await settle();
  document.getElementById("clear").click();
  document.getElementById("confirm-clear").click();
  await settle();
  assert(records.length === 0, "History file was not cleared");
  assert(rows().length === 0, "Deleted transcripts remained visible");
  assert(!document.getElementById("empty").hidden, "Cleared history empty state was not shown");
});
await check("vocabulary edits save and unsafe entries are rejected", async () => {
  go("vocabulary");
  const vocabulary = document.getElementById("vocabulary");
  assert(vocabulary.value === "", "Old settings should load empty hints");
  vocabulary.value = "Example product\nAPI";
  vocabulary.dispatchEvent(new Event("input"));
  document.getElementById("settings-form").requestSubmit();
  await settle();
  assert(savedSettings.vocabulary.join("|") === "Example product|API", "Vocabulary did not save");
  vocabulary.value = "<unsafe>";
  vocabulary.dispatchEvent(new Event("input"));
  document.getElementById("settings-form").requestSubmit();
  await settle();
  assert(savedSettings.vocabulary[0] === "Example product", "Unsafe vocabulary was submitted");
  assert(document.getElementById("status").classList.contains("status-error"), "Validation error missing");
});
const report = document.createElement("pre");
report.id = "test-results";
report.textContent = JSON.stringify(window.frontendTestResults, null, 2);
document.body.append(report);
window.frontendTestsDone = true;
