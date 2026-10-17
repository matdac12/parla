/** Shared by the app window's Home, History and Timings pages. */
export interface DictationRecord {
  id: string;
  kind: string;
  started_at: number;
  provider_id: string;
  /** Stage offsets in ms from key-up (see `timing::DictationRecord`). */
  key_up: number;
  audio_ready: number | null;
  provider_returned: number | null;
  pasted: number | null;
  audio_ms: number;
  text: string;
  error: string | null;
}

export function must<T extends HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (element === null) throw new Error(`missing #${id}`);
  return element as T;
}

export function el<K extends keyof HTMLElementTagNameMap>(tag: K, className?: string, text?: string): HTMLElementTagNameMap[K] {
  const element = document.createElement(tag);
  if (className !== undefined) element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
}

/** Makes `parent`'s children exactly `nodes`, moving only what changed, so
 *  cached rows are not detached and re-attached on every render. */
export function reconcile(parent: Node, nodes: Node[]): void {
  let cursor = parent.firstChild;
  for (const node of nodes) {
    if (node === cursor) cursor = cursor.nextSibling;
    else parent.insertBefore(node, cursor);
  }
  while (cursor) {
    const next = cursor.nextSibling;
    cursor.remove();
    cursor = next;
  }
}

const PROVIDERS: Record<string, string> = { openai: "OpenAI", xai: "xAI Grok", parakeet: "Parakeet" };

export function providerName(id: string): string {
  return PROVIDERS[id] ?? id;
}

export function seconds(ms: number): string {
  return `${(ms / 1000).toFixed(1)} s`;
}

// One formatter each: toLocale*String with options builds a new one per call.
const timeFormat = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit" });
const fullFormat = new Intl.DateTimeFormat(undefined, { dateStyle: "short", timeStyle: "medium" });

export function clock(ms: number): string {
  return timeFormat.format(ms);
}

export function fullDate(ms: number): string {
  return fullFormat.format(ms);
}
