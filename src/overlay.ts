import { listen } from "@tauri-apps/api/event";

const BARS = 12;
const pill = document.getElementById("pill") as HTMLDivElement;
const dot = document.createElement("div");
dot.className = "dot";
const container = document.createElement("div");
container.className = "bars";
pill.append(dot, container);

const bars: HTMLDivElement[] = [];
for (let i = 0; i < BARS; i++) {
  const bar = document.createElement("div");
  bar.className = "bar";
  container.appendChild(bar);
  bars.push(bar);
}

// Scrolling history of recent levels; speech RMS is small, so boost it.
const history = new Array<number>(BARS).fill(0);
void listen<number>("level", (event) => {
  history.shift();
  history.push(Math.min(1, Math.sqrt(event.payload) * 2.2));
  history.forEach((value, i) => {
    bars[i].style.height = `${4 + value * 22}px`;
    bars[i].style.opacity = String(0.45 + 0.55 * (i / (BARS - 1)));
  });
});
