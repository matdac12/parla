# Streaming dictation and discreet captions

Approved in conversation on 2026-10-15. The user explicitly requested autonomous
implementation and verification without further approval stops.

## Experience

- Keep the existing recording pill exactly 116 × 32 CSS pixels, its current
  screen position, meter, lock indicator, spinner, success and retry states.
- Add a separate dark translucent caption companion 8 px above it, at most
  330 px wide, with 13 px text and at most two lines. Recent provisional words
  are dimmer. No timers, labels, buttons, character typing effects or focus.
- Captions appear only after text arrives, update in small groups, remain while
  finalizing, and clear after paste, failure, cancellation or the next session.
- Live captions are optional independently of streaming. Both default on for
  cloud dictation. Local Parakeet and benchmark/retry retain batch behavior.
- Never type partials into the destination. Release finalizes; paste one complete
  final transcript once. Escape during recording cancels and never pastes.
- Preserve automatic Italian/English switching; never force a single language.

## Architecture

The existing warm recorder remains authoritative for the complete recovery clip.
A nonblocking subscription tees approximately 100 ms of raw mono audio into a
bounded queue. A network task resamples incrementally to the provider's format,
streams through a backend-only authenticated WebSocket, and emits caption updates.
The callback and audio worker never wait for the network. Queue overflow, setup
failure, protocol error, disconnect, missing final or timeout discards the partial
result and uses the entire saved clip through the original batch provider.

OpenAI uses a dedicated transcription session with the documented live model;
xAI uses its streaming STT endpoint. All secrets stay in Rust. Startup, writes
and finalization have deadlines. Dropping a session cancels its task, including
connection setup; session generations reject late caption events.

The overlay keeps one WebView. Its transparent canvas grows upward to make room
for captions while the pill's physical position stays fixed. With captions off,
the original window dimensions and layout are used. Rendering uses text nodes,
bounded preview text, and coalesced updates; the entire transcript stays in Rust.

## Verification

Exercise incremental resampling, pre-roll and tail preservation, queue overflow,
cancel, setup and finalization timeout, revised partials, final replacement,
multiple provider events, and one final delivery with batch fallback. Use local
WebSocket servers for deterministic protocol tests. Browser checks use real
overlay CSS/DOM with mocked IPC, including unchanged pill dimensions, its anchor,
two-line clipping, provisional styles, safe text, generations, clearing and toggles.
Verify Rust suite/Clippy, frontend build, native WebView lifecycle and release build.
Use only the bundled nonprivate speech fixture for any gated live-provider test;
never record the user's microphone or upload saved personal clips for validation.

## Sources

- https://developers.openai.com/api/docs/guides/realtime-transcription
- https://docs.x.ai/developers/model-capabilities/audio/speech-to-text
- https://docs.rs/tokio-tungstenite/0.30.0/tokio_tungstenite/
