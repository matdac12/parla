# Parla

Parla is a Windows push-to-talk dictation app focused on speed: hold a hotkey, speak, release, and paste the transcript into the focused app.
It will compare local and cloud speech-to-text providers by latency; see the [v0 design spec](docs/superpowers/specs/2026-10-14-parla-design.md). The core flow records the microphone, transcribes through OpenAI or xAI, and pastes on Windows; the settings/history/benchmark pages are placeholders.

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

The build leaves `parla.exe` and its required DLLs, including `DirectML.dll`, in
`src-tauri\target\release\`. Keep the executable and DLLs in the same folder;
copying only the executable will prevent startup. Or download the `parla-windows-x64` artifact from the latest green [GitHub Actions run](https://github.com/matdac6/parla/actions), extract the whole artifact into one folder, and run `parla.exe`. WebView2 is still required; no installer is produced.

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
    },
    "xai": {
      "api_key": "YOUR_XAI_API_KEY",
      "model": "grok-voice-transcribe-2.0",
      "language": null
    }
  }
}
```

`OPENAI_API_KEY` overrides the file's key, including an empty environment value.
Set `"active_provider": "xai"` to dictate through xAI. Its key can be overridden
with `XAI_API_KEY` instead, using the same rules.
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

### xAI Grok

Verified against official xAI documentation on 2026-10-15:
[`POST https://api.x.ai/v1/stt`](https://docs.x.ai/developers/rest-api-reference/inference/speech-to-text)
accepts multipart `file` (16 kHz mono WAV here, sent last) plus `model`, returning
JSON with `text`; the request shape and response come from the
[Speech to Text guide](https://docs.x.ai/developers/model-capabilities/audio/speech-to-text).
The default model is [`grok-voice-transcribe-2.0`](https://docs.x.ai/developers/model-capabilities/audio/speech-to-text)
(batch), at **[$0.10 per audio hour](https://docs.x.ai/developers/model-capabilities/audio/voice)**
(about $0.0017/min). A non-null `language` code is also sent as the `format=true`
option so spoken numbers and currencies are normalized. xAI's batch endpoint
supports up to 500 MB; the client sets a 5 s connect timeout and 30 s total
request timeout and does not retry automatically.

The live integration test only contacts xAI when `XAI_API_KEY` is set. Run
`cargo test --test xai_live -- --nocapture` from `src-tauri/` to see the skip note
when no key is available.

For the PARLA-8 UI, `invoke("save_settings", { settings })` resolves to
`{ "warning": null }` on success, or `{ "warning": "Settings saved, but hotkey was not applied: …" }`
when persistence succeeded but runtime rebinding failed. Show that warning without
reporting the save as failed: API keys/provider/delay and the requested hotkey are
already persisted. The previous runtime binding remains active (or the listener
stays unavailable). Only a changed hotkey is reapplied. Disk failures reject the
invoke and leave runtime/shared settings unchanged. `get_settings` continues to
return the complete saved settings object. Saves are serialized separately from
the shared settings lock; neither disk IO nor main-thread rebinding holds that lock.

The settings window (tray **Settings…**) lists every registered provider with its
availability, lets you pick the active one, and writes API keys and model names into
`providers.<id>`. Keys are stored as plaintext; the password field has a show/hide
toggle and never logs or echoes the value. Providers that are not ready are disabled
with the reason shown inline (for example, a missing key or an undownloaded model), and
the list refreshes after each save so typing a key enables its provider immediately.

## Hotkey

Hold **Ctrl+Space**, then release Space to finish. Releasing Ctrl before Space also works: the hold ends when Space comes up. Key-down starts microphone capture (including the recorder’s 150 ms pre-roll); Space-up stops it, transcribes with the selected provider, and pastes the raw transcript. New holds during recording finalization, transcription, or clipboard restoration are ignored through their release; nothing is queued. Each accepted hold snapshots settings, so saves affect the next dictation. If no release arrives within 120 seconds after recording starts, Parla stops and transcribes the captured clip as a forced release, records `hold exceeded 120 s`, then frees the session after transcription/paste finishes. A later press can recover the stale hold. A disconnected release channel also forces completion.

On Windows, a keyboard-only low-level hook observes key-down/key-up events and ignores injected input; Tauri's global-shortcut plugin reserves the accelerator so normal key handling does not consume it. `Hotkey::set_shortcut(&str)` supports runtime rebinding (for example `Ctrl+Shift+F9`); the settings UI comes later. Release the current hold before rebinding; press and release keys normally after app startup. Letters, digits, F1–F12, arrows, navigation, and common punctuation keys are supported; unsupported keys such as numpad and media keys are rejected. A failed registration retains the previous shortcut; if the default is taken at startup, Parla stays running with a disabled-hotkey tray tooltip so it can be rebound later; `Ctrl+V` is reserved for paste.

Paste runs on a background worker: save nonempty clipboard text, publish the transcript, wait 100 ms, reuse an already-held Ctrl (or synthesize it), clear only currently-down conflicting modifiers, send Ctrl+V with a 100 ms modifier hold, then wait **250 ms** (`DEFAULT_RESTORE_DELAY_MS`) before restoring the previous text. The JSON/commands configure that restore delay; settings UI comes later. Timing records capture injection completion before the restoration delay, but the worker remains busy until restoration finishes. Clipboard restore is best effort, retries transient read/write failures up to five times with 100 ms total backoff, and rechecks for different copied text before every write. Empty or non-text clipboards are not restored: the transcript remains and previous non-text formats are lost. Slow apps may require a longer restore delay.

The settings window's **Hotkey** field accepts accelerators such as `Ctrl+Space` or
`Ctrl+Shift+F9`; **Record keys** captures one by holding the combination you want (Esc
cancels). Saving rebinds immediately, without a restart. An accelerator that cannot be
registered (for example the reserved `Ctrl+V`) leaves the previous binding active and is
reported inline as a warning rather than failing the save.

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
- OpenAI and xAI dictation, microphone capture, and settings persistence are implemented. The local provider, settings UI, history persistence, retry UI, and benchmarks come in later tickets.
- Provider errors paste nothing. The last failed clip is retained only in memory; there is no notification/retry action yet. Empty audio/transcripts are treated as failures. Recorder startup failures are logged and emitted in the timing event without a clip.
- Settings are loaded at startup. Editing the JSON externally requires a restart; `save_settings` persists settings first, applies a changed hotkey with a nonfatal warning on failure, and uses the restore delay for the next dictation. A failed persisted hotkey registration leaves the default binding in place if available.
- Timing emits `dictation://recorded` and logs stage timings, transcript length and failure status only (no transcript or raw provider error body); history is not persisted yet. Stage offsets are monotonic milliseconds from key-up; `started_at` is Unix milliseconds. `pasted` means input injection completed, not acknowledgement by the receiving app.
- Real API transcription and end-to-end Windows dictation still require a configured key and a manual smoke test. The default model follows the spec; its latency has not been compared on this machine.

## Local model (Parakeet)

Choose **Parakeet V3 (local)** in Settings and use **Download Parakeet model**
(the settings UI is supplied by PARLA-8). The one-time download requires internet;
afterwards transcription stays on your CPU and needs no API key or network.
Until all four files are present with the expected sizes the provider is unavailable.
Downloads verify exact sizes and SHA-256 hashes in a sibling temporary directory,
then publish the completed directory by rename. Failed downloads can be retried;
partial downloads are discarded rather than resumed. The download command also
uses an expected-size fast path for installed files, without re-reading 670 MB.
An explicit repair (`download_model` with `repair: true`) verifies existing files'
hashes and replaces corrupt installations. Normal downloads always verify incoming
files. Downloads abort after 30 seconds without incoming bytes (15-second connect
limit, one-hour total request cap); the busy flag clears so you can retry.
Hugging Face requests identify the app with `User-Agent: parla/<version>`.

The int8 NVIDIA Parakeet TDT 0.6B **V3** ONNX export occupies 670,619,706 bytes
(about 640 MiB; allow at least 1.4 GB free while replacing an existing model).
First use loads it into RAM; later dictations reuse the loaded model. When Parakeet
is active at startup it warms in the background. CPU latency depends on the machine
and clip length; the first load costs more than later dictations. No GPU is required.

For a manual installation, download these exact files into
`%LOCALAPPDATA%\com.matdac6.parla\models\parakeet-tdt-0.6b-v3\`:

- [encoder-model.int8.onnx](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/encoder-model.int8.onnx)
- [decoder_joint-model.int8.onnx](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/decoder_joint-model.int8.onnx)
- [nemo128.onnx](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/nemo128.onnx)
- [vocab.txt](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/vocab.txt)

Keep the filenames unchanged. This is Tauri v2 `app_local_data_dir()` for identifier
`com.matdac6.parla`: Windows LocalAppData, so the model is not copied by roaming profiles.
`model_status.model_dir` reports this resolved location. Settings and API keys stay
in roaming `%APPDATA%\com.matdac6.parla\settings.json`. Existing models from the
previous roaming folder are not moved automatically: move the four files to the
new folder or set `providers.parakeet.model_dir` to their existing folder. On Linux it is
`$XDG_DATA_HOME/com.matdac6.parla/models/parakeet-tdt-0.6b-v3` (normally
`~/.local/share/com.matdac6.parla/models/parakeet-tdt-0.6b-v3`). To use another
folder, set `providers.parakeet` to `{ "model_dir": "<absolute folder>" }` in
settings.json; `null` uses the default. Set `active_provider` to `"parakeet"`.

The Windows CI artifact includes `parla.exe` and ONNX Runtime companion DLLs;
keep them together when extracting it. CI dereferences build-output symlinks into
real files and refuses to upload an artifact without a `DirectML.dll` larger than
1 MB. ONNX Runtime itself is statically linked
using `ort`'s prebuilt CPU binaries. `transcribe-rs` is pinned to a git revision,
and `ort`/`ort-sys` are pinned to the compatible rc.12 API. No model is bundled,
committed, or downloaded by CI. The prebuilt Windows runtime links DirectML even
when Parla uses CPU inference, so `DirectML.dll` is a load-time requirement. Under
[Windows' standard DLL search order](https://learn.microsoft.com/en-us/windows/win32/dlls/dynamic-link-library-search-order),
the application directory precedes System32; keeping the supplied DLL beside
`parla.exe` selects the matching version rather than System32's DirectML DLL.

For an optional inference smoke test, set `PARLA_PARAKEET_MODEL_DIR` to an installed
model folder and run `cargo test provider::parakeet::tests::live_fixture` from
`src-tauri`. Without that variable the test skips inference. An explicit network
smoke test is available with `PARLA_PARAKEET_DOWNLOAD_DIR=<new folder>` and
`cargo test models::tests::live_download_only_when_explicitly_requested`; leave this
variable unset in CI.

Settings IPC (listen before invoking; IDs are provider IDs):

```typescript
invoke('model_status', { id: 'parakeet' })
// Promise<{ id: 'parakeet', model_dir: string, available: boolean, downloading: boolean }>
invoke('download_model', { id: 'parakeet' }) // normal install; fast size check
invoke('download_model', { id: 'parakeet', repair: true }) // verify hashes; repair if needed
// Promise<void>, resolves after installation; rejects on error, unknown ID, or busy.
// model://progress: { id: 'parakeet', downloaded: number, total: number } (aggregate bytes)
// model://done:     { id: 'parakeet' }
// model://error:    { id: 'parakeet', error: string }
```

`model://error` reports an accepted download failing; unknown IDs and concurrent
invocations reject directly without a terminal event. The settings snapshot fixes
the destination for the current download; changing `model_dir` during download
changes what subsequent status checks and dictations see. Query status again after
a terminal event or settings change. Terminal events see `downloading: false`.
