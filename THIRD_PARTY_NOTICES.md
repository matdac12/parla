# Third-party notices

No third-party source code or artwork has been copied into this scaffold. The placeholder icon is original to Parla.

Dependencies retain their own licenses; dependency versions are recorded in `package-lock.json` and `src-tauri/Cargo.lock`.

## PARLA-2 shortcut and paste research / adapted patterns

### cjpais/Handy (MIT)

Source: https://github.com/cjpais/Handy/tree/cfcb650758899d96392ab6248cb2c1c03e50aa93

Adapted clipboard save/write/delay/paste/restore transaction and error-safe restore pattern from `src-tauri/src/clipboard.rs`; virtual-key Ctrl+V and modifier hold timing from `src-tauri/src/input.rs`. Studied `shortcut/tauri_impl.rs`, `shortcut/handy_keys.rs`, and `paste_tx/windows.rs`. Current Handy offers both Tauri and handy-keys; its rdev dependency is macOS-only. Parla uses rdev raw key events for Windows press/release (including very rapid holds), with Tauri registration only to reserve the shortcut. Tauri's Windows release polling was rejected because a release/repress between 50 ms polls can merge holds. No receipt-based Windows clipboard code was copied.

License verified 2026-10-15, retained below:

```text
MIT License

Copyright (c) 2025 CJ Pais

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

### jamditis/audiobud (MIT)

Source: https://github.com/jamditis/audiobud/tree/76ef5adc2cd648836bb262c6b4f445af05b3ebe9

Studied Windows shortcut handling and clipboard serialization in `src-tauri/src/shortcut/` and `src-tauri/src/clipboard.rs`. Adopted the pattern of serializing clipboard capture, write, paste and restore as one transaction; no source was copied verbatim.

License verified 2026-10-15, retained below:

```text
MIT License

Copyright (c) 2025 CJ Pais (Handy, the upstream project)
Copyright (c) 2026 Joe Amditis (AudioBud, modifications)

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

Dependencies added: `tauri-plugin-global-shortcut` (Apache-2.0 OR MIT), `arboard` (Apache-2.0 OR MIT), `enigo` (MIT), and `rdev` (MIT). Their upstream license files remain applicable. Locked versions are in `src-tauri/Cargo.lock`.

Research: https://github.com/cjpais/Handy/issues/502 (old clipboard pasted), https://github.com/cjpais/Handy/issues/2051 (held modifiers affect Ctrl+V), https://handy.computer/docs/paste-methods (increase delay for stale pastes). The 250 ms post-paste default is Parla's conservative choice, not a guarantee of consumer receipt.

Windows listener dependency: https://github.com/Narsil/rdev (rdev 0.5.3, MIT, Copyright (c) 2020 Nicolas Patry). Used as a library, no rdev source copied. The raw-event hold state and key conversion table are original Parla code.

## PARLA-2 fix round 1: keyboard-only Windows hook

Replaced the rdev runtime dependency with `windows-sys` 0.60.2 (Apache-2.0 OR MIT). The keyboard-only hook and watchdog are original Parla code following Microsoft's API documentation:

- https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelkeyboardproc
- https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-kbdllhookstruct
- https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getasynckeystate

The virtual-key mapping table in `src-tauri/src/hotkey.rs` is adapted from rdev 0.5.3 `src/windows/keycodes.rs`, with the missing right Windows key added. Source: https://docs.rs/crate/rdev/0.5.3/source/src/windows/keycodes.rs . Its MIT license is retained below:

```text
MIT License

Copyright (c) 2020 Nicolas Patry

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Handy and handy-recorder

- Repository: https://github.com/cjpais/Handy (MIT) and its recorder crate
  https://github.com/handy-computer/handy-recorder (MIT).
- Taken: the recorder design and, adapted, the resampling approach in
  `src-tauri/src/recorder.rs` — the pre-warmed stream kept on a dedicated
  thread (the cpal `Stream` is `!Send`), mixing device channels down to mono,
  resampling to 16 kHz with rubato's windowed FFT resampler
  (`BlackmanHarris2`, 1024-sample chunks) and draining its delay line at stop,
  and reopening the device on the next `start()` after a failure.
- Original copyright: Copyright (c) 2026 handy-computer (MIT); Handy by CJ
  Pais (MIT).

## Recorded fixture

- `src-tauri/tests/fixtures/hello.wav` was generated locally with espeak-ng
  1.52.0 (GPLv3), then resampled to 16 kHz mono with ffmpeg. It is synthesized
  audio output, not copied source code or third-party recorded material.
