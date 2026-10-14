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

Hold **Ctrl+Space**, then release Space to finish. Releasing Ctrl before Space also works: the hold ends when Space comes up. This spike pastes the fixed string `Parla test ✓`; recording and transcription will replace it in PARLA-4. Holding the shortcut does not repeatedly paste.

On Windows, rdev observes raw key-down/key-up events, suppressing repeats without polling; Tauri's global-shortcut plugin reserves the accelerator so normal key handling does not consume it. `Hotkey::set_shortcut(&str)` supports runtime rebinding (for example `Ctrl+Shift+F9`); the settings UI comes later. Release the current hold before rebinding; press and release keys normally after app startup. Letters, digits, F1–F12, arrows, navigation, and common punctuation keys are supported; unsupported keys such as numpad and media keys are rejected. A failed registration retains the previous shortcut; `Ctrl+V` is reserved for paste.

Paste runs on a background worker: save nonempty clipboard text, publish the test string, wait 100 ms, clear held shortcut modifiers, send Ctrl+V with a 100 ms modifier hold, then wait **250 ms** (`DEFAULT_RESTORE_DELAY_MS`) before restoring the previous text. The caller can configure that restore delay; settings UI comes later. Clipboard restore is best effort and skips restoration when the clipboard contains different text copied during the paste. Empty or non-text clipboards are not restored: the test text remains and previous non-text formats are lost. Slow apps may require a longer restore delay.

## Benchmark

Provider comparison and history instructions will be added with benchmarking.

## Known limitations / untested on Windows

- Everything in this repo was built on Linux; Windows behaviour has only been compiled in CI, not run.
- This initial scaffold's Windows CI run is pending the orchestrator's push; no Windows build has been verified yet.
- Tray visibility, Settings window focus, hide-on-close, and Quit need a manual Windows smoke test.
- Hotkey release and paste have not been run on Windows. Verify Notepad, a browser text field, and Windows Terminal; try both release orders, a long hold, repeated short holds, and clipboard restoration under load. Try very rapid release/repress cycles too; event transitions use raw key-up notifications.
- A non-elevated app cannot inject paste into elevated/admin windows (Windows UIPI). Secure desktop and applications that intercept Ctrl+V are unsupported.
- The rdev Windows hook is process-lived; hook startup failure exits the app with a diagnostic. Hook permission, keyboard layouts, IME behavior, and recovery after session lock/sleep need manual checks. Rebinding briefly pauses hotkey dispatch.
- Windows Terminal must allow Ctrl+V paste. Ctrl+Space may conflict with an IME or another global shortcut; registration failure is reported during startup.
- Clearing held Ctrl/Alt/Shift/Windows modifiers makes paste independent of release order. A modifier still physically held may appear released until it is pressed again; test both left and right Ctrl and check for stuck modifiers.
- Clipboard restoration uses a timed delay, not an acknowledgement from the receiving app. Clipboard managers or slow consumers can still cause stale/duplicate pastes. Restoration preserves text only, and comparing clipboard text does not detect copying identical text during the delay.
- Linux can compile and test the app, but insertion is a no-op returning an unsupported error; the fallback global shortcut plugin requires a supported desktop (Wayland is not a supported target).
- Dictation, audio capture, providers, and real settings are not implemented yet.
