// The setup wizard against mocked IPC, to look at it and click through it in a
// browser without Parla running: `npm run dev`, then open
// http://localhost:1420/tests/setup.html#first (a new install) or #again.
// ?shortcut puts a Parla.lnk in the mocked Startup folder (Mattia's machine);
// ?keyset has an OpenAI key already; ?local starts on the local engine.
// ?step=N clicks Continue N times first (for screenshots), ?said=text pastes
// that into the Try it box as a finished dictation. Every call is kept in
// window.ipcCalls.
import { mockIPC } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";

const params = new URLSearchParams(location.search);
const html = new DOMParser().parseFromString(await (await fetch("/setup.html")).text(), "text/html");
document.body.replaceChildren(...html.body.childNodes);

const settings = {
  active_provider: params.has("local") ? "parakeet" : "openai",
  hotkey: "Ctrl+Alt+Space",
  local_model: "parakeet-v3",
  providers: params.has("keyset") ? { openai: { api_key: "sk-test" } } : {},
};
const models = [{ id: "parakeet-v3", name: "Parakeet v3", size: 670619706, model_dir: "x", available: params.has("local"), downloading: false, selected: true }];
const autostart = { supported: true, enabled: false, stale: false, command: null, shortcut: params.has("shortcut") ? "Parla.lnk" : null };
let metering;

window.ipcCalls = [];
mockIPC(async (cmd, args) => {
  window.ipcCalls.push({ cmd, args });
  switch (cmd) {
    case "get_settings": return structuredClone(settings);
    case "save_settings": Object.assign(settings, args.settings); return { warning: null };
    case "select_provider": settings.active_provider = args.id; return;
    case "list_providers":
      return [
        { id: "openai", display_name: "OpenAI", available: params.has("keyset") || Boolean(settings.providers.openai?.api_key), reason: "OpenAI API key is not set" },
        { id: "xai", display_name: "xAI Grok", available: false, reason: "xAI API key is not set" },
        { id: "parakeet", display_name: "Parakeet (local)", available: models[0].available, reason: "Model not downloaded" },
      ].map((p) => ({ ...p, reason: p.available ? null : p.reason }));
    case "list_models": return structuredClone(models);
    case "select_model":
      models[0].downloading = true;
      for (let percent = 10; percent <= 100; percent += 15) {
        await new Promise((resolve) => setTimeout(resolve, 250));
        await emit("model://progress", { id: args.id, downloaded: Math.min(percent, 100), total: 100 });
      }
      models[0].downloading = false;
      models[0].available = true;
      return args.id;
    case "mic_info": return "Microphone Array (Intel Smart Sound Technology)";
    case "mic_meter_start":
      clearInterval(metering);
      metering = setInterval(() => emit("setup://level", 0.004 + Math.random() * 0.03), 80);
      return;
    case "mic_meter_stop": clearInterval(metering); return;
    case "autostart_status": return structuredClone(autostart);
    case "set_autostart":
      if (args.enabled && autostart.shortcut) throw new Error("Parla.lnk in your Startup folder already starts Parla at login. Delete it first, or Parla would start twice.");
      autostart.enabled = args.enabled;
      return structuredClone(autostart);
    default: return;
  }
}, { shouldMockEvents: true });

await import("../setup.ts");

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const steps = Number(params.get("step") ?? 0);
for (let i = 0; i < steps; i++) {
  await wait(300);
  document.getElementById("next").click();
}
if (params.has("said")) {
  await wait(300);
  document.getElementById("try-box").value = params.get("said");
  await emit("dictation://recorded", { kind: "dictation", error: null, pasted: 840 });
}
