# AGENTS.md

Parla is a Windows tray dictation app: Tauri v2, Rust backend in `src-tauri/src`,
vanilla TypeScript + Vite frontend in `src/`, npm. `README.md` covers behaviour,
data paths and live tests.

## Product rules (only Mattia changes these)

- Lean, fast Rust. Latency comes first: weigh every change by its cost on the
  hotkey-to-paste path.
- Paste the raw transcript. Never add an LLM rewrite or cleanup pass.
- Keep the engine (`transcribe-rs` on ONNX Runtime via `ort`) and the native
  dependencies. A new model goes in as a drop-in on that engine; any engine or
  native-dependency swap waits for Mattia's decision.

## Checks (CI runs all of them)

From `src-tauri/`: `cargo fmt --check`,
`cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`.
From the root: `npm ci && npm run build` (UI type-check and bundle);
`npx tauri build --no-bundle` builds `src-tauri/target/release/parla.exe`.

## Builds

- `rust-lld` is pinned in `src-tauri/.cargo/config.toml`. Keep it: overriding
  the linker per command (`-C linker=`) forces a near-clean rebuild.
- Run builds at normal priority with full jobs. The README's below-normal tip
  is for a human dictating, not for agents.
- Put Windows-only code behind `cfg(windows)`; CI lints and tests on Linux too.

## Shared machine

- Take timed measurements only in the turn the orchestrator assigns you on
  `%LOCALAPPDATA%\parla-machine.lock`.
- Never commit Mattia's audio (`%APPDATA%\com.matdac6.parla\clips\` or any of
  his recordings). Tests use `src-tauri/tests/fixtures/hello.wav`.
- After touching the hotkey hook or the recorder, ask Mattia to check by hand:
  the first syllable is captured, AltGr typing works, Esc passes through when idle.

## Workflow

- Issues live in Traccia, project PARLA. Before proposing a perf change, read
  the PARLA memory "Perf investigation round 3": it lists ruled-out ideas.
- Work in a git worktree, not the main checkout.
- Branch `parla-NN-slug`; every commit message names the issue (`PARLA-NN: ...`).
