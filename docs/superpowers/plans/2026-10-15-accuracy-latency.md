# Dictation accuracy and latency implementation plan

> Execute inline with superpowers:executing-plans. Approved handoff governs scope.

**Goal:** Add editable native recognition hints, overlap connection setup safely, and defer recovery conversion.
**Architecture:** Shared validated vocabulary is persisted per user. Provider builders select compatible native fields. Transport connects immediately but gates audio on eligibility. Raw recovery audio converts once, only for batch or failure retention.
**Tech stack:** Rust/Tauri, Tokio, TypeScript, existing browser fixtures.
**Spec:** ../../handoff/2026-10-15-dictation-accuracy-and-latency.md

## Constraints and review focus

Preserve every pre-existing working change, COM MTA initialization, default microphone, overlay geometry, full recovery samples, bounded queues and single final paste. No commits, worktrees, personal transcript artifacts or additional approval gates. Never upload personal audio.

Test old settings and provider preservation; invalid/oversized vocabulary; compatible model fields; short-hold and Escape cancellation before audio; delayed setup and buffered prefix/tail; lazy full-clip fallback and retry compatibility.

## Tasks

- [x] 1. Settings/provider hints: add failing migration, validation and request tests; implement `Settings::vocabulary: Vec<String>` and validation (100 entries, 50 Unicode characters, no controls or angle brackets). Use `keywords[]`/`languages[]` for gpt-transcribe and JSON arrays for gpt-live-transcribe; repeated xAI keyterm. Add textarea in existing settings, preserve unrelated fields. Seed only local profile with approved entries at final restart.
- [x] 2. Early setup: test immediate connection with zero upload before eligibility and cancellation; move grace gate after readiness and share cancellation signal from controller. Preserve setup/write/final deadlines. Record setup and first-caption offsets from session start.
- [x] 3. Lazy recovery: test raw duration and exact once-only conversion, full fallback and no conversion for successful stream. Add `Recorder::stop_raw` and cached recovery representation; keep `stop` for local/benchmark callers. Integrate lazy materialization through completion/failure.
- [x] 4. Verification: targeted tests, full locked suite, all-feature Clippy, fmt, frontend build and T3 browser fixtures. Fixture-only cloud smoke, representative synthetic conversion/setup measurements; report limitations.
- [x] 5. Release: build optimized executable, stop only exact current release path when necessary, apply local vocabulary preserving full JSON, restart hidden and verify responding. Invite one manual Italian test.

## Execution ledger

Initial state inspected: branch feat/streaming-captions, 26 modified tracked files plus streaming/tests/docs directories. No accuracy-pass edits existed. Existing accepted streaming spec and verification ledger read. Official provider pages verified 2026-10-15. Shared 100/50 vocabulary cap is an application limit using xAI's documented bound; OpenAI unsafe characters are rejected before network.

Progress: migration regression failed with vocabulary=null before implementation, then passed. Early-connect regression failed on the 200 ms accept deadline before moving the gate, then passed. Current 150-test unit run passed (3 opt-ins ignored); later cancellation/save regressions added. Recovery tests check full sample equality at 16/44.1/48 kHz, cached conversion, existing Clip reuse and no materialization on streaming success. Controller quick-release and Escape tests exercise actual rejection and verify no uploaded frame even when recorder stop is delayed; stalled-handshake cancellation also covered.
Browser: frontend 12/12 from Home and Settings in same-origin iframe; overlay 6/6, including unchanged pill dimensions/anchor and cancellation clearing. Frontend build passed.
Live: only bundled hello.wav uploaded. OpenAI streaming and batch passed (74 characters); setup 1252 ms, first caption 1814 ms. xAI streaming and batch passed (73 characters); setup 670 ms, first caption 1777 ms. Single runs are smoke evidence, not proven latency gains. Native fields validated against official provider pages in handoff plus OpenAI Realtime client-events schema.
Review: fresh read-only reviewer identified configurable gpt-transcribe streaming hint omission and obsolete no-TCP short-hold assertion. Both accepted for fix; adding failing model regression, removing obsolete scheduling-dependent assertion in favor of zero-audio/controller/handshake coverage. No other critical/important issues reported. Prior interrupted delegate remains stopped.
Runtime inspection: current release process path matches this root checkout; current user-selected provider is local Parakeet. Preserve that choice and all provider sections. No configured single-language hint; profile currently has no vocabulary. Seed only the ten handoff-approved entries at final restart.

Final checks: cargo test --locked passed 152 unit tests / 4 explicit opt-ins ignored, four core contracts, and two environment-gated integration entry points (no keys exported there, so these do not count as additional live smoke). All-target/all-feature Clippy with -D warnings and cargo fmt --check passed. npm run build passed. gpt-transcribe streaming hint regression verified RED then GREEN. Removed only the obsolete no-connection assertion; controller rejection, socket abort, readiness and stalled-handshake regressions remain green. No deferred review findings.
OpenAI comparison (one fixture run per mode): old delayed-setup simulation setup=1233 ms / first-caption=1799 ms / returned=6805 ms; early setup=1252 / 1814 / 6791 ms. Network variation masks any benefit; this does not establish improved first-caption latency. The producer paces fixture packets at 100 ms, so delivery offsets include fixture duration and scheduling; no destination paste occurs in these tests.
Tooling note: overlapping cargo test processes caused one Windows LNK1104 when another test binary was running. No product change needed; rerunning sequentially exposed the expected failing model regression and subsequent full checks passed. Subsequent cargo commands are serialized.

xAI comparison (one fixture run per mode): delayed setup=914 ms / first-caption=2072 ms / returned=6357 ms; early setup=670 / 1777 / 6433 ms. Earlier caption by 295 ms in this pair, final return 76 ms later. Combined with the inconclusive OpenAI pair, these are smoke measurements and do not prove a general UX or final-paste improvement.

Native WebView2 lifecycle smoke passed with native-smoke enabled, including physical overlay anchor equality. Expected test-executable icon resource warning used the supplied fallback icon. No microphone/audio recording was used in this smoke.

Timing interpretation: first_caption_ms measures first nonempty provider caption received by the transport, not WebView paint or human perception. Delivery and transcription quality still need user speech acceptance. Optimized synthetic conversion measurement is running for 5/35/120-second 48 kHz clips; it does not upload audio or exercise microphone/paste.

Optimized synthetic measurement passed: 48 kHz mono clips of 5/35/120 seconds took 4860/26562/99106 microseconds for the legacy full conversion work, versus no materialization at the lazy boundary (reported 0 microseconds at measurement resolution). This excludes capture drain/network/paste and uses synthetic samples on a loaded machine; it is avoided work, not a real-world end-to-end speedup claim.
Local profile seeded with all ten approved handoff entries, merging any existing vocabulary. Atomic same-directory replacement verified exact JSON equality for every other setting and provider section, including active provider. No values/credentials printed or copied into repo artifacts. Release build started through the requested Tauri CLI command; only exact resolved release-path processes were eligible for stopping.

Release completed: requested node Tauri build --no-bundle passed in 1m54s. Stopped only PID 34908 after normalized resolved path equality. Updated release launched hidden as PID 35816; exact executable path verified and Responding=true. Startup log contains no microphone/COM initialization errors. DirectML.dll remains its existing cache symlink; no runtime DLLs replaced. Post-launch local profile has ten vocabulary entries. The user changed active provider from the initial Parakeet snapshot to xAI before final seeding; the latest xAI choice was preserved. All pre-existing uncommitted changes remain in the root checkout; no commit, reset, stash, worktree, push or merge performed.
Completed approved implementation and autonomous verification. Remaining human acceptance is one test at a time: Italian, English, mixed language/vocabulary, quick cancellation, longer dictation, physical USB reconnect. No personal speech quality or aesthetic acceptance claimed.

Human acceptance: Mattia confirmed that the shortcut works again after the duplicate-instance conflict was handled in the parallel troubleshooting thread. He then confirmed Italian dictation and English dictation both work well, and requested committing and pushing all current working changes as a checkpoint. Mixed-language/vocabulary, longer speech and physical USB reconnect acceptance have not been separately reported. No additional app restart is needed for this checkpoint.
