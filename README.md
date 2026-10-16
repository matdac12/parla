# Parla

Parla is a Windows dictation app where speed comes first: hold **Ctrl+Alt** to
talk, or tap **Ctrl+Alt+Space** to lock hands-free recording, and the raw
transcript is pasted into the focused app. It records the microphone while you
talk, transcribes during recording through OpenAI or xAI Grok (or locally with
Parakeet v3 or Ultra, phrase by phrase at your pauses), shows a small pill on screen (live level meter, spinner
while transcribing, then a check), and appends per-stage latency to a local
history so providers can be compared on your own voice and network. See the [v0 design spec](docs/superpowers/specs/2026-10-14-parla-design.md).

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
5. Focus Notepad, hold **Ctrl+Alt**, speak, release. The transcript is pasted
   into the focused app. Or tap **Ctrl+Alt+Space** to lock recording
   hands-free, speak, and press **Ctrl+Alt** to stop (**Esc** cancels).

## Live dictation

Cloud streaming and live captions are enabled by default. The original recording
pill keeps its appearance and position; a separate, subdued two-line preview
appears above it once words arrive. Recent words can change as recognition
settles. Nothing is typed into the focused app during recording: Parla pastes
the complete final transcript once you stop. Esc cancels while recording.

Settings → Live dictation lets you disable captions independently of streaming,
or return to whole-clip transcription. OpenAI uses `gpt-live-transcribe` for
streaming; its batch model remains separately configurable. Language detection
stays automatic unless you explicitly configured a provider language.

The full recording is retained. If the connection fails, the queue overflows,
or a usable final result does not arrive within the bounded wait, Parla sends
the saved clip through the existing batch path. Local Parakeet, benchmarks and
retries continue using whole clips. Latency records include the model and
`streaming`, `batch` or `batch_fallback` mode.

Provider smoke tests upload only the bundled `hello.wav` fixture:

```powershell
cd src-tauri
$env:PARLA_LIVE_STREAM_SETTINGS = "$env:APPDATA/com.matdac6.parla/settings.json"
$env:PARLA_LIVE_STREAM_PROVIDER = 'openai' # or xai
cargo test --locked --lib streaming::transport::tests::live_fixture -- --ignored --nocapture
```

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

The dev app starts in the tray. Choose **Open Parla** or **Settings…** in the tray to open its
window. Closing a secondary window destroys it; choose **Quit** to exit Parla.
The main, history and benchmark windows are created only when opened and can
be reopened from the tray. Only the recording pill stays loaded at startup.
All windows are created after their app state is registered.

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

### The Parla window

**Open Parla** in the tray opens the main window. It shows lifetime totals
(**Words dictated**, **Dictations**, **Audio time**, **Total cost (est.)** and
**Projected per year (est.)** against Wispr Flow's $144) and the transcript
history, newest first:
each entry has its time, provider, audio length, word count and full text, with a
**Copy** button. A search box filters the loaded transcripts, new dictations
appear at the top as they finish, and **Show older** loads more (200 at a time, up
to 2000). Failed dictations are listed in red without a Copy button; the newest one,
while its recording is still saved, has a **Retry** button that transcribes the
saved clip and adds the result to the list (nothing is pasted, since the
focus is Parla itself).

The totals are kept in `stats.json` next to `history.jsonl`, so clearing the
history does not reset them. A word is a whitespace-separated token of a
transcript that was pasted successfully; failures and benchmark rows are not
counted. On first run the counters start from whatever is already in
`history.jsonl`.
Counter updates appear immediately; a dedicated writer persists them in order
in the background and flushes queued updates when Parla exits normally.

Cost is an estimate: the audio sent to each provider, model and mode (batch or
streaming) multiplied by built-in list prices in `src-tauri/src/pricing.rs`
(checked 2026-10-15: xAI $0.10/h batch and $0.20/h streaming; OpenAI
`gpt-transcribe` $0.0045/min, `gpt-live-transcribe` $0.017/min; Parakeet free).
A provider counts as billed when it returned a transcript, even if pasting then
failed. A live session that fell back to batch is counted twice. Benchmark runs
add to cost but not to words, dictations or audio time. Models without a known
price are left out of the total and shown as a note. The yearly projection
(cost per day since first use × 365) appears after 7 days. Counters written by
older versions keep their words and get audio and cost from `history.jsonl`
once.

**Settings** opens inside the same window, with a back arrow (or **Esc**) to
return home. The **Benchmark** and **Latency log** buttons open their own
windows.

The tray menu contains, in order:

- **Open Parla** — shows or focuses the main window.
- **Settings…** — opens the main window on its settings view (providers, keys,
  hotkey, restore delay, Parakeet model and local model choice).
- **History…** — shows the latency log window (per-stage timings; see
  [History](#history)).
- **Benchmark…** — shows the benchmark window.
- **Test mic (3 s)** — records three seconds from the default microphone, saves
  it as `clips/test.wav` in the app data directory, and reveals the file in
  Explorer.
- **Retry last failed dictation** — enabled only while a recovery clip exists.
- **Quit** — exits Parla.

### Two ways to record

Both use the same shortcut, **Ctrl+Alt+Space** by default (left Alt):

- **Hold to talk.** Hold **Ctrl+Alt**. Recording runs while both keys are down;
  release either one to stop, transcribe, and paste.
- **Hands-free.** Tap **Ctrl+Alt+Space** (or press Space while already holding
  Ctrl+Alt). The recording is *locked* and keeps going after you let go. Press
  **Ctrl+Alt** again (the full shortcut also works) to stop, transcribe, and
  paste.
- **Cancel.** Press **Esc** at any point while recording to discard the clip
  without transcribing or pasting anything. Esc is swallowed only while a
  recording is in progress, so the focused app does not also see it, and behaves
  normally otherwise. Esc does not cancel a transcription that is already
  running.

Safeguards: a hold shorter than 300 ms is discarded silently, so a stray
Ctrl+Alt press does nothing; pressing any other key during the hold (another
Ctrl+Alt shortcut, for instance) cancels it; and only the **left** Alt counts,
so AltGr on layouts such as Italian (Right Alt plus a synthetic Ctrl) never
starts a recording. A shortcut with only one modifier (say `Ctrl+F9`) has no
hold mode: tap to start, tap to stop.

While transcribing, new presses are ignored; nothing is queued. If no stop
arrives within 120 seconds of capture starting, Parla stops and transcribes the
clip anyway. Before pasting, Parla saves any nonempty text clipboard; after the
paste and a delay it restores that text (best effort). The transcript is raw: no
filler removal or rewriting.
Pasting uses a 25 ms clipboard settle delay and a 25 ms modifier hold.
Enable **Paste compatibility mode** in Settings to use the original 100 ms
delays for apps that need them. The separate clipboard restore delay is unchanged.

### Recording pill

A small dark pill appears at the bottom centre of the primary monitor about
250 ms after recording starts (so brief or cancelled presses never flash it).
It never takes focus and ignores the mouse, so pasting still goes to your app.

- **Recording:** nine bars follow your voice. A small dot on the left means the
  keys are held; a padlock (with violet bars) means the recording is locked
  hands-free.
- **Transcribing:** the pill shrinks to a spinner from the moment you stop until
  the text is ready.
- **Done:** a green check for about 0.75 s once the text has been pasted. A red
  cross means the dictation failed (the usual failure notification and red tray
  icon still apply). The failed pill stays for 10 s with a **reload button**:
  click it to retry the saved clip (spinner, then check or cross again). The
  pill never takes focus, so the retried text is pasted into the app you were
  using. The pill is click-through at all other times. A cancelled recording just
  disappears.

Parla opens the default input device once at startup and keeps its stream warm,
so pressing the hotkey has no device-startup latency.

## Providers and API keys

The settings view lists every provider, whether it is ready, and lets you pick
the active one. Provider selection saves immediately and is remembered after
restart; other settings still use Save. Keys and model names are written into `providers.<id>`.

Settings live at **`%APPDATA%\com.matdac6.parla\settings.json`** on Windows
(`~/.config/com.matdac6.parla/settings.json` on Linux). Missing fields use
defaults. Example:

```json
{
  "active_provider": "openai",
  "hotkey": "Ctrl+Alt+Space",
  "restore_delay_ms": 250,
  "paste_compatibility_mode": false,
  "local_model": "parakeet-v3",
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
configurable per provider. The settings view prefills each model field with its
default (`gpt-transcribe`, `grok-voice-transcribe-2.0`) and restores it if you
clear the field.

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

## Local model (Parakeet v3 or Parakeet Ultra)

Choose **Parakeet (local)** in Settings, then pick the **Local model**:

- **Parakeet v3** (default): NVIDIA's Parakeet TDT 0.6B v3, the model Parla
  has always used.
- **Parakeet Ultra**: [moondream/parakeet-ultra](https://huggingface.co/moondream/parakeet-ultra),
  a post-trained v3 (same architecture, tokenizer and 25 languages), as the
  [Masterx ONNX int8 export](https://huggingface.co/Masterx/parakeet-tdt-0.6b-ultra-onnx/tree/99b09f030a5a6efeaa13cf2cf54592100ce2c3f1)
  (CC BY 4.0). Same engine, same speed. On 100 FLEURS clips per language
  (PARLA-36) it was better on English (WER 5.03 vs 5.55) and on Italian
  sentences without digits (2.78 vs 3.09), slightly worse on Italian overall
  (3.78 vs 3.39). Like v3's int8 export it can still loop on rare inputs.

Picking a model that is not on disk downloads it first (progress is shown on
its row, or click **Download**), and Parla switches only once every file is
verified; until then the previous model keeps working. Switching frees the old
model and loads the new one in the background, with no restart. Both models
stay on disk, each in its own folder, so switching back is instant. Missing
`local_model` in `settings.json` means v3. The one-time download needs
internet; afterwards transcription runs on your CPU with no API key or network.
The provider is unavailable until all four files of the selected model are
present at their expected sizes.

Parakeet Ultra: four files, **670,618,921 bytes** total, stored at
**`%LOCALAPPDATA%\com.matdac6.parla\models\parakeet-ultra-onnx-int8\`**. The
encoder, decoder and `vocab.txt` (byte-identical to v3's) come from the Masterx
revision above; that repo has no preprocessor, so `nemo128.onnx` (a fixed
mel filterbank, no trained weights) comes from the pinned v3 revision below.

Parakeet v3:

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
`download_model` IPC command with `repair: true`; the settings view exposes
only the normal download. Downloads abort after 30 s with no incoming bytes
(15 s connect limit, one-hour total cap), and Hugging Face requests identify the
app with `User-Agent: parla/<version>`.

Inference is CPU-only and needs no GPU. The model loads into RAM on first use
(so the first dictation costs more) and is reused afterwards; when Parakeet is
the active provider at startup, it warms in the background.

Local speed comes from four measures (measured on an i7-1370P laptop):

- **Wake on key-down.** Pressing the hotkey runs a tiny inference (0.5 s of
  silence, ~70 ms) while you start talking. After a minute idle a 12.7 s clip
  otherwise took 1.7-8.3 s (clocks down, model pages cold); with it, ~0.67 s.
- **Phrase-by-phrase transcription** (with **Transcribe while I speak**). The
  recording is cut at pauses of about half a second once a phrase has 1.5 s of
  speech, and each phrase is transcribed while you keep talking, so key-up only
  waits for the last one: 0.37 s instead of 1.2 s for a 12.7 s clip, 0.86 s
  instead of 4.5 s for 37 s. Each phrase is decoded with the previous 2 s of
  audio as context and only its own words are kept, so sentences read the same
  across cuts. Phrases longer than 20 s are cut at their quietest moment.
- **Silence trimming.** Leading, trailing and long inner silences are shortened
  to about a quarter second before inference, on the batch path too.
- **No power throttling.** Parla opts out of Windows EcoQoS, which otherwise
  schedules a background app (Parla always is while you dictate into another
  app) on efficiency cores at low clocks.

Thread counts, P-core pinning, and the GGUF/ggml engine that Handy now uses were
benchmarked too and were not faster on that machine. To use a different
folder for v3, set `providers.parakeet.model_dir` to an absolute path (`null`
uses the default) and set `active_provider` to `"parakeet"`;
`providers.parakeet.ultra_model_dir` does the same for Ultra. Each override
applies only to its own model, so an Ultra download never replaces v3 files.

To compare the two models on your own recordings (16 kHz mono WAVs; text, WER
against an optional reference, ms per clip and ms for the last 4.5 s, which is
about Parla's key-up cost), run from `src-tauri`:

```powershell
cargo run --release --example compare_models -- --list refs.tsv more.wav
```

`refs.tsv` holds `path<TAB>lang<TAB>reference` lines (`lang` is `it`, `en` or
`mixed`; it picks the language for spelling out digits in the "num" WER).
`--fleurs <dir>` reads a FLEURS folder with `refs.tsv`, `--runs N` repeats the
timing, `--no-trim` skips the silence trim (very quiet recordings lose words
to it), and `--tsv out.tsv` saves every row. Timings need an otherwise idle
machine.

The Windows CI artifact bundles `parla.exe` and the ONNX Runtime companion DLLs.
ONNX Runtime is statically linked from `ort`'s prebuilt CPU binaries; the
prebuilt Windows runtime links DirectML even for CPU inference, so `DirectML.dll`
is a load-time requirement and must sit beside `parla.exe`. No model is bundled,
committed, or downloaded by CI.

Optional inference smoke test: set `PARLA_PARAKEET_MODEL_DIR` (v3) and/or
`PARLA_PARAKEET_ULTRA_MODEL_DIR` to an installed model folder and run
`cargo test provider::parakeet::tests::live_fixture` from `src-tauri`; without
the variables the test skips. A network download test is available with
`PARLA_PARAKEET_DOWNLOAD_DIR=<new folder>` (v3) and/or
`PARLA_PARAKEET_ULTRA_DOWNLOAD_DIR=<new folder>` and
`cargo test models::tests::live_download_only_when_explicitly_requested`.
To compare whole-clip and phrase-by-phrase transcripts and timings on your own
recordings, also set `PARLA_LIVE_CLIPS` to a folder of 16 kHz mono WAVs and run
`cargo test --release --lib live_clips -- --nocapture`.

## Hotkey

The default shortcut is **Ctrl+Alt+Space**; see [Using Parla](#using-parla) for
what holding, tapping and **Esc** do. Rebind it in the settings view: type an
accelerator such as `Ctrl+Shift+F9`, or click **Record keys** and hold the
combination you want (Esc cancels). Saving rebinds immediately, with no restart,
and requires that no recording is in progress. An accelerator that cannot be
registered leaves the previous binding active and is reported inline as a
warning rather than failing the save. Letters, digits, F1–F12, arrows,
navigation keys, and common punctuation are supported; numpad and media keys are
rejected, and **Ctrl+V is reserved** for pasting.

The hold mode uses the shortcut's modifiers as the chord, and needs two or more
(`Ctrl+Alt+Space`, `Ctrl+Shift+F9`). With fewer, the shortcut is tap to start,
tap to stop.

On Windows the hotkey is a keyboard-only low-level hook that ignores injected
input; Tauri's global-shortcut plugin merely reserves the accelerator. If the
default is already taken at startup, Parla keeps running with a
disabled-hotkey tray tooltip so you can rebind it in Settings. `Ctrl+Alt+Space`
can also conflict with an IME or another global shortcut.

## History

Every completed dictation (success or failure) is appended as one JSON object per
line to **`%APPDATA%\com.matdac6.parla\history.jsonl`** on Windows
(`~/.local/share/com.matdac6.parla/history.jsonl` on Linux). The file is
append-only and written on a background thread after the paste, so it never
blocks dictation. A `kind` field (`"dictation"` or `"benchmark"`) separates the
two kinds of row.

The main window's list is the place to read and copy transcripts. Tray
**History…** is the latency log: it lists dictations newest first with: wall-clock time, provider,
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
   Closing the benchmark window stops an active recording and keeps the clip
   available when the window is reopened.
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
red cross in the pill, a short notification, and sets a red tray icon and error
tooltip. Recorder startup
failures notify too, but there is no clip to retry. Clipboard and injection
failures also report an error; an injection failure can occur after input has
already reached the destination.

To recover, click the reload button on the failed pill, press **Retry** on the
failed entry in the app window, or choose **Retry last failed dictation** in the
tray after fixing the issue (the tray waits 300 ms for the menu to dismiss; the pill and window
buttons start immediately). Retry uses the current settings/provider and pastes
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

## Resource use

Before windows were made lazy, measured once on a Windows 11 machine with the
app idle in the tray and OpenAI
as the active provider: about **610 MB** working set (**270 MB** private) in
total, of which `parla.exe` itself is only about 40 MB. Nearly all of the rest is
the WebView2 browser processes behind the four hidden windows (main, history,
benchmark, pill). With the local Parakeet model loaded, `parla.exe` grows by about
700 MB (about 1.4 GB working set in total). These are single measurements, not
benchmarks. Main, history and benchmark windows are now created on demand and
destroyed on close; the pill stays warm. The recorder drains audio every 10 ms
while start/stop commands wake it immediately. Model unloading is not implemented.

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

Frontend regression checks use the real DOM/modules with Tauri's official IPC
mocks: run `npm run dev`, then open
`http://localhost:1420/tests/frontend.html` and the same URL with `?settings`.
The page reports results for row reuse, hidden rendering, 2,000-row updates,
history-read races, cross-window history clearing, and compatibility settings.
These test pages are not included in the production bundle.

An opt-in native Windows/WebView2 lifecycle smoke test verifies lazy creation,
initial Settings navigation, destruction and reopening. With the dev server
running, run from `src-tauri/`:

```powershell
cargo test --locked --features native-smoke --lib secondary_windows_are_lazy_and_can_be_destroyed_and_reopened -- --ignored --nocapture
```

Linux can compile, format, lint, and unit-test the app, but text insertion is a
no-op that returns an unsupported error (Windows is the target), and the global
shortcut backend needs a supported desktop (Wayland is not supported). Live
tests are gated by environment variables and skip when absent:

- `OPENAI_API_KEY` — `cargo test --test openai_live`
- `XAI_API_KEY` — `cargo test --test xai_live`
- `PARLA_PARAKEET_MODEL_DIR` / `PARLA_PARAKEET_ULTRA_MODEL_DIR` — `cargo test provider::parakeet::tests::live_fixture`
- `PARLA_PARAKEET_DOWNLOAD_DIR` / `PARLA_PARAKEET_ULTRA_DOWNLOAD_DIR` — `cargo test models::tests::live_download_only_when_explicitly_requested`
- `PARLA_PARAKEET_MODEL_DIR` + `PARLA_LIVE_CLIPS` — `cargo test --release --lib live_clips -- --nocapture`

CI runs on GitHub Actions. The `windows` job runs `cargo test --locked` and
`npx tauri build --no-bundle`, stages `parla.exe` and the ONNX Runtime DLLs
(failing if a real `DirectML.dll` larger than 1 MB is missing), and uploads the
`parla-windows-x64` artifact. The `linux` job runs `cargo fmt --check`,
`cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, and
`npm ci && npm run build`.

## Developing on Windows

- `rust-lld` is pinned as the linker in `src-tauri/.cargo/config.toml`. Never
  override the linker per command (for example `-C linker=`): switching it
  re-links every build script and forces a near-clean rebuild.
- Expected times on the maintainer's machine: a dev incremental rebuild takes
  about 5–9 s, a `cargo test --lib` run about 5 s, and a clean dev build about
  3 minutes.
- Add Microsoft Defender exclusions for `%USERPROFILE%\.cargo`,
  `%USERPROFILE%\.rustup`, the `target` folders (`src-tauri\target` in each
  checkout) and `%USERPROFILE%\.t3\worktrees`, and exclude code folders from
  Windows Search indexing. Before the exclusions, Defender used about 344 CPU-seconds
  during a single clean build.
- Builds are what slow Parla's local transcription. If you dictate while a long
  build runs, start it at below-normal priority (rustc and the linker inherit it),
  for example in PowerShell:

  ```powershell
  cmd /c start "" /belownormal /b /wait cargo test --locked
  ```

## Known limitations / untested on Windows

CI compiles and unit-tests on Linux and on the Windows runner. On a real Windows
machine the maintainer has so far confirmed: the app starts in the tray; OpenAI,
xAI and local Parakeet dictation paste into text fields; the benchmark view runs
all three providers; hold-to-talk, hands-free, Esc cancel and the recording pill
behave as described. Still unchecked: Windows Terminal, elevated windows,
lock/sleep recovery, notification delivery, clipboard restore under load, and
the failure/retry path.

- **No installer, code signing, or auto-update.** The app ships as a plain
  `--no-bundle` build. Windows SmartScreen will likely warn on first run; signed
  and packaged behavior (including reliable notifications) is untested.
- **Elevated windows.** A non-elevated app cannot inject paste into
  elevated/admin windows (Windows UIPI). Secure desktop and applications that
  intercept Ctrl+V are unsupported. Windows Terminal must be allowed to paste
  with Ctrl+V.
- **Keyboard hook.** The hook is keyboard-only. A 100 ms watchdog (deferred
  during synthetic input) ends a hold or chord whose key-up was missed; the hook
  refreshes after recovery and every five seconds while idle to survive silent
  timeout removal. Hook permission, keyboard layouts, IME behavior, and
  lock/sleep recovery still need manual checks, and rebinding briefly pauses
  hotkey dispatch.
- **Esc.** Esc is swallowed only while recording, and a transcription already in
  flight cannot be cancelled.
- **Modifiers.** Paste reuses Ctrl if it is already held, and injected cleanup
  cannot change the physical modifier model: cleared Alt/Shift/Windows modifiers
  can appear released to other apps until pressed again. If you release Ctrl
  but keep Alt held when a hold ends, the paste can arrive as Ctrl+Alt+V.
- **Clipboard restore is best effort.** Restoration uses a timed delay, not an
  acknowledgement from the receiving app, so clipboard managers or slow consumers
  can cause stale or duplicate pastes. Only text is preserved (empty or non-text
  clipboards are not restored, and previous non-text formats are lost);
  comparing clipboard text does not detect copying identical text during the
  delay. A longer restore delay helps slow apps.
- **Local Parakeet.** The first run loads the whole model into RAM, costing more
  than later dictations; inference is CPU-only, so time grows with clip length
  (and it is weaker than the cloud providers on speech that switches between
  languages mid-sentence). Allow ~1.4 GB free while replacing the model.
- **Settings reload.** Settings are read at startup; editing `settings.json`
  externally requires a restart.
- **Memory.** The recording pill keeps WebView2 loaded; secondary windows release
  their resources on close. The older baseline is in
  [Resource use](#resource-use).
- **Linux.** Compiles and tests, but insertion is a no-op and Wayland is not a
  supported desktop.

## Recognition hints and latency

Settings includes an optional vocabulary editor (one term per line, at most 100
terms of 50 characters). The list belongs to the local user profile and ships
empty. Controls and angle brackets are rejected before requests; terms are
native recognition hints, with no transcript replacement or rewriting.
OpenAI `gpt-transcribe` and `gpt-live-transcribe` receive Italian and English
language hints and vocabulary keywords. Other OpenAI models retain only their
supported legacy language field. xAI receives repeated keyterms and otherwise
keeps automatic language detection; local Parakeet is unchanged.

Cloud setup starts during the initial 300 ms hold grace. Audio stays buffered
locally until the grace expires, and rejected holds or recording cancellation
close setup without uploading audio. The complete original recording is kept
for recovery; successful streaming skips full-clip batch conversion. Retry,
local transcription and benchmarks still receive the complete 16 kHz clip.

History records may include `stream_setup_ms` and `first_caption_ms`, measured
from streaming session start, unlike release-relative delivery stages. These
fields are absent in older records. Conversion savings are generally small;
provider/network finalization remains the main release-to-paste delay.
