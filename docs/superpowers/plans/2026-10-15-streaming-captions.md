# Streaming Captions Implementation Plan

> **For agentic workers:** Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Stream cloud dictation while showing optional captions above the unchanged recording pill, then paste once.

**Architecture:** Nonblocking recorder subscription feeds a bounded Tokio queue.
A cancellable WebSocket task owns provider protocol state and incremental
resampling. The existing completion path selects a valid final transcript or
falls back to batch on the saved full clip. A single generation-tagged overlay
canvas renders only two lines of captions above the original pill.

**Tech Stack:** Rust/Tauri, Tokio, tokio-tungstenite, rubato, TypeScript/CSS.

**Spec:** ../specs/2026-10-15-streaming-captions-design.md

## Global constraints

- Preserve 116 × 32 px pill and its physical anchor; caption gap 8 px, width ≤330 px, text 13 px, two lines.
- Partials never paste. Escape never pastes. Recovery uses the complete original clip.
- No audio callback/network blocking; bounded queues; no plaintext keys or audio in diagnostics.
- Italian/English automatic detection remains. Streaming and captions have separate settings.
- Preserve all existing first-pass changes in the current checkout.

## Review focus

- Cancellation during connection and after the final audio packet must never deliver text.
- Slow or failed transport must not truncate the batch recovery clip.
- Revised/interleaved/duplicate provider events must not duplicate the delivered transcript.
- Delayed overlay events must not reappear in another session or after cancellation.
- A monitor scale change or captions toggle must not move the original pill.

## Task 1: Bounded recorder subscription

**Files:** src-tauri/src/recorder.rs; new src-tauri/src/streaming/audio.rs.

**Interfaces:** `AudioFeed`/`AudioChunk` carries raw mono f32 plus sample rate.
`Recorder::subscribe(feed)` attaches to the current generation, including captured
pre-roll. Capture flushes the final fragment and closes input on Stop.

- [x] Add and run tests for pre-roll, incremental packets, tail, overflow, stale generations and full-clip recovery.
- [x] Implement teeing only on the worker and expose reusable incremental resampling.
- [x] Run recorder/audio tests and the existing suite.

## Task 2: Provider session and single delivery

**Files:** new streaming/mod.rs, protocol.rs, transport.rs; provider configs;
flow.rs; timing.rs; Cargo.toml/lock.

**Interfaces:** `Session::start(settings, caption_callback)` provides an AudioFeed,
and `finish()` yields a final transcript or a recoverable streaming error. Drop
cancels immediately. `Caption` has settled/provisional text. Stream completion
is selected before the existing batch provider and single paste call.

- [x] Test protocol parsers and local WebSocket server exchange, cancellation, timeouts and final replacement.
- [x] Implement OpenAI and xAI adapters, bounded transport and deadlines.
- [x] Test batch fallback and one final paste, then integrate into hotkey flow.
- [x] Record streaming/fallback/model metadata without secrets for latency comparison.

## Task 3: Caption companion and settings

**Files:** overlay.rs, overlay.ts/css/html; settings.rs/ts; src/app.html;
new src/tests/overlay.html/js; existing src/tests/frontend.js.

**Interfaces:** Generation-tagged caption reset/update/clear events. Explicit
`streaming_enabled` and `live_captions` settings, both default true.

- [x] Add browser checks for unchanged pill geometry/anchor, two lines, safe text, revisions, generations and clear/toggle behavior.
- [x] Implement upward canvas extension, coalesced text updates and optional controls.
- [x] Verify browser checks and frontend build; inspect rendered overlay.

## Task 4: Integration and release

- [x] Run all-feature Rust tests, formatting, Clippy, browser checks and native window smoke.
- [x] Run gated provider tests on only the bundled speech fixture when configured.
- [x] Request a fresh read-only review, resolve important findings and rerun affected checks.
- [x] Update README, build release, preserve the running app until ready for testing together.

## Execution ledger

Ruling: The user explicitly authorized autonomous completion of the approved design;
spec/plan are recorded here without interrupting them for additional review gates.
Ruling: Work continues on feat/streaming-captions in the current checkout so the
already implemented, uncommitted first pass remains available and intact.
Pre-flight: AudioFeed and Caption types connect Tasks 1–3; use the signatures above.
Progress: Baseline first-pass suite and release were verified in the previous turn.

Verification: default Rust suite: 135 unit tests plus 4 core contracts passed (two explicit live tests ignored by default). Both configured cloud providers separately passed the bundled speech fixture with nonempty final text and caption updates. Overlay browser suite: 6/6 passed. Frontend suite: 10/10 passed from both Home and Settings, run in an iframe because the collaborative preview fixes top-level document.hidden as nonconfigurable. Native WebView2 window smoke passed, including physical bottom-anchor equality across caption canvas sizes. Fresh independent read-only review found no critical or important issues. Cancellation during finalization retains the existing app behavior: Escape cancels during recording only.
Release: prior app was stopped only when release compilation began, after implementation and live checks; updated app will be launched after the build succeeds. Human review of aesthetics, mixed Italian/English speech quality and one-minute release-to-paste timing remains intentionally for the user's return.
Final checks: all-feature unit suite 136 passed / 3 explicitly ignored; all-feature Clippy with warnings denied passed. Both pre-connect and connected-session cancellation tests pass. Native anchor smoke was rerun successfully after fixing the test startup race (setup occurs after run begins).

Release build passed: src-tauri/target/release/parla.exe, 38,097,920 bytes. Initial build hit a locked old executable (path comparison used different separators); resolved absolute path identified the old process, stopped it, and rebuild passed. Updated app launched for the user. No personal audio or real destination text box was used during autonomous tests.
Completion audit: inspected current spec, completed checklist, release artifact and responding release process (PID 28632). Added and passed an explicit local-server regression for both stalled WebSocket handshake and stalled provider readiness deadlines (10.02 s); all-feature Clippy rerun passed. This final addition is test-only and does not change the running release. Autonomous implementation and validation are complete; user acceptance testing remains for the planned joint session.
Follow-up bug: USB-C headset connection preceded RPC_E_CHANGED_MODE (0x80010106) when fetching the default input configuration on reopen. CPAL 0.18.2 selects STA by default; a regression on this Windows machine observed APTTYPE_MAINSTA=3 where the dedicated worker requires MTA=1. Explicitly initialize only parla-audio as MTA before CPAL calls, hold a non-Send RAII guard until stream teardown, and balance successful COM initialization on exit. UI apartment remains untouched. The reported intermittent activation error was not reproduced by the initial open/reopen probe; the apartment regression failed before the fix and passed after it. Hardware probe passed initial open plus repeated reopens and simulated failed-stream next-start recovery; no captured audio was saved or uploaded. Unit suite 138 passed, 3 explicit opt-ins ignored; all-target/all-feature Clippy and formatting passed. Real physical USB reconnect acceptance remains for the user.
USB-C follow-up release: optimized build passed in 1m55s, executable 38,098,432 bytes; restarted PID 32240 and confirmed responding. Startup stderr contains no microphone initialization/configuration errors. Physical reconnect test remains for the user.
Follow-up provider persistence: selecting a provider used to update only the draft form until Save. Added select_provider IPC, serialized with the existing SaveLock, merging only active_provider into current persisted/runtime settings. Frontend saves the choice immediately, preserves unrelated draft edits, prevents overlapping provider/form saves, and restores the previous choice with a visible error if persistence fails. Real-file reload regression and failure/unknown-provider checks passed; 139 unit tests passed (3 explicit opt-ins ignored), 11 browser checks passed including immediate persistence and draft preservation/rollback, frontend build and all-target/all-feature Clippy passed. Provider hint and README explain immediate saving. Release rebuild in progress.
Provider persistence follow-up release: build passed (1m55s); updated app launched. Both Home and Settings browser routes passed all 11 checks. User should select their preferred provider once in the updated build; previous unsaved selections cannot be recovered.
