# Task: Personal vocabulary, bilingual recognition and lower dictation latency

**Date:** 2026-10-15 · **Repo:** Parla · **Branch:** feat/streaming-captions

## What to do

Implement the next approved accuracy/performance pass in this Windows Rust/Tauri
dictation app. The user approved the decisions below and explicitly requested a
fresh T3-owned GPT-6.1 Sol agent to execute them. Read the current checkout,
write a concrete implementation plan, then carry it through testing and a
release build. Do not reopen settled design questions. Work independently on
the authorized scope; test the final experience with Mattia one step at a time
when he is available.

## Context you need

Streaming transcription and the discreet caption companion are implemented,
running, and enthusiastically accepted after several hours of real use. The
user loves the current appearance and near-immediate final paste. Preserve it.

This checkout contains MANY uncommitted changes: first performance pass,
streaming/captions, Windows COM microphone recovery, provider auto-persistence,
tests and docs. They are the working product, not disposable scratch work.
Use this shared checkout; a new worktree from HEAD would omit all that work.
The parent is handing off ownership and will not edit product code in parallel.
Inspect git status/diff before changes; do not reset, stash away, or overwrite
existing work. Do not commit/push/merge merely to make this handoff work.

## Approved decisions

### 1. User-owned vocabulary hints

The user reviewed the proposed list and explicitly approved ALL entries:

- Supabase
- Atlassian
- MCP
- CRM
- Traccia
- Wispr Flow (the confirmed intended spelling behind logged Whisper flow / WhisperFlow)
- Francesca
- OpenAI
- xAI
- Grok

Add configurable, persisted vocabulary hints to recognition. Use the provider's
native keyword/keyterm support; do not add an LLM rewriting pass or blindly
replace words after transcription. Hints must not force unrelated speech into
these terms. Validate limits and unsafe characters before sending session
updates; apply hints on both streaming and batch fallback where supported.
Keep the feature simple in the existing settings visual language.

These are this user's approved vocabulary, including a personal name. Keep
the actual list in the local user profile, editable/removable by the user;
do not ship personal terms as universal defaults. Preserve every existing
setting/provider section/API key when applying it. The user explicitly
authorized these terms in this handoff; do not copy any full personal
transcripts, credentials, or other names into repo artifacts.

### 2. Automatic Italian + English

The user speaks Italian about 75% and English about 25%, often moving between
projects or switching within a recording. No per-recording language selection.
For OpenAI use both expected language hints (`it`, `en`) where supported by
the selected streaming/batch model. The percentages are descriptive, not
weights or quotas. Preserve original speech/language; do not translate.
xAI documents a single optional language hint, so retain automatic detection
and mid-recording switching rather than forcing Italian or inventing a
multi-language parameter. Local Parakeet stays on its existing path.

### 3. Microphone behavior

Follow Windows' default input device by default and, for this pass, keep only
that behavior. It already exists. NO microphone picker, preferred-device
database, or extra microphone UI is requested.

Preserve the recent fix: initialize the dedicated audio worker as COM MTA
before CPAL; the non-Send RAII guard balances initialization on that thread.
The user connected a USB-C headset before seeing RPC_E_CHANGED_MODE during
input configuration/reopen. The apartment regression failed before the fix
and passed afterward; hardware open/reopen and simulated failed-stream
next-start recovery passed. User subsequently reported the app works.

### 4. Lazy batch-recovery conversion

Current Recorder::stop drains the final raw samples, closes the audio feed,
resamples the ENTIRE clip to 16 kHz and converts to i16, then flow collects
the streaming result and pastes. Defer/move full-clip conversion away from
the successful streaming delivery path. Keep the entire original audio and
sample rate for recovery, and materialize the batch Clip only when needed.
An incremental recovery conversion is also acceptable if it is simpler and
measurably better; do not duplicate expensive conversions or stall the
microphone worker/audio callback. Preserve retry/benchmark/local behavior.

Be honest about expected gains. Today's 25 successful streaming dictations
had median audio_ready 6 ms, provider_returned 696 ms and pasted 749 ms
(timestamps relative to release; medians are not additive). Median paste
step was 58 ms. A 35.34 s recording had 12 ms audio preparation, 717 ms more
until final provider return, 65 ms paste, 794 ms total. Most remaining delay
is provider/network finalization, not this conversion. Do not promise a
400 ms saving; point 4 was an item number, not a duration.

### 5. Connect earlier without uploading accidental holds

Current streaming::Session::spawn sleeps MIN_HOLD (300 ms) BEFORE connecting.
Overlap authenticated WebSocket connection/setup with the initial hold grace
period. Audio stays buffered locally and must not be transmitted until the
hold is confirmed/eligible. A short rejected hold or Escape must close the
setup task and never upload audio or paste. Mere connection/setup may occur
earlier; this change was explained and approved. Keep bounded queues, deadlines
and complete-clip fallback. No always-connected idle cloud session is needed.
Measure first-caption latency; faster connection alone is not a proven UX gain.

## Read first

- `docs/superpowers/specs/2026-10-15-streaming-captions-design.md` — accepted UX/invariants.
- `docs/superpowers/plans/2026-10-15-streaming-captions.md` — existing architecture, verification ledger and follow-up fixes; do not recreate that work.
- `src-tauri/src/recorder.rs` — warm audio worker, subscription, recovery conversion, COM guard.
- `src-tauri/src/streaming/{mod,audio,transport,protocol}.rs` — cancellation, bounded feed, PCM conversion, setup/finalization and reconciliation.
- `src-tauri/src/flow.rs` — release/cancel boundary, streaming selection, fallback and single paste.
- `src-tauri/src/provider/{openai,xai}.rs` — batch request builders and config resolution.
- `src-tauri/src/settings.rs`, `src-tauri/src/commands/settings.rs`, `src/settings.ts`, `app.html` — persistence/UI. Provider radio choices now auto-save via select_provider without saving other drafts.
- `src-tauri/src/timing.rs` — backward-compatible latency records and mode/model metadata.
- `tests/frontend.html/js`, `tests/overlay.html/js` — actual UI with mocked IPC.
- `README.md` — build/test commands and runtime packaging.

Verify current official API schemas before coding:
- https://developers.openai.com/api/docs/guides/realtime-transcription
- https://developers.openai.com/api/docs/guides/speech-to-text
- https://docs.x.ai/developers/model-capabilities/audio/speech-to-text

At handoff, OpenAI live uses gpt-live-transcribe with `delay: low`, supports
`keywords` and `languages`; xAI uses grok-voice-transcribe-2.0 and repeated
`keyterm` parameters. Verify limits and batch-model compatibility rather
than assuming all model families accept the same fields.

## Constraints

- Keep the 116x32 recording pill, screen anchor, waves/lock/spinner/check/retry.
  Caption companion stays at most 330 px wide, two lines, 13 px type, 8 px above.
- Never paste partials. Exactly one complete final paste; Escape DURING RECORDING
  cancels and never pastes. Existing Escape behavior after release is out of scope.
- Retain full recovery audio, pre-roll and tail. Failed/empty/disconnected/slow
  streaming falls back to batch. No audio callback waits for network or disk.
- Stream early connection is cancellable during connect, readiness wait and
  before upload. Test both real short-hold controller rejection and task abort.
- Preserve automatic provider persistence and the USB COM fix.
- This is Windows PowerShell. Use rg, do not use Bash brace expansion in paths.
- Never print settings.json or API keys. User profile is under
  `%APPDATA%/com.matdac6.parla`; settings.json contains credentials, history.jsonl
  contains personal transcripts. Only read the minimum necessary locally.
- Provider live tests may upload ONLY bundled `src-tauri/tests/fixtures/hello.wav`.
  Do not upload saved personal audio. User will perform personal speech tests.
- Do not widen scope into transcript rewriting, auto-learning from other apps,
  reading destination text/screens, model migration, new overlay design, or
  cost dashboard work. No unrelated agents or top-level conversations needed.

## Implementation order and definition of done

1. Inspect current state and record an actionable plan for this approved pass.
2. Implement approved vocabulary + bilingual hints with old-settings migration,
   serialization/request tests and editable user-local configuration.
3. Overlap connection setup with the grace period; prove zero audio frames for
   rejected/cancelled holds, correct buffered prefix/tail and single final paste.
4. Remove recovery conversion from the streaming success path; prove fallback
   receives the complete correct clip and retry/local/benchmark still work.
5. Run targeted tests, then `cargo test --locked` and
   `cargo clippy --locked --all-targets --all-features -- -D warnings`,
   `cargo fmt --check` from src-tauri; `npm run build` at repo root.
6. Run browser regression fixtures with T3 preview. Its top-level document.hidden
   is nonconfigurable; run frontend fixture in a same-origin iframe for visibility
   tests. Finish pill animations before checking settled geometry in a hidden tab.
7. Add reliable stage timings (including first caption/setup if needed) and compare
   before/after with representative fixture durations. Do not infer gains solely
   from faster unit tests; describe measurement limitations.
8. Use configured cloud providers only for explicit bundled-fixture smoke tests.
   Existing opt-in: PARLA_LIVE_STREAM_SETTINGS = local settings path,
   PARLA_LIVE_STREAM_PROVIDER = openai or xai, then
   `cargo test --locked --lib streaming::transport::tests::live_fixture -- --ignored --nocapture`.
9. Build release (`node node_modules/@tauri-apps/cli/tauri.js build --no-bundle`),
   launch and verify the intended process is responding. A running Windows exe
   locks rebuilding: identify the exact executable with Resolve-Path and compare
   normalized native paths before stopping ONLY that app at final restart time.
   Start-Process background launches must use -WindowStyle Hidden.
10. Report changes/tests honestly and invite one manual test at a time: Italian,
    English, mixed language + vocabulary, quick cancel, longer dictation, USB
    reconnect. Human speech/aesthetic acceptance must not be claimed in advance.

## Current verification / runtime details

Most recent pass: 139 unit tests passed, 3 opt-ins ignored; all-target/all-feature
Clippy and formatting passed; frontend build passed; 11 frontend checks passed
from both Home and Settings; optimized release built and launched. Live OpenAI
and xAI bundled-fixture tests passed in the earlier streaming pass. Check fresh
state instead of trusting old counts or process IDs.

`native-smoke` Cargo feature supports the ignored real WebView2 lifecycle test
in tray.rs. It includes native overlay anchor checks. A harmless test-executable
icon-resource warning precedes its supplied fallback icon. Native microphone
open/reopen probe is separately opt-in in recorder.rs; it saves/uploads nothing.

DirectML.dll in target/release may be a SYMLINK showing Length 0. That is not an
empty DLL. Do not replace it with a system DLL. ORT statically links here; a
separate onnxruntime.dll may be absent. The verified cached DirectML target is
18,527,776 bytes, SHA256 9c9e6d822561c6c41b90e6994b3e8857cf1d66dbfb1e0c4c799c7c89b4e92da1.

## Suggested skills

- `superpowers:writing-plans` — turn this approved brief into implementation steps.
- `superpowers:test-driven-development` — regressions for audio/cancel/config changes.
- `openai-docs` — verify provider model fields against official OpenAI sources.
- `superpowers:systematic-debugging` — investigate any unexpected behavior before fixes.
- `superpowers:verification-before-completion` — commands/evidence before completion claims.

## Short launch prompt

Read `docs/handoff/2026-10-15-dictation-accuracy-and-latency.md` first.
Execute the approved accuracy/latency pass there, preserving all existing working changes.
