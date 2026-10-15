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
let loads = 0;
let holdHistory = false;
let finishHistory;
async function emit(name, payload) {
  if (name === "dictation://recorded") records.unshift(payload);
  await emitEvent(name, payload);
}
mockIPC((command, args) => {
  switch (command) {
    case "get_settings": return { active_provider: "openai", hotkey: "Ctrl+Alt+Space", restore_delay_ms: 250, paste_compatibility_mode: false, streaming_enabled: true, live_captions: true, providers: {} };
    case "list_providers": return [{ id: "openai", display_name: "OpenAI", available: true, reason: null }, { id: "xai", display_name: "xAI Grok", available: true, reason: null }];
    case "select_provider": {
      if (rejectProviderSelection) throw new Error("disk full");
      selectedProvider = args.id;
      return null;
    }
    case "get_history": {
      loads++;
      const snapshot = records.slice(0, args.limit);
      return holdHistory ? new Promise((resolve) => { finishHistory = () => { holdHistory = false; resolve(snapshot); }; }) : snapshot;
    }
    case "has_failed_clip": return false;
    case "get_stats": return { total_words: 100, dictations: 10 };
    case "save_settings": savedSettings = structuredClone(args.settings); return { warning: null };
    default: throw new Error(`Unexpected IPC: ${command}`);
  }
}, { shouldMockEvents: true });
Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText: async () => {} } });
const initialSettings = new URLSearchParams(location.search).has("settings");
location.hash = initialSettings ? "settings" : "home";
await import("/src/app.ts");
const settle = () => new Promise((resolve) => setTimeout(resolve, 100));
await settle();
window.frontendTestResults = [];
async function check(name, test) {
  try { await test(); window.frontendTestResults.push({ name, passed: true }); }
  catch (error) { window.frontendTestResults.push({ name, passed: false, error: String(error) }); }
}
function assert(value, message) { if (!value) throw new Error(message); }
const list = document.getElementById("list");
await check("initial tray Settings route is honored without loading history", () => {
  if (!initialSettings) return;
  assert(!document.getElementById("view-settings").hidden, "Settings route was missed");
  assert(loads === 0, "History loaded despite starting in Settings");
});
document.getElementById("back").click();
await settle();
await check("new dictation preserves existing rows and Copy feedback", async () => {
  const existing = list.firstElementChild;
  const copy = existing.querySelector("button");
  copy.click();
  await settle();
  await emit("dictation://recorded", { ...records[0], id: "live-1", text: "New words" });
  await settle();
  assert(existing.parentElement === list, "Existing row was rebuilt");
  assert(copy.textContent === "Copied", "Copy feedback was reset");
  assert(list.firstElementChild.textContent.includes("New words"), "New row missing");
  assert(list.childElementCount === 200, "Page limit was not preserved");
});
await check("unchanged retry availability does not rebuild rows", async () => {
  const existing = list.firstElementChild;
  await emit("failure://changed", false);
  await settle();
  assert(list.firstElementChild === existing, "Retry event rebuilt unchanged rows");
});
await check("hidden history defers DOM updates and catches up on return", async () => {
  const existing = list.firstElementChild;
  Object.defineProperty(document, "hidden", { configurable: true, value: true });
  document.dispatchEvent(new Event("visibilitychange"));
  await emit("dictation://recorded", { ...records[0], id: "live-hidden", text: "Received while hidden" });
  await settle();
  assert(list.firstElementChild === existing, "Hidden window rendered an update");
  Object.defineProperty(document, "hidden", { configurable: true, value: false });
  document.dispatchEvent(new Event("visibilitychange"));
  await settle();
  assert(list.firstElementChild.textContent.includes("Received while hidden"), "Deferred update was lost");
});
await check("search keeps matching row identity and handles no matches", async () => {
  const existing = list.firstElementChild;
  const search = document.getElementById("search");
  search.value = "Received while";
  search.dispatchEvent(new Event("input"));
  await settle();
  assert(list.firstElementChild === existing && list.childElementCount === 1, "Search rebuilt or lost its matching row");
  search.value = "no-such-transcript";
  search.dispatchEvent(new Event("input"));
  await settle();
  assert(list.childElementCount === 0 && !document.getElementById("empty").hidden, "Empty search state is incorrect");
  search.value = "";
  search.dispatchEvent(new Event("input"));
  await settle();
});
await check("history stays deferred while browsing Settings", async () => {
  document.getElementById("open-settings").click();
  const existing = list.firstElementChild;
  await emit("dictation://recorded", { ...records[0], id: "live-settings", text: "During settings" });
  await settle();
  assert(list.firstElementChild === existing, "Home rendered while Settings was open");
  document.getElementById("back").click();
  await settle();
  assert(list.firstElementChild.textContent.includes("During settings"), "Returning home lost a live update");
});
await check("history reads do not overwrite dictations arriving during the read", async () => {
  holdHistory = true;
  document.getElementById("more").click();
  await emit("dictation://recorded", { ...records[0], id: "live-race", text: "Arrived during read" });
  finishHistory();
  await settle();
  assert(list.firstElementChild.textContent.includes("Arrived during read"), "History read overwrote the live dictation");
  assert(list.childElementCount === 400, "Expanded page limit was not preserved");
});
await check("a full 2000-row history only adds one DOM row per dictation", async () => {
  while (!document.getElementById("more").hidden) {
    document.getElementById("more").click();
    await settle();
  }
  assert(list.childElementCount === 2000, "Full history page was not loaded");
  const retained = list.firstElementChild;
  let added = 0;
  const observer = new MutationObserver((mutations) => {
    added += mutations.reduce((count, mutation) => count + mutation.addedNodes.length, 0);
  });
  observer.observe(list, { childList: true });
  await emit("dictation://recorded", { ...records[0], id: "live-full", text: "Newest in full history" });
  await settle();
  observer.disconnect();
  assert(added === 1 && retained.parentElement === list, `Expected one inserted row, got ${added}`);
  assert(list.childElementCount === 2000, "Full list exceeded its cap");
});
await check("provider choice saves immediately and preserves unrelated draft edits", async () => {
  document.getElementById("open-settings").click();
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
  document.getElementById("open-settings").click();
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
await check("returning home refreshes history cleared in another window", async () => {
  records.length = 0;
  document.getElementById("back").click();
  await settle();
  assert(list.childElementCount === 0, "Deleted transcripts remained visible");
  assert(!document.getElementById("empty").hidden, "Cleared history empty state was not shown");
});
await check("vocabulary edits save and unsafe entries are rejected", async () => {
  document.getElementById("open-settings").click();
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
