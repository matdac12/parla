# Local dictation: precompute the last phrase, trim the paste step

Approved in conversation on 2026-10-15 (Traccia PARLA-20). The user wants this
implemented autonomously and verified end to end without further approval stops.

## Baseline (main 7449b0b, i7-1370P, Parakeet V3 int8, live mode)

Measured from `history.jsonl` on the user's first local dictations:

| audio | text ready after key-up (`provider_returned`) | pasted (`pasted`) |
|---|---|---|
| 3.2 s | 165 ms | 266 ms |
| 4.4 s | 191 ms | 249 ms |
| 9.3 s | 323 ms | 376 ms |

Almost all of "text ready" is the last phrase being transcribed after key-up
(`streaming/local.rs`: the splitter emits the tail from `PhraseSplitter::finish`
only when the recorder closes the feed). The paste stage adds 50-100 ms
(`inserter.rs::paste_with_backend`).

## Goals

1. **Precomputed tail.** When the user pauses before releasing the key, the
   unfinished phrase is transcribed during the pause. If no speech follows, the
   result is reused at key-up. Target: `provider_returned` at most 60 ms for a
   dictation that ends with a pause of 270 ms or more, unchanged otherwise.
2. **Shorter paste stage.** Remove avoidable work between "text ready" and Ctrl+V
   without changing paste reliability. Target: `pasted - provider_returned`
   down by at least 15 ms on the user's machine, or a measured explanation of
   why it cannot be.

Non-goals: changing transcripts (the reused text must be byte-identical to what
the current code would produce), the cloud providers, phrase cut rules
(PAUSE/KEEP/MIN/MAX constants), context length, or any paste wait duration
(`settle` 25 ms / compatibility 100 ms, `restore_delay_ms`).

## Part 1: precomputed tail

### Invariant that makes reuse safe

`PhraseSplitter::finish` trims trailing quiet to `KEEP_FRAMES` (8 frames,
240 ms) once `quiet_run > KEEP_FRAMES`, and `cut_at_pause` also ends a phrase
with exactly `KEEP_FRAMES` of quiet. Therefore a snapshot of the open phrase,
taken when `quiet_run` reaches `SPECULATE_FRAMES = KEEP_FRAMES + 1` (9 frames,
270 ms) and trimmed to `KEEP_FRAMES` of trailing quiet, is sample-for-sample
equal to:

- what `finish()` returns if no speech frame arrives before key-up, and
- the phrase `cut_at_pause()` emits if the pause grows to `PAUSE_FRAMES` with
  no speech in between.

Identity is tracked by a counter, not by comparing samples: `speech_seen: u64`
counts every speech frame the splitter has ever accepted. A snapshot and a later
phrase with the same `speech_seen` are the same audio. Add a debug assertion in
tests (not release) that equal ids imply equal samples.

### `src-tauri/src/segment.rs`

- `const SPECULATE_FRAMES: usize = KEEP_FRAMES + 1;` with a doc comment
  explaining the invariant above.
- New field `speech_seen: u64`, incremented wherever `speech_frames` is
  incremented. Do not reset it on cuts.
- New public type:
  ```rust
  pub enum Event {
      /// A finished phrase. `id` is `speech_seen` when it was cut.
      Phrase { id: u64, samples: Vec<f32> },
      /// The open phrase as key-up would leave it now; transcribe speculatively.
      Tail { id: u64, samples: Vec<f32> },
  }
  ```
- `pub fn push_events(&mut self, samples: &[f32]) -> Vec<Event>`: same frame
  loop as `push`. Emit `Event::Tail` exactly once per quiet stretch, at the
  frame where `quiet_run == SPECULATE_FRAMES` and `speech_frames > 0`. The
  samples are `phrase[..phrase.len() - FRAME]` (that is, 9 quiet frames minus 1,
  leaving `KEEP_FRAMES`). Emit `Event::Phrase` where `push` returns a phrase
  today, with `id = speech_seen`.
- Keep `push` as a thin wrapper returning only the phrase samples, so existing
  callers and tests stay unchanged.
- `pub fn finish_event(self) -> Option<Event>` returning
  `Event::Phrase { id: speech_seen, samples }`. Keep `finish()` as a wrapper.
- A forced cut (`cut_at_quietest`) emits `Phrase` with the current
  `speech_seen`. Its remainder is open speech, so no `Tail` is pending.

Tests to add in `segment.rs`:

1. `tail_snapshot_equals_finish_when_only_quiet_follows`: speech then 0.4 s of
   quiet (`hush`), then finish. Exactly one `Tail`; its id and samples equal the
   finished phrase's.
2. `tail_snapshot_equals_the_pause_cut`: speech of 2 s, then 0.8 s of quiet. A
   `Tail` and then a `Phrase` with the same id and identical samples.
3. `speech_after_a_tail_changes_the_id`: speech, 0.3 s of quiet (Tail), more
   speech, finish. The final phrase id differs from the Tail id, and its samples
   are longer.
4. `no_tail_for_short_breaths`: a 0.2 s gap (under 270 ms) emits no Tail.
5. `push_and_finish_wrappers_are_unchanged`: the existing tests still pass
   untouched.

### `src-tauri/src/streaming/local.rs`

Splitter thread (`split`): call `push_events` and `finish_event` and forward the
events over the existing `std::sync::mpsc` channel (change the element type from
`Vec<f32>` to `segment::Event`).

Transcriber thread (`transcribe`): restructure as follows. `Transcript::add`
splits into:

```rust
/// Text of `phrase` given the committed context; does not change state.
fn transcribe_next(&self, settings: &Settings, phrase: &[f32]) -> Result<String, ProviderError>;
/// Commits a phrase and its text: updates `recent` and joins the text.
fn commit(&mut self, phrase: &[f32], piece: &str) -> bool;
```

Loop:

```text
speculated: Option<(id, String)> = None
latest_tail: Option<(id, samples)> = None
loop {
    // 1. Drain everything already queued, without blocking.
    while let Ok(event) = queue.try_recv() { handle(event) }
    // 2. Idle and a newer tail is waiting: transcribe it speculatively.
    if let Some((id, samples)) = latest_tail.take() {
        if speculated.as_ref().map(|s| s.0) != Some(id) {
            let text = transcript.transcribe_next(settings, &samples)?;
            speculated = Some((id, text));
        }
        continue;
    }
    // 3. Otherwise block for the next event; exit when the channel closes.
    match queue.recv() { Ok(event) => handle(event), Err(_) => break }
}
handle(Tail{id, samples})   => latest_tail = Some((id, samples))   // newest wins
handle(Phrase{id, samples}) => {
    let piece = match speculated.take() {
        Some((sid, text)) if sid == id => { reused += 1; text }
        _ => transcript.transcribe_next(settings, &samples)?,
    };
    if transcript.commit(&samples, &piece) { caption(...) }
    latest_tail = None   // any pending tail belonged to this phrase or earlier
}
```

Rules:

- Speculation only ever runs when the queue is empty, so every committed
  phrase before it is already in `transcript.recent`. The context it used is
  final, and its text equals what a non-speculative run would produce.
- A speculation is never cancelled midway (inference is not interruptible). If
  key-up arrives during it, the final `Phrase` waits in the queue and reuses
  the result when the ids match. That is the intended "wait, don't restart".
- `halted()` is checked at the top of each loop iteration as today.
- `wake()` still runs first.
- Captions: unchanged (only committed text). Optional and only if trivial: show
  the speculated text as the `provisional` caption part. If you do it, the
  provisional text must clear on the next commit.

Record whether the tail was reused. Add `tail_precomputed: Option<bool>` to
`streaming::Streamed`, set to `Some(true)` when the final phrase reused a
speculation and `Some(false)` otherwise. Plumb it into `DictationRecord` (in
`timing.rs`) as `#[serde(default, skip_serializing_if = "Option::is_none")]
pub tail_precomputed: Option<bool>` in `flow.rs::transcribe_and_paste`, where
`stream_setup_ms` and `first_caption_ms` are copied today. Cloud sessions leave
it `None`. Check `src/` and the history view for any strict field lists; there
should be none, because unknown fields are ignored.

Tests to add in `local.rs`. Pure tests need no model; the transcriber loop
should take a closure or trait for "transcribe" so they can count calls:

1. `final_phrase_reuses_the_matching_speculation`: events
   Phrase(a), Tail(id 7), [close with] Phrase(id 7). The fake transcriber is
   called twice (a, tail) and not a third time, and the text is joined.
2. `newer_speech_discards_the_speculation`: Tail(7), Phrase(9) → transcribed
   again.
3. `newest_tail_wins`: Tail(5) and Tail(7) queued before the transcriber is
   idle: only 7 is speculated.
4. `speculation_never_runs_ahead_of_queued_phrases`: Phrase(a), Tail(7) queued
   together: a is committed before the tail is transcribed (assert the call
   order and the context passed).

### Verification for Part 1

- `cargo test`, `cargo clippy --all-targets` (0 warnings) and `cargo fmt --check`
  from `src-tauri`.
- Real model, in release:
  `PARLA_PARAKEET_MODEL_DIR=%LOCALAPPDATA%\com.matdac6.parla\models\parakeet-tdt-0.6b-v3`
  with `streaming::tests::local_session_end_to_end_when_model_is_provided`.
  Extend that test to append 0.5 s of silence before closing the feed (people
  pause before releasing) and print `tail_precomputed`. Expected key-up to text:
  at most 60 ms with the pause; without the pause, within noise of today
  (348 ms for the 12.7 s clip, 869 ms for the 37 s clip).
  Clips: `%TEMP%\parla-live-clips` (re-create from
  `%APPDATA%\com.matdac6.parla\clips\bench-*.wav` if missing).
- Transcripts must be byte-identical with and without the precompute for the
  same clip and pause (run once with speculation disabled through a test-only
  switch, and compare).

## Part 2: paste stage

`paste_with_backend` today does: `Clipboard::new` + `Enigo::new` (in
`paste_text_with_options`), `read_text` (save the user's clipboard),
`write_text`, `wait(settle=25 ms)`, `send_paste(key_hold=25 ms)` (which also
probes and releases modifiers), then takes the timestamp and restores after
`restore_delay_ms`.

1. **Measure first.** Time each step (backend construction, read, write,
   settle, send) for 10 real pastes into Notepad through the app. Use a
   temporary `eprintln!` or the existing timing record. Note the numbers in the
   PR/issue. Do not keep noisy logging.
2. **Prepare the backend before key-up.** Construct the `arboard::Clipboard`
   and `enigo::Enigo` at recording start on the dictation thread
   (`flow.rs::run_session` already owns start-to-paste on one thread). Pass them
   into a new `inserter::paste_prepared(prepared, text, restore_delay,
   compatibility_mode)`. Fall back to today's lazy construction if preparation
   failed. Keep the `TRANSACTION` mutex at paste time.
3. **Keep the read at paste time.** The user can copy something while
   dictating hands-free, so the saved clipboard must be read at paste, not at
   key-down. Do not cache it.
4. **No wait changes.** `settle`, `key_hold`, compatibility mode and
   `restore_delay_ms` stay as they are. If measurement shows the waits are
   essentially all of the stage, report that in the issue instead of tuning them.
5. Existing `inserter.rs` tests (fake `Desktop`) must keep passing. Add one for
   `paste_prepared` with the fake backend proving identical call order.

Verification: report `pasted - provider_returned` from `history.jsonl` before
and after (5+ dictations each, Parakeet live).

## Done means

- Both parts merged to `main`, release app rebuilt with
  `npx tauri build --no-bundle`, and Parla relaunched from
  `src-tauri\target\release\parla.exe`. Stop the running instance first;
  Windows locks the exe.
- README "Local model" section updated: one bullet for the precomputed tail,
  and the paste note if part 2 changed behaviour.
- PARLA-20 updated with measured before/after numbers and moved to Done.
