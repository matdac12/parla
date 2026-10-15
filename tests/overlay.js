import { mockIPC } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";

const html = new DOMParser().parseFromString(await (await fetch("/overlay.html")).text(), "text/html");
document.body.replaceChildren(...html.body.childNodes);
mockIPC(() => {}, { shouldMockEvents: true });
await import("/src/overlay.ts");
const settle = () => new Promise(resolve => setTimeout(resolve, 280));
await settle();
window.overlayTestResults = [];
const assert = (value, message) => { if (!value) throw new Error(message); };
async function check(name, test) {
  try { await test(); window.overlayTestResults.push({ name, passed: true }); }
  catch (error) { window.overlayTestResults.push({ name, passed: false, error: String(error) }); }
}
const pill = document.getElementById("pill");
await check("captions preserve the original pill dimensions and bottom anchor", async () => {
  await emit("caption-session", { generation: 1, enabled: true });
  await emit("phase", "recording");
  await settle();
  // A hidden browser throttles animation frames; inspect the settled geometry.
  pill.getAnimations().forEach(animation => animation.finish());
  const rect = pill.getBoundingClientRect();
  assert(Math.abs(rect.width - 116) < 0.1 && Math.abs(rect.height - 32) < 0.1, `Original pill dimensions changed: ${rect.width} x ${rect.height}`);
  assert(Math.abs(rect.bottom - (innerHeight - 4)) < 1, "Pill is not anchored 4 px above its canvas bottom");
  await emit("caption", { generation: 1, stable: "Ciao, ", provisional: "hello." });
  await settle();
  assert(pill.getBoundingClientRect().bottom === rect.bottom, "Caption moved the pill");
});
await check("caption companion is above the pill and provisional words are dimmer", async () => {
  const panel = document.getElementById("captions");
  assert(!panel.hidden, "Caption companion did not appear");
  assert(Math.abs(pill.getBoundingClientRect().top - panel.getBoundingClientRect().bottom - 8) < 1, "Caption gap is not 8 px");
  assert(getComputedStyle(panel.querySelector(".caption-provisional")).color !== getComputedStyle(panel.querySelector(".caption-stable")).color, "Provisional words are not differentiated");
  assert(getComputedStyle(panel).pointerEvents === "none", "Captions intercept the mouse");
});
await check("long previews retain the latest words within two lines", async () => {
  await emit("caption", { generation: 1, stable: "previous words ".repeat(30), provisional: "the latest words" });
  await settle();
  const panel = document.getElementById("captions");
  const text = panel.querySelector(".caption-text");
  assert(text.clientHeight <= 38, "Caption exceeded two lines");
  assert(text.scrollTop + text.clientHeight >= text.scrollHeight - 1, "Latest words scrolled out of view");
});
await check("transcript markup stays inert and revised text replaces the old preview", async () => {
  await emit("caption", { generation: 1, stable: "", provisional: '<img src=x onerror="window.captionInjected=true">' });
  await settle();
  const panel = document.getElementById("captions");
  assert(panel.querySelector("img") === null && !window.captionInjected, "Transcript injected markup");
  await emit("caption", { generation: 1, stable: "", provisional: "Corrected phrase" });
  await settle();
  assert(panel.textContent.trim() === "Corrected phrase", "Revision appended instead of replacing");
});
await check("old sessions cannot revive captions and disabling captions keeps streaming UI quiet", async () => {
  await emit("caption-session", { generation: 2, enabled: false });
  await emit("caption", { generation: 1, stable: "stale", provisional: "" });
  await emit("caption", { generation: 2, stable: "disabled", provisional: "" });
  await settle();
  assert(document.getElementById("captions").hidden, "Disabled or stale captions reappeared");
});
await check("captions remain during finalization and clear after paste, error or cancellation", async () => {
  for (const ending of ["done", "error", "idle"]) {
    await emit("caption-session", { generation: 3, enabled: true });
    await emit("phase", "recording");
    await emit("caption", { generation: 3, stable: "Hello", provisional: "" });
    await emit("phase", "processing");
    await settle();
    assert(!document.getElementById("captions").hidden, "Captions vanished during finalization");
    await emit("phase", ending);
    await settle();
    assert(document.getElementById("captions").hidden, `Captions survived ${ending}`);
  }
});
window.overlayTestsDone = true;
// Keep the final visual available for inspection, without adding test UI to the overlay.
await emit("caption-session", { generation: 4, enabled: true });
await emit("phase", "recording");
await emit("caption", { generation: 4, stable: "Vorrei condividere un aggiornamento. ", provisional: "The main priority is to finish this week." });
await emit("level", 0.15);
