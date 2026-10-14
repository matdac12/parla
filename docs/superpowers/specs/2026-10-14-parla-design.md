# Parla — v0 design

## Goal

A push-to-talk dictation app for Windows where speed is the first priority and accuracy the second. Hold a hotkey, speak, release, and the transcript is pasted into the focused app. v0 must work end to end and make it easy to compare speech-to-text providers (local and cloud) on latency, so we can find the fastest one for daily use.

## Decisions already made

- Windows is the primary target. Linux/macOS are not goals.
- Speed first, accuracy second. Privacy and offline use are not priorities.
- No streaming. Record while the key is held, transcribe the whole clip on release.
- No transcript cleanup (filler removal, rewriting). Output is the raw transcript.
- Cost is not a constraint: ~72 h of speech per year is $3–$40 per year on any candidate cloud provider, against the current $144 per year.
- Providers are swappable, local and cloud side by side.

## Non-goals for v0

Streaming, LLM cleanup, per-app context, personal dictionary, voice commands, auto-update, installers beyond a plain build artifact, macOS/Linux support.

## Architecture

Tauri v2 app (Rust backend, small TypeScript/HTML settings UI), the same stack Handy uses. Rust owns everything on the hot path so the webview never sits between key release and paste.

Units, each with one purpose:

| Unit | Purpose | Depends on |
|---|---|---|
| `hotkey` | Global hold-to-talk shortcut; emits `Pressed` / `Released` | OS, Tauri global-shortcut plugin |
| `recorder` | Captures mic audio as 16 kHz mono PCM while held; stream is opened ahead of time so key-down has no startup delay | `cpal` |
| `provider` | `trait Stt { fn transcribe(&self, audio: &Clip) -> Result<String> }` plus one impl per provider | per-impl (HTTP client or local runtime) |
| `inserter` | Puts text into the focused app: set clipboard, send Ctrl+V, restore the previous clipboard after a short delay | `enigo` or `arboard` |
| `timing` | Records per-stage timestamps for every dictation and appends them to a local history | none |
| `bench` | Runs one saved clip through every enabled provider and reports latency and text side by side | `provider`, `timing` |
| `settings` | Persists chosen provider, API keys, hotkey; exposes them to the UI | disk |
| `ui` | Tray icon, settings window, history/benchmark view | `settings`, `timing`, `bench` |

### Dictation flow

1. Hotkey `Pressed` → recorder starts buffering. Tray icon shows recording.
2. Hotkey `Released` → recorder stops, returns a `Clip`.
3. Active provider transcribes the clip.
4. Inserter pastes the text.
5. Timing records: key-up, audio ready, provider returned, pasted.

### Providers

v0 ships with the following. Each one is a small file implementing the trait, so adding another later is cheap.

- **Local: Parakeet V3** (and optionally Whisper via GGML). Handy's author publishes a Rust crate for this (`transcribe-rs`); the implementation plan must confirm it is usable before depending on it.
- **Cloud: xAI Grok STT** (batch endpoint)
- **Cloud: OpenAI `gpt-transcribe`**
- **Cloud: Groq `whisper-large-v3-turbo`**

ElevenLabs Scribe is a candidate for a fast follow-up. It is not in v0 unless the first benchmark makes it look necessary.

Prices and latency claims in the research notes came from third-party sources and must be re-checked against provider docs when each provider is implemented.

### Benchmarking (the point of v0)

A "Benchmark" action records a clip, then runs it through every enabled provider and shows, per provider: time from request to text, total key-up-to-text time, and the transcript. Results are kept in the history view so runs can be compared over days and across networks. Real numbers on the user's own voice and connection replace the blog benchmarks.

### Error handling

- Provider failure (network, auth, timeout): show a tray notification with the reason; do not paste anything; keep the clip so it can be retried.
- Missing API key or model: the provider shows as unavailable in settings instead of failing at dictation time.
- Clipboard restore is best effort. The delay before restore is a setting, because Handy has a known bug where the old clipboard gets pasted under load.

### Testing

- Unit tests for the provider response parsing and the timing recorder (no network).
- Each provider has an integration test behind an env-var key that runs a short fixture clip.
- Manual smoke test on Windows for hotkey, recording, and paste into Notepad, a browser text field, and a terminal.

## Known risks

1. **Development happens on Linux; the target is Windows.** Windows builds will run in GitHub Actions on a Windows runner, and hotkey, audio and paste behavior must be tested by hand on a Windows machine. This is the largest risk to iteration speed.
2. **Paste reliability** (focus, clipboard races, elevated windows) is where dictation tools tend to break. It gets a dedicated spike early.
3. **Hold-to-talk global shortcuts** need key-up events; the Tauri plugin supports them, but Windows behavior must be verified.

## Prior art to borrow from

- **Handy** (github.com/cjpais/Handy): Tauri v2, activation modes, shortcuts, local Parakeet/Whisper integration, paste handling. Read it for how it solves hold/toggle shortcuts and the Windows quirks listed in its README.
- Other open-source dictation projects will be surveyed during planning for hotkey and paste handling.

## Definition of done for v0

On Windows: hold the hotkey, speak, release, and the text appears in the focused app, using either a local or a cloud provider chosen in settings. The benchmark view shows latency for every configured provider on the same clip.
