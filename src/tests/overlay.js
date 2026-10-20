import { mockIPC } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";

const html = new DOMParser().parseFromString(await (await fetch("/overlay.html")).text(), "text/html");
document.body.replaceChildren(...html.body.childNodes);
window.ipcCalls = [];
mockIPC((cmd, args) => { window.ipcCalls.push({ cmd, args }); }, { shouldMockEvents: true });
await import("../overlay.ts");
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
// The reminder Card (PARLA-88): Open and Undo, and a 5 s countdown ring.
const card = document.getElementById("card");
const root = document.querySelector(".overlay-root");
const ring = card.querySelector(".card-ring");
const reduced = matchMedia("(prefers-reduced-motion: reduce)").matches;
const reminder = {
  tag: "reminder",
  header: "ricordami di chiamare Luca domani alle 15",
  text: "Chiamare Luca @CRM",
  detail: "Sun 18/10 · 15:00",
  actions: ["open", "undo"],
  primary: "open",
};
const buttons = () => [...card.querySelectorAll(".card-foot button")];
const named = (element, name) => element.getAnimations({ subtree: true }).find(animation => animation.animationName === name);
await check("a reminder card shows its title, day and time, Open filled, Undo, and no Copy", async () => {
  await emit("card", { generation: 7, card: reminder, countdown: true });
  await settle();
  assert(!card.hidden, "The reminder Card did not open");
  assert(card.querySelector(".card-tag span").textContent === "Reminder", "Not tagged Reminder");
  assert(getComputedStyle(card.querySelector(".card-tag i")).backgroundColor === "rgb(255, 159, 67)", "The dot is not orange");
  assert(card.querySelector(".card-header").textContent === "“ricordami di chiamare Luca domani alle 15”", "The header is not the instruction");
  const body = card.querySelector(".card-body");
  const detail = card.querySelector(".card-detail");
  assert(body.textContent === "Chiamare Luca @CRM", "The body is not the title");
  assert(!detail.hidden && detail.textContent === "Sun 18/10 · 15:00", "No second line");
  assert(getComputedStyle(detail).color !== getComputedStyle(body).color, "The second line is not muted");
  const labels = buttons().map(button => `${button.textContent}${button.classList.contains("primary") ? "*" : ""}`);
  assert(JSON.stringify(labels) === JSON.stringify(["Open*", "Undo"]), `Buttons: ${labels}`);
  assert(card.querySelector(".card-close") !== null, "No ✕");
});
await check("the ring counts down 5 s beside the tag, still under reduced motion only as a plain wait", async () => {
  const countdown = named(ring, "ring-countdown");
  if (reduced) {
    assert(getComputedStyle(ring).display === "none" && countdown === undefined, "The ring animates under reduced motion");
  } else {
    assert(getComputedStyle(ring).display !== "none", "No ring");
    assert(countdown !== undefined && countdown.effect.getTiming().duration === 5000, "The ring does not count 5 s");
  }
});
await check("only its own generation's leave fades the card and the check", async () => {
  await emit("card-leave", 6);
  await settle();
  assert(!root.hasAttribute("data-leaving"), "Another session's timer faded the Card");
  await emit("card-leave", 7);
  await settle();
  assert(root.hasAttribute("data-leaving"), "The countdown's end did not start the leave");
  const fading = named(card, "card-leave");
  if (reduced) {
    assert(fading === undefined && named(pill, "pill-leave") === undefined, "The leave animates under reduced motion");
  } else {
    assert(fading !== undefined && fading.effect.getTiming().duration === 300, "The Card does not fade over 300 ms");
    assert(named(pill, "pill-leave") !== undefined, "The check does not go with the Card");
  }
  // A click can't race the close: the fading Card takes none.
  assert(getComputedStyle(card).pointerEvents === "none", "The fading Card still takes clicks");
  // Rust closes it, then hides the window: the faded check must not come back.
  await emit("card", null);
  await settle();
  assert(card.hidden && root.hasAttribute("data-leaving"), "Closing brought the faded check back");
  await emit("phase", "recording");
  await settle();
  assert(!root.hasAttribute("data-leaving"), "The next session's pill stayed faded");
});
await check("Undo stops the countdown, and a failed Undo gives the buttons back with no ring", async () => {
  await emit("card", { generation: 8, card: reminder, countdown: true });
  await settle();
  window.ipcCalls.length = 0;
  buttons()[1].click();
  await settle();
  const call = window.ipcCalls.find(c => c.cmd === "card_action");
  assert(call && call.args.generation === 8 && call.args.action === "undo", `Undo sent ${JSON.stringify(window.ipcCalls)}`);
  assert(buttons().every(button => button.disabled), "Buttons stayed live during the Undo");
  assert(getComputedStyle(ring).display === "none", "The ring kept counting during the Undo");
  // Rust's keep_card after a failure: the same Card, no countdown.
  await emit("card", { generation: 8, card: reminder, countdown: false });
  await settle();
  assert(!card.hidden && buttons().every(button => !button.disabled), "The buttons did not come back");
  assert(getComputedStyle(ring).display === "none", "The ring came back");
  buttons()[0].click();
  await settle();
  assert(window.ipcCalls.some(c => c.cmd === "card_action" && c.args.action === "open"), "Open was not sent");
  await emit("card", null);
});
await check("other cards have no ring and no second line", async () => {
  await emit("card", { generation: 9, card: { tag: "command", header: "traduci", text: "Hello", actions: ["copy"], primary: "copy" }, countdown: false });
  await settle();
  assert(getComputedStyle(ring).display === "none", "A command Card has a ring");
  assert(card.querySelector(".card-detail").hidden, "A command Card has a second line");
  await emit("card", null);
});
window.overlayEmit = emit;
window.overlayTestsDone = true;
// Keep the final visual available for inspection, without adding test UI to the overlay.
await emit("caption-session", { generation: 4, enabled: true });
await emit("phase", "recording");
await emit("caption", { generation: 4, stable: "Vorrei condividere un aggiornamento. ", provisional: "The main priority is to finish this week." });
await emit("level", 0.15);
