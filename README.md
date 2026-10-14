# Parla

Parla is a Windows push-to-talk dictation app focused on speed: hold a hotkey, speak, release, and paste the transcript into the focused app.
It will compare local and cloud speech-to-text providers by latency; see the [v0 design spec](docs/superpowers/specs/2026-10-14-parla-design.md). This scaffold currently provides only the tray and a placeholder settings window.

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

The build produces `src-tauri\target\release\parla.exe`. Or download the `parla-windows-x64` artifact from the latest green [GitHub Actions run](https://github.com/matdac6/parla/actions), extract it, and run `parla.exe`. WebView2 is still required; no installer is produced.

For frontend development alone: `npm run dev`. To type-check and build it: `npm run build`.

## API keys

Configuration instructions will be added with the cloud providers.

## Local model (Parakeet)

Model setup and download instructions will be added with the local provider.

## Hotkey

The default will be **hold Ctrl+Space**. Hotkey handling and rebinding are not implemented yet.

## Benchmark

Provider comparison and history instructions will be added with benchmarking.

## Known limitations / untested on Windows

- Everything in this repo was built on Linux; Windows behaviour has only been compiled in CI, not run.
- This initial scaffold's Windows CI run is pending the orchestrator's push; no Windows build has been verified yet.
- Tray visibility, Settings window focus, hide-on-close, and Quit need a manual Windows smoke test.
- Dictation, audio capture, text insertion, providers, and real settings are not implemented yet.
