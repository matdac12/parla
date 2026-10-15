# Parla

Parla is a Windows push-to-talk dictation app where speed comes first: hold a
hotkey, speak, release, and the raw transcript is pasted into the focused app.
It records the microphone on key-down, transcribes the whole clip on release
through OpenAI, xAI Grok, or a local Parakeet V3 model, and appends per-stage
latency to a local history so providers can be compared on your own voice and
network. See the [v0 design spec](docs/superpowers/specs/2026-10-14-parla-design.md).

## Quick start on Windows

1. Open the latest green **CI** run on `main` under
   [Actions](https://github.com/matdac6/parla/actions) and download the
   `parla-windows-x64` artifact.
2. Extract the whole folder. `parla.exe` and the ONNX Runtime `.dll` files
   (including `DirectML.dll`) must stay in the same folder; copying only the
   executable will fail to start.
3. Run `parla.exe`. It has no main window: it lives in the system tray.
   WebView2 is required (preinstalled on Windows 11; see
   [prerequisites](#windows-prerequisites)).
4. Open the tray menu and choose **Settings…**, paste an OpenAI or xAI API key
   (or download the local Parakeet model), then **Save**.
5. Focus Notepad, hold **Ctrl+Space**, speak, release. The transcript is pasted
   into the focused app.

## Windows prerequisites

Building needs:

- [Rust via rustup](https://rustup.rs/) with the stable MSVC toolchain
  `x86_64-pc-windows-msvc`.
- [Visual Studio 2022 Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
  with **Desktop development with C++**, which includes the MSVC compiler and
  the Windows SDK.
- [Node.js LTS](https://nodejs.org/en/download) with npm (CI uses Node 24).
- The [Microsoft Edge WebView2 Evergreen Runtime](https://developer.microsoft.com/en-us/microsoft-edge/webview2/#download-section).
  It is preinstalled on Windows 11 and required on Windows 10.
- **Internet during the first build.** Cargo fetches `transcribe-rs` from its
  GitHub revision, and `ort-sys` (via the `ort` crate's default
  `download-binaries` feature) downloads a prebuilt ONNX Runtime archive from
  `cdn.pyke.io` and copies its companion DLLs next to the executable. `npm ci`
  also fetches packages. A later offline rebuild works from the cargo/npm caches.

See also [Tauri's prerequisites](https://v2.tauri.app/start/prerequisites/).

## Build and run

In PowerShell:

```powershell
git clone https://github.com/matdac6/parla
cd parla
npm ci
npm run tauri dev
```

The dev app starts in the tray. Choose **Settings…** in the tray to open its
window. Closing any window hides it; choose **Quit** to exit.

For a standalone executable, from the repository root:

```powershell
npm run tauri build -- --no-bundle
.\src-tauri\target\release\parla.exe
```

The build leaves `parla.exe` and the ONNX Runtime companion DLLs (including
`DirectML.dll`) in `src-tauri\target\release\`. Keep them in the same folder;
copying only the executable will prevent startup. No installer, code signing, or
auto-update is produced.

The first build compiles the whole Rust dependency tree and downloads ONNX
Runtime, so expect several minutes; incremental rebuilds are much faster.
For frontend-only work, `npm run dev` serves the pages, and `npm run build`
type-checks and bundles them.

## Using Parla

The tray menu contains, in order:

- **Settings…** — shows or focuses the settings window (providers, keys, hotkey,
  restore delay, Parakeet model).
- **History…** — shows the dictation history window.
- **Benchmark…** — shows the benchmark window.
- **Test mic (3 s)** — records three seconds from the default microphone, saves
  it as `clips/test.wav` in the app data directory, and reveals the file in
  Explorer.
- **Retry last failed dictation** — enabled only while a recovery clip exists.
- **Quit** — exits Parla.

Hold-to-talk: pressing the hotkey starts microphone capture (the recorder adds a
150 ms pre-roll so the first syllable is not clipped). Releasing the main key
stops capture in either order — release Space, or release Ctrl first — and then
the clip is transcribed and the raw transcript is pasted with Ctrl+V. Before
pasting, Parla saves any nonempty text clipboard; after the paste and a delay it
restores that text (best effort). The transcript is raw: no filler removal or
rewriting. New holds that start while a dictation is finalizing, transcribing, or
pasting are ignored through their release; nothing is queued. If no release
arrives within 120 seconds of capture starting, Parla stops and transcribes the
clip anyway.

Parla opens the default input device once at startup and keeps its stream warm,
so pressing the hotkey has no device-startup latency.

## Providers and API keys

The settings window lists every provider, whether it is ready, and lets you pick
the active one. Keys and model names are written into `providers.<id>`.

Settings live at **`%APPDATA%\com.matdac6.parla\settings.json`** on Windows
(`~/.config/com.matdac6.parla/settings.json` on Linux). Missing fields use
defaults. Example:

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
    },
    "parakeet": {
      "model_dir": null
    }
  }
}
```

Keys are stored as plaintext in the file. Environment variables override the
file's key at request time and are never written to the file or returned by
`get_settings`: `OPENAI_API_KEY` for OpenAI and `XAI_API_KEY` for xAI (an
environment value, even an empty one, wins over the file). `language` is `null`
for automatic detection or an ISO code such as `"it"`. Model names are
configurable per provider.

### OpenAI

Verified against the official documentation on 2026-10-15:
[`POST /v1/audio/transcriptions`](https://developers.openai.com/api/reference/resources/audio/subresources/transcriptions/methods/create)
accepts a multipart `file` (16 kHz mono WAV) plus `model` and
`response_format=json`, and returns JSON with `text`. The default model is
[`gpt-transcribe`](https://developers.openai.com/api/docs/models/gpt-transcribe)
at an estimated
**[$0.0045 per audio minute](https://developers.openai.com/api/docs/pricing)**.
A non-null `language` is sent as the `language` field. The upload cap is 25 MB.

### xAI Grok

Verified against the official xAI documentation on 2026-10-15:
[`POST https://api.x.ai/v1/stt`](https://docs.x.ai/developers/rest-api-reference/inference/speech-to-text)
accepts a multipart `file` (16 kHz mono WAV here, sent last) plus `model`, and
returns JSON with `text`; the request shape comes from the
[Speech to Text guide](https://docs.x.ai/developers/model-capabilities/audio/speech-to-text).
The default model is
[`grok-voice-transcribe-2.0`](https://docs.x.ai/developers/model-capabilities/audio/speech-to-text)
(batch) at **[$0.10 per audio hour](https://docs.x.ai/developers/model-capabilities/audio/voice)**
(about $0.0017/min). A non-null `language` is also sent with `format=true` so
spoken numbers and currencies are normalized. The batch endpoint accepts up to
500 MB.

Both cloud providers share one connection-reusing rustls HTTP client with a 5 s
connect timeout and a 30 s total request timeout, and neither retries
automatically. There is no local latency comparison yet; set a different model
to benchmark alternatives (for example `gpt-4o-mini-transcribe`).

## Local model (Parakeet V3)

Choose **Parakeet V3 (local)** in Settings and click **Download model**
(progress is shown inline). The one-time download needs internet; afterwards
transcription runs on your CPU with no API key or network. The provider is
unavailable until all four files are present at their expected sizes.

Four files, **670,619,706 bytes** total (about 640 MiB; allow at least 1.4 GB
free while replacing an existing model), stored at
**`%LOCALAPPDATA%\com.matdac6.parla\models\parakeet-tdt-0.6b-v3\`** on Windows
(`~/.local/share/com.matdac6.parla/models/parakeet-tdt-0.6b-v3` on Linux):

- [encoder-model.int8.onnx](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/encoder-model.int8.onnx)
- [decoder_joint-model.int8.onnx](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/decoder_joint-model.int8.onnx)
- [nemo128.onnx](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/nemo128.onnx)
- [vocab.txt](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/vocab.txt)

Keep the filenames unchanged. A download verifies exact sizes and SHA-256 hashes
in a sibling temporary directory, then publishes the completed directory by
rename; a failed download is discarded, not resumed, and can be retried.
**Download model** also short-circuits when the installed files already match
their expected sizes (without re-reading 670 MB). An explicit repair — verify
existing hashes and replace a corrupt installation — is available through the
`download_model` IPC command with `repair: true`; the settings window exposes
only the normal download. Downloads abort after 30 s with no incoming bytes
(15 s connect limit, one-hour total cap), and Hugging Face requests identify the
app with `User-Agent: parla/<version>`.

Inference is CPU-only and needs no GPU. The model loads into RAM on first use
(so the first dictation costs more) and is reused afterwards; when Parakeet is
the active provider at startup, it warms in the background. To use a different
folder, set `providers.parakeet.model_dir` to an absolute path (`null` uses the
default) and set `active_provider` to `"parakeet"`.

The Windows CI artifact bundles `parla.exe` and the ONNX Runtime companion DLLs.
ONNX Runtime is statically linked from `ort`'s prebuilt CPU binaries; the
prebuilt Windows runtime links DirectML even for CPU inference, so `DirectML.dll`
is a load-time requirement and must sit beside `parla.exe`. No model is bundled,
committed, or downloaded by CI.

Optional inference smoke test: set `PARLA_PARAKEET_MODEL_DIR` to an installed
model folder and run `cargo test provider::parakeet::tests::live_fixture` from
`src-tauri`; without the variable the test skips. A network download test is
available with `PARLA_PARAKEET_DOWNLOAD_DIR=<new folder>` and
`cargo test models::tests::live_download_only_when_explicitly_requested`.

## Hotkey

The default hotkey is hold **Ctrl+Space** (see
[Using Parla](#using-parla) for the hold behaviour). Rebind it in the settings
window: type an accelerator such as `Ctrl+Shift+F9`, or click **Record keys**
and hold the combination you want (Esc cancels). Saving rebinds immediately,
with no restart, and requires that no hold is in progress. An accelerator that
cannot be registered leaves the previous binding active and is reported inline
as a warning rather than failing the save. Letters, digits, F1–F12, arrows,
navigation keys, and common punctuation are supported; numpad and media keys are
rejected, and **Ctrl+V is reserved** for pasting.

On Windows the hotkey is a keyboard-only low-level hook that ignores injected
input; Tauri's global-shortcut plugin merely reserves the accelerator. If the
default is already taken at startup, Parla keeps running with a
disabled-hotkey tray tooltip so you can rebind it in Settings. `Ctrl+Space` can
also conflict with an IME or another global shortcut.

## History

Every completed dictation (success or failure) is appended as one JSON object per
line to **`%APPDATA%\com.matdac6.parla\history.jsonl`** on Windows
(`~/.local/share/com.matdac6.parla/history.jsonl` on Linux). The file is
append-only and written on a background thread after the paste, so it never
blocks dictation. A `kind` field (`"dictation"` or `"benchmark"`) separates the
two kinds of row.

Tray **History…** lists dictations newest first with: wall-clock time, provider,
captured audio length, then the latency breakdown — **Ready** (key-up to audio
ready), **Transcribe** (audio ready to provider returned), **Paste** (provider
returned to input injection), and **Total** (key-up to pasted). A dash means that
stage never completed (a failed transcription has no paste time). The transcript
is truncated, with the full text on hover or click; failures are tinted and show
the error. The view updates live and **Clear history…** truncates the file after
a confirmation. Reads are bounded: the view requests the newest 200 rows and the
backend walks the file backwards, so a large history never has to fit in memory;
corrupt lines are skipped.

## Benchmark

Tray **Benchmark…** compares providers on your own voice and network.

1. Click **Record clip**, speak, click **Stop recording** (it stops by itself
   after 30 s). Hotkey dictation is blocked while the benchmark owns the
   microphone.
2. Click **Run benchmark**. The clip goes through every registered provider in
   turn; each row fills in as its provider finishes. **Run again** re-runs the
   same clip, so runs compare identical audio. Providers that are not configured
   appear as rows with the reason and are not called; one provider failing shows
   its error and does not stop the others.

What the numbers mean:

- **Request** — time in the provider's transcribe call (encoding, upload, server
  time, and download for cloud providers; inference for Parakeet).
- **Total** — from handing the clip to that provider until its text is ready. A
  benchmark has no key-up, so "clip ready" stands in for it; expect Request plus
  a few milliseconds.

Providers run sequentially. Cloud providers get no warm-up, so the first request
includes TLS setup, like the first dictation after a pause. Local Parakeet is
warmed with an untimed half-second of silence so its model load is not counted.
The fastest successful row is highlighted.

Storage: each provider result is a row in `history.jsonl` with
`kind: "benchmark"` and a shared `bench_run_id`; the **Previous runs** list
rebuilds runs from those rows. The clip is saved as `clips/bench-<id>.wav` in the
app data directory. To compare providers, record the same ~5 s sentence a few
times and compare Request across runs rather than trusting a single run.

## When something fails

A transcription failure (missing configuration, network, authentication,
timeout, empty audio, or an empty transcript) pastes nothing: Parla shows a
short notification and sets a red tray icon and error tooltip. Recorder startup
failures notify too, but there is no clip to retry. Clipboard and injection
failures also report an error; an injection failure can occur after input has
already reached the destination.

To recover, choose **Retry last failed dictation** in the tray after fixing the
issue (the tray waits 300 ms for the menu to dismiss; the `retry_last_failed`
command starts immediately). Retry uses the current settings/provider and pastes
into whichever app has focus when injection runs. Busy retries report
"Busy, try again" and keep the clip; failed retries keep it as well.

The last failed clip is held in memory and saved as
**`%APPDATA%\com.matdac6.parla\clips\last-failed.wav`**. It survives restarts,
loads lazily on retry, and can be opened in an audio player. A new failure
replaces it; a successful retry deletes it. It contains your voice and stays
until replaced or successfully retried — delete it manually while Parla is
closed if you don't want it kept.

Notification delivery on Windows is guaranteed only for installed apps with an
AppUserModelID; development may show PowerShell's name and icon, and delivery
from the unpackaged `parla.exe` is not guaranteed. The red tray icon and error
tooltip are always set as a fallback.

## Development

From `src-tauri/`, with Rust on `PATH`:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

and from the repository root, `npm ci && npm run build` if you touched the
frontend.

Linux can compile, format, lint, and unit-test the app, but text insertion is a
no-op that returns an unsupported error (Windows is the target), and the global
shortcut backend needs a supported desktop (Wayland is not supported). Live
tests are gated by environment variables and skip when absent:

- `OPENAI_API_KEY` — `cargo test --test openai_live`
- `XAI_API_KEY` — `cargo test --test xai_live`
- `PARLA_PARAKEET_MODEL_DIR` — `cargo test provider::parakeet::tests::live_fixture`
- `PARLA_PARAKEET_DOWNLOAD_DIR` — `cargo test models::tests::live_download_only_when_explicitly_requested`

CI runs on GitHub Actions. The `windows` job runs `cargo test --locked` and
`npx tauri build --no-bundle`, stages `parla.exe` and the ONNX Runtime DLLs
(failing if a real `DirectML.dll` larger than 1 MB is missing), and uploads the
`parla-windows-x64` artifact. The `linux` job runs `cargo fmt --check`,
`cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, and
`npm ci && npm run build`.

## Known limitations / untested on Windows

Everything here was built on Linux and only compiled and unit-tested in CI on the
Windows runner; nothing has been run on Windows yet. The list below is what needs
a manual Windows smoke test or is a known limitation.

- **No installer, code signing, or auto-update.** The app ships as a plain
  `--no-bundle` build. Windows SmartScreen will likely warn on first run; signed
  and packaged behavior (including reliable notifications) is untested.
- **Tray, windows, and lifecycle.** Tray visibility, settings/history/benchmark
  window focus, hide-on-close, and Quit need a manual Windows check.
- **Hotkey and paste.** Hotkey release and pasting have not been run on Windows.
  Test Notepad, a browser text field, and Windows Terminal; try both release
  orders, a long hold, repeated short holds, very rapid release/repress cycles
  (event transitions use raw key-up notifications), and clipboard restoration
  under load.
- **Elevated windows.** A non-elevated app cannot inject paste into
  elevated/admin windows (Windows UIPI). Secure desktop and applications that
  intercept Ctrl+V are unsupported. Windows Terminal must be allowed to paste
  with Ctrl+V.
- **Keyboard hook.** The hook is keyboard-only. A 100 ms watchdog (deferred
  during synthetic input) ends a hold whose key-up was missed; the hook refreshes
  after recovery and every five seconds while idle to survive silent timeout
  removal. Hook permission, keyboard layouts, IME behavior, and lock/sleep
  recovery still need manual Windows checks, and rebinding briefly pauses hotkey
  dispatch.
- **Modifiers.** Paste reuses Ctrl if it is already held, and injected cleanup
  cannot change the physical modifier model: cleared Alt/Shift/Windows modifiers
  can appear released to other apps until pressed again. Test both left and right
  Ctrl and repeated Space presses while continuing to hold Ctrl.
- **Clipboard restore is best effort.** Restoration uses a timed delay, not an
  acknowledgement from the receiving app, so clipboard managers or slow consumers
  can cause stale or duplicate pastes. Only text is preserved (empty or non-text
  clipboards are not restored, and previous non-text formats are lost);
  comparing clipboard text does not detect copying identical text during the
  delay. A longer restore delay helps slow apps.
- **Local Parakeet.** The first run loads the whole model into RAM, costing more
  than later dictations; inference is CPU-only. Allow ~1.4 GB free while
  replacing the model.
- **Settings reload.** Settings are read at startup; editing `settings.json`
  externally requires a restart.
- **Real end-to-end dictation.** Live transcription needs a configured key (or a
  downloaded local model) and a manual smoke test; the default models' latency
  has not been compared on this machine.
- **Linux.** Compiles and tests, but insertion is a no-op and Wayland is not a
  supported desktop.
