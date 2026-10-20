# Developing Parla

Building, testing and contributing. Using the app is covered in the
[user guide](guide.md).

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
git clone https://github.com/matdac12/parla
cd parla
npm ci
npm run tauri dev
```

The dev app starts in the tray. Choose **Open Parla** or **Settings…** in the tray to open its
window. Closing a secondary window destroys it; choose **Quit Parla** to exit.
The main and benchmark windows are created only when opened: the main window
from the tray, the benchmark window from the main window's sidebar. Only the
recording pill stays loaded at startup.
All windows are created after their app state is registered.

For a standalone executable, from the repository root:

```powershell
npm run tauri build -- --no-bundle
.\src-tauri\target\release\parla.exe
```

The build leaves `parla.exe` in `src-tauri\target\release\`. `ort` also puts
`DirectML.dll` there, but `build.rs` delay-loads it and the CPU-only runtime
never calls it, so the exe runs without it. The exe does need the Visual C++
runtime (`msvcp140*.dll`, `vcruntime140*.dll`), which a machine with Visual
Studio already has. The installer and the portable zip, which ship that runtime
next to the exe, come from the release workflow; see [Releasing](#releasing).

The first build compiles the whole Rust dependency tree and downloads ONNX
Runtime, so expect several minutes; incremental rebuilds are much faster.
For frontend-only work, `npm run dev` serves the pages, and `npm run build`
type-checks and bundles them.

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
mocks (pages in `src/tests/`): run `npm run dev`, then open
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

Windows is the target, and the only platform CI checks. Parla may still compile
on Linux, but nothing checks that it does; there, text insertion is a no-op that
returns an unsupported error, and the global shortcut backend needs a supported
desktop (Wayland is not supported). Live tests are gated by environment
variables and skip when absent:

- `OPENAI_API_KEY` — `cargo test --test openai_live`
- `XAI_API_KEY` — `cargo test --test xai_live`
- `PARLA_PARAKEET_MODEL_DIR` / `PARLA_PARAKEET_ULTRA_MODEL_DIR` — `cargo test provider::parakeet::tests::live_fixture`
- `PARLA_PARAKEET_DOWNLOAD_DIR` / `PARLA_PARAKEET_ULTRA_DOWNLOAD_DIR` — `cargo test models::tests::live_download_only_when_explicitly_requested`
- `PARLA_PARAKEET_MODEL_DIR` + `PARLA_LIVE_CLIPS` — `cargo test --release --lib live_clips -- --nocapture`

CI runs on GitHub Actions in a single `windows` job, on every branch push except
docs-only ones (`**.md`, `docs/**`, `LICENSE`), and by hand (**CI → Run
workflow**). It runs `cargo fmt --check`, `npm ci && npm run build`,
`cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked` on
the runner's preinstalled stable Rust, without debuginfo. It builds no exe and
uploads no artifact; only the release workflow does. A new push cancels the
branch's running CI; runs on main are never cancelled. Branch runs restore main's
Rust cache, and only main saves it.

## Releasing

Releases come from `.github/workflows/release.yml`. It builds `parla.exe`, packs
it with the x64 VC++ runtime DLLs from the runner's Visual Studio redist folder
into an NSIS installer (`tauri bundle` with `src-tauri/tauri.release.conf.json`)
and a portable zip, writes `SHA256SUMS.txt`, checks that the installed exe
starts, loads those DLLs from its own folder and does not load `DirectML.dll`,
and, for a `v*` tag, publishes them as a GitHub Release. The release notes are
`.github/release-notes.md` plus GitHub's generated changelog.

```powershell
node scripts/set-version.mjs 0.2.0   # package.json, package-lock.json, tauri.conf.json, Cargo.toml, Cargo.lock
git commit -am "Release v0.2.0"
git tag v0.2.0
git push origin main v0.2.0
```

The workflow fails if the tag does not match the version in all of those files.
Run it from the Actions tab (**Release → Run workflow**) on any branch for a dry
run: it builds and uploads the `parla-release` artifact but publishes nothing.
Builds are unsigned, so SmartScreen warns on first run.

### Updates

Installed copies update themselves (`src-tauri/src/updater.rs`, `src/updates.ts`).
The release workflow signs the installer with an updater key and publishes
`Parla_<version>_x64-setup.exe.sig` and `latest.json` next to it. The app reads
`https://github.com/matdac12/parla/releases/latest/download/latest.json` and
verifies the signature with the public key in `src-tauri/tauri.conf.json`
(`plugins.updater.pubkey`). That key is empty in the repo, and while it is empty
Parla builds and runs with the updater off: no check, no notice, no controls.
This is the updater key, not a Windows code-signing certificate.

One-time setup, in a PowerShell window. The private key never goes into the
repo or into a chat.

1. Generate the keypair outside the repo. It asks for a password:

   ```powershell
   npx tauri signer generate -w "$env:USERPROFILE\.tauri\parla-updater.key"
   ```

   It writes the private key to that path and the public key to
   `parla-updater.key.pub`. Back both up and keep the password in your password
   manager: a release signed with another key cannot update installed copies,
   which would then need a manual reinstall.
2. Paste the **contents of `parla-updater.key.pub`** (one line of base64) between
   the quotes of `plugins.updater.pubkey` in `src-tauri/tauri.conf.json` and
   commit it. Only releases built from this commit on can update, and only to
   a newer release: copies installed before it have no key and need one manual
   install.
3. Store the private key and its password as GitHub secrets:

   ```powershell
   Get-Content -Raw "$env:USERPROFILE\.tauri\parla-updater.key" | gh secret set TAURI_SIGNING_PRIVATE_KEY --repo matdac12/parla
   gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --repo matdac12/parla
   ```

   The second command prompts for the password. Without the first secret the
   workflow builds as before and publishes no `latest.json`. A **tag** build
   fails if the public key is set but the secret is missing, and any build
   fails if the secret is set but the public key is empty.
4. Test with two releases that both carry the key, say 0.1.3 and then 0.1.4
   (0.1.3 cannot update to anything yet). Install 0.1.3, then cut 0.1.4:

   ```powershell
   node scripts/set-version.mjs 0.1.4
   git commit -am "Release v0.1.4"
   git tag v0.1.4
   git push origin main v0.1.4
   ```

   When the workflow is green, check that the release lists `latest.json` and
   `Parla_0.1.4_x64-setup.exe.sig`, and that `latest.json` has version `0.1.4`
   and a `signature`. In the installed 0.1.3 open **Settings → Updates**, click
   **Check for updates now** (the background check waits two minutes after
   start and a day between checks), then **Install and restart**. Parla should
   come back as 0.1.4 with its settings and history unchanged. A portable 0.1.3
   shows "new version available" and a release-page button instead of Install.

A dry run (**Release → Run workflow**) with the secrets set builds the signed
`.sig` and `latest.json` into the `parla-release` artifact and publishes
nothing.

## Developing on Windows

- `rust-lld` is pinned as the linker in `src-tauri/.cargo/config.toml`. Never
  override the linker per command (for example `-C linker=`): switching it
  re-links every build script and forces a near-clean rebuild.
- Expected times on the maintainer's machine: a dev incremental rebuild takes
  about 5–9 s, a `cargo test --lib` run about 5 s, and a clean dev build about
  3 minutes.
- [sccache](https://github.com/mozilla/sccache) caches dependency builds across
  worktrees. Set it up per user, never in the repo: put the prebuilt `sccache.exe`
  in a user folder and add `[build] rustc-wrapper = '<path to sccache.exe>'` to
  `%USERPROFILE%\.cargo\config.toml`. Pick a target dir with `--target-dir`, not
  the `CARGO_TARGET_DIR` variable: sccache hashes every `CARGO_*` variable, so
  setting it causes cache misses.
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


## Live streaming smoke test

Provider smoke tests upload only the bundled `hello.wav` fixture:

```powershell
cd src-tauri
$env:PARLA_LIVE_STREAM_SETTINGS = "$env:APPDATA/com.matdac6.parla/settings.json"
$env:PARLA_LIVE_STREAM_PROVIDER = 'openai' # or xai
cargo test --locked --lib streaming::transport::tests::live_fixture -- --ignored --nocapture
```

