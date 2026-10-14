# Third-party notices

No third-party source code or artwork has been copied into this scaffold. The placeholder icon is original to Parla.

Dependencies retain their own licenses; dependency versions are recorded in `package-lock.json` and `src-tauri/Cargo.lock`.

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
