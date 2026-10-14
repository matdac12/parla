# Parla

Parla is a Windows push-to-talk dictation app focused on speed: hold a hotkey, speak, release, and paste the transcript into the focused app.
It will compare local and cloud speech-to-text providers by latency; see the [v0 design spec](docs/superpowers/specs/2026-10-14-parla-design.md). The core flow records the microphone, transcribes through OpenAI, and pastes on Windows; the settings/history/benchmark pages are placeholders.

## Windows prerequisites

- Install [Rust via rustup](https://rustup.rs/), using the stable MSVC toolchain `x86_64-pc-windows-msvc`.
- Install [Visual Studio 2022 Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with **Desktop development with C++**, including the Windows SDK and MSVC compiler.
- Install [Node.js LTS](https://nodejs.org/en/download) with npm (Node 24 is used in CI).
- Install the [Microsoft Edge WebView2 Evergreen Runtime](https://developer.microsoft.com/en-us/microsoft-edge/webview2/#download-section) on Windows 10. It is preinstalled on Windows 11.

See also [Tauri's prerequisites](https://v2.tauri.app/start/prerequisites/).

## Build and run

In PowerShell:

```powershell
git clone https://github.com/matdac6/parla
cd parla
npm ci
npm run tauri dev
```

The app starts in the tray. Choose **Settings…** to open or focus its window. Closing the window hides it; choose **Quit** in the tray menu to exit.

For a standalone executable, from the repository root:

```powershell
npm run tauri build -- --no-bundle
.\src-tauri\target\release\parla.exe
```

The build produces `src-tauri\target\release\parla.exe`. Or download the `parla-windows-x64` artifact from the latest green [GitHub Actions run](https://github.com/matdac6/parla/actions), extract it, and run `parla.exe`. WebView2 is still required; no installer is produced.

For frontend development alone: `npm run dev`. To type-check and build it: `npm run build`.

## API keys

Settings live at **`%APPDATA%\com.matdac6.parla\settings.json`** on Windows.
Create the directory/file before launching, or save through the settings commands
once the settings UI is available. Missing fields use defaults. Example:

```json
{
  "active_provider": "openai",
  "hotkey": "Ctrl+Space",
  "restore_delay_ms": 250,
  "providers": {
    "openai": {
      "api_key": "YOUR_OPENAI_API_KEY",
      "model": "gpt-transcribe",
      "language": null
    }
  }
}
```

`OPENAI_API_KEY` overrides the file's key, including an empty environment value.
For the current PowerShell session (then launch Parla from that session):

```powershell
$env:OPENAI_API_KEY = "YOUR_OPENAI_API_KEY"
.\src-tauri\target\release\parla.exe
```

Keys in the JSON file are stored as plaintext. Environment overrides are resolved
at request time and are never written to the file or returned by `get_settings`.
Leave `language` null for automatic detection, or use an ISO language code such
as `"it"`. Model names are configurable per provider.

Verified against official OpenAI documentation on 2026-10-15:
[`POST /v1/audio/transcriptions`](https://developers.openai.com/api/reference/resources/audio/subresources/transcriptions/methods/create)
accepts multipart `file` (16 kHz mono WAV here) and `model`, returning JSON with
`text`. The spec's [`gpt-transcribe`](https://developers.openai.com/api/docs/models/gpt-transcribe)
is now listed and remains the default, at an estimated
**[$0.0045 per audio minute](https://developers.openai.com/api/docs/pricing)**.
No local latency comparison has been run; the model setting allows benchmarking
against alternatives such as `gpt-4o-mini-transcribe` (estimated $0.003/min).
One shared rustls HTTP client reuses connections, with a 5 s connect timeout and
30 s total request timeout. Requests do not retry automatically.

The live integration test uses the committed speech fixture and only contacts
OpenAI when `OPENAI_API_KEY` is set. Run `cargo test --test openai_live -- --nocapture`
from `src-tauri/` to see the skip note when no key is available.

## Local model (Parakeet)

Model setup and download instructions will be added with the local provider.

## Hotkey

Hold **Ctrl+Space**, then release Space to finish. Releasing Ctrl before Space also works: the hold ends when Space comes up. Key-down starts microphone capture (including the recorder’s 150 ms pre-roll); Space-up stops it, transcribes with the selected provider, and pastes the raw transcript. New holds during recording finalization, transcription, or clipboard restoration are ignored through their release; nothing is queued. Each accepted hold snapshots settings, so saves affect the next dictation.

On Windows, a keyboard-only low-level hook observes key-down/key-up events and ignores injected input; Tauri's global-shortcut plugin reserves the accelerator so normal key handling does not consume it. `Hotkey::set_shortcut(&str)` supports runtime rebinding (for example `Ctrl+Shift+F9`); the settings UI comes later. Release the current hold before rebinding; press and release keys normally after app startup. Letters, digits, F1–F12, arrows, navigation, and common punctuation keys are supported; unsupported keys such as numpad and media keys are rejected. A failed registration retains the previous shortcut; if the default is taken at startup, Parla stays running with a disabled-hotkey tray tooltip so it can be rebound later; `Ctrl+V` is reserved for paste.

Paste runs on a background worker: save nonempty clipboard text, publish the transcript, wait 100 ms, reuse an already-held Ctrl (or synthesize it), clear only currently-down conflicting modifiers, send Ctrl+V with a 100 ms modifier hold, then wait **250 ms** (`DEFAULT_RESTORE_DELAY_MS`) before restoring the previous text. The JSON/commands configure that restore delay; settings UI comes later. Timing records capture injection completion before the restoration delay, but the worker remains busy until restoration finishes. Clipboard restore is best effort, retries transient read/write failures up to five times with 100 ms total backoff, and rechecks for different copied text before every write. Empty or non-text clipboards are not restored: the transcript remains and previous non-text formats are lost. Slow apps may require a longer restore delay.

## Benchmark

Provider comparison and history instructions will be added with benchmarking.

## Known limitations / untested on Windows

- Everything in this repo was built on Linux; Windows behaviour has only been compiled in CI, not run.
- This initial scaffold's Windows CI run is pending the orchestrator's push; no Windows build has been verified yet.
- Tray visibility, Settings window focus, hide-on-close, and Quit need a manual Windows smoke test.
- Hotkey release and paste have not been run on Windows. Verify Notepad, a browser text field, and Windows Terminal; try both release orders, a long hold, repeated short holds, and clipboard restoration under load. Try very rapid release/repress cycles too; event transitions use raw key-up notifications.
- A non-elevated app cannot inject paste into elevated/admin windows (Windows UIPI). Secure desktop and applications that intercept Ctrl+V are unsupported.
- The Windows hook is keyboard-only. A 100 ms watchdog (deferred during synthetic input) ends a hold if its key-up was missed; the hook refreshes after recovery and every five seconds while idle to recover silent timeout removal. Recovery refreshes modifier state without sampling our synthetic Ctrl hold. Hook permission, keyboard layouts, IME behavior, and lock/sleep recovery still need manual Windows checks. Rebinding briefly pauses hotkey dispatch.
- Windows Terminal must allow Ctrl+V paste. Ctrl+Space may conflict with an IME or another global shortcut; registration failure is reported during startup.
- Paste reuses Ctrl if it is already down, and injected cleanup cannot change the physical modifier model. Cleared Alt/Shift/Windows modifiers may appear released to other apps until pressed again; test both left and right Ctrl and repeated Space presses while continuing to hold Ctrl.
- Clipboard restoration uses a timed delay, not an acknowledgement from the receiving app. Clipboard managers or slow consumers can still cause stale/duplicate pastes. Restoration preserves text only, and comparing clipboard text does not detect copying identical text during the delay.
- Linux can compile and test the app, but insertion is a no-op returning an unsupported error; the fallback global shortcut plugin requires a supported desktop (Wayland is not a supported target).
- OpenAI dictation, microphone capture, and settings persistence are implemented. Local/xAI providers, settings UI, history persistence, retry UI, and benchmarks come in later tickets.
- Provider errors paste nothing. The last failed clip is retained only in memory; there is no notification/retry action yet. Empty audio/transcripts are treated as failures. Recorder startup failures are logged and emitted in the timing event without a clip.
- Settings are loaded at startup. Editing the JSON externally requires a restart; `save_settings` applies the hotkey and restore delay at runtime. A failed persisted hotkey registration leaves the default binding in place if available.
- Timing emits `dictation://recorded` and logs transcripts/errors; history is not persisted yet. Stage offsets are monotonic milliseconds from key-up; `started_at` is Unix milliseconds. `pasted` means input injection completed, not acknowledgement by the receiving app.
- Real API transcription and end-to-end Windows dictation still require a configured key and a manual smoke test. The default model follows the spec; its latency has not been compared on this machine.
