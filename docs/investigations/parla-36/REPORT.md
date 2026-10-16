# PARLA-36: FluidVoice's models and a transcribe-cpp engine for Parla

Date: 2026-10-16. Branch: `parla-36-local-engine`, a prototype and not for merge
as-is. It holds the integration prototype (from `proto/transcribe-cpp`) plus the
benchmark (from `research/fluidvoice-models`).

## TL;DR

1. **FluidVoice's own models can't go into Parla.**
   - **Fluid Speech Mini/Pico** are CoreML-only, English-only Parakeet
     compressions. No Windows path.
   - The **Fluid-1** cleanup LLMs are licensed for personal, non-commercial use
     only, and the licence explicitly forbids research, product and bundling
     use. I deleted the download without running it.
2. **The FluidVoice ecosystem points to two things that do work on Windows:**
   - **Parakeet Ultra**: Moondream's post-trained Parakeet TDT 0.6B v3,
     CC BY 4.0. Same architecture, tokenizer and 25 languages, and it's what
     Fluid Speech Mini is distilled from.
   - **transcribe.cpp / `transcribe-cpp`** (MIT, crates.io 0.4.0): the GGUF/ggml
     runtime Handy moved to on 2026-10-14, when Handy dropped ONNX Runtime
     entirely.
3. **Accuracy: GGUF Parakeet Ultra beats Parla's current local model every
   time.** This held on both machines and both languages. On the Windows x64
   runner, 100 FLEURS clips per language:
   - Italian WER 3.39 % → 2.43 % (Q8_0) or 2.31 % (Q4_K_M).
   - English WER 5.55 % → 4.61 % / 4.43 %.
   - Parla's ONNX int8 path also produced **repetition loops** ("eight zero two
     point one one one one one…", "s s s s"). That was 2 to 4 clips per 200
     here. No GGUF run ever produced one.
4. **Speed is the open question, and it needs your laptop.**
   - On the Linux ARM box, GGUF was **~25 % faster**: the key-up tail went from
     284 → 198 ms p50.
   - On the 2-vCPU AMD EPYC GitHub runner, it was **~35-55 % slower** (tail
     964 → 1438 ms p50). That runner build was tuned with `GGML_NATIVE`.
   - The i7-1370P is a different CPU class (Raptor Lake, 6P+8E, AVX2 +
     AVX-VNNI). Run the kit there before deciding anything.
5. **Windows compatibility is proven.**
   - The prototype app builds and passes all tests with MSVC and the pinned
     rust-lld (CI green).
   - It ships as **one 19.7 MB `parla.exe` with no DLLs**. Today it's 37.7 MB
     plus an 18.5 MB `DirectML.dll`, and the CI artifact zip goes from 22.1 MB
     to 7.0 MB.
   - Windows CI also got faster: `cargo test` 565 → 421-496 s and `tauri build`
     642 → 562-601 s.
   - **This would make PARLA-32 and PARLA-34 obsolete.**
6. **English-only option.** It's real but small.
   - Parakeet Unified EN (4.51 %) and v2 (4.69 %) are only a little better on
     English than multilingual Ultra (4.90 %).
   - Both would break Italian, so it only makes sense as an opt-in setting.
     It's in the laptop kit.
7. **Nemotron 3.5 streaming** (OpenMDW, permissive) has a tiny key-up cost
   (70-85 ms finalize), but it's **2x worse** than Parakeet on Italian and
   English. It also writes numbers as words. Parla's phrase splitting already
   gets most of the streaming benefit, so it's not recommended.
8. **Cohere Transcribe** is the most accurate on Italian when numbers are left
   out (2.06 %), but **3.6x slower** than Parakeet. Not for dictation.

**Recommendation.** Run the laptop kit, then pick one:
- **(A)** If GGUF Ultra's tail p50 is within ~+30 ms of today's ONNX path on the
  i7-1370P: adopt the prototype. That's the accuracy win, no repetition loops,
  one DLL-free exe, and faster CI.
- **(B)** If GGUF is clearly slower on CPU but Vulkan on the Iris Xe is fast:
  adopt with Vulkan, the way Handy ships it on x64.
- **(C)** If both are slower: keep ONNX Runtime for now. Re-test when
  transcribe-cpp improves its x86 kernels. Note that the Ultra **ONNX** int8
  export doesn't fix the loops and its accuracy gain is mixed.

---

## 1. What FluidVoice actually ships

Source: fluidvoice.com, github.com/altic-dev/FluidVoice @ ad1d1ff (2026-10-15),
and the app's model catalog (`ParakeetSpeechModelCatalog.swift`, model feed
`models.fluidvoice.app/feed/v1/speech-models.json`).

| Model in FluidVoice | What it is | Licence | Windows? | Parla? |
|---|---|---|---|---|
| **Fluid Speech Mini** (242 MiB) | Parakeet TDT v3 encoder **distilled from Parakeet Ultra**, palettized 2/3/4-bit, 4-piece CoreML encoder, 15 s window, **English only**, self-reported Open ASR WER 4.37 | CC BY 4.0 (`config.json` inside the tar) | No (CoreML `.mlmodelc` only) | No |
| Fluid Speech Pico (154 MiB) | Same, smaller, WER 4.9 | CC BY 4.0 | No | No |
| Parakeet TDT v2 / v3 | NVIDIA | CC BY 4.0 | yes (ONNX, GGUF) | v3 = today |
| **Parakeet Ultra** (via FluidInference CoreML) | moondream/parakeet-ultra, post-trained v3 | CC BY 4.0 | **yes: GGUF (handy-computer) and ONNX int8 (Masterx, istupakov layout)** | **candidate** |
| Nemotron Speech 3.5 (multilingual, streaming) | nvidia/nemotron-3.5-asr-streaming-0.6b | OpenMDW-1.1 (permissive) | yes (GGUF) | tested, no |
| Parakeet Flash | parakeet_realtime_eou_120m, English | NVIDIA Open Model License | GGUF/ONNX exist | not tested |
| Cohere Transcribe | 2B, 14 languages | Apache-2.0 (gated on HF) | yes (GGUF, ONNX) | tested, too slow |
| **Fluid-1 / Mini / Pico / Quad** cleanup LLMs | Qwen3.5 finetunes (0.8B-4B), GGUF/MLX/CoreML | **personal non-commercial only; research, product, bundling all forbidden** | GGUF exists | **No: licence** |

FluidVoice's macOS app is GPLv3 (Apache-2.0 before 2026-02-23). Its "Windows
pre-build" isn't in the public releases, which are all `.dmg`/`.zip` for macOS.

## 2. Accuracy and speed

Method: FLEURS test split (google/fleurs, CC BY 4.0), the first N clips of the
archive per language.
- WER uses a simple normaliser: lowercase, punctuation dropped, apostrophes
  split words.
- FLEURS references use digits, so models that write numbers as words get
  penalised. "WER no-digit" leaves out reference sentences that contain digits.
- "ms" is pure inference after warm-up, which is what key-up pays.
- Raw per-clip outputs are in `linux-arm/` and `windows-runner/`.

### 2a. Linux ARM (Asahi, M1 Pro 10 cores), 200 clips per language

| config | runtime | it WER | it no-digit | en WER | en no-digit | ms/clip it / en | repetition loops |
|---|---|---:|---:|---:|---:|---:|---:|
| **Parla today**: v3 ONNX int8 | transcribe-rs + ORT | 3.81 | 3.23 | 7.60 | 5.46 | 744 / 500 | 2 |
| Ultra ONNX int8 (drop-in files) | transcribe-rs + ORT | 4.17 | 3.17 | 6.03 | 5.23 | 730 / 497 | 4 |
| v3 GGUF Q8_0 | transcribe-cpp | 3.37 | 3.12 | 5.66 | 4.78 | 572 / 371 | 0 |
| **Ultra GGUF Q8_0** | transcribe-cpp | **2.99** | 2.54 | **4.90** | 4.18 | **564 / 372** | **0** |
| Nemotron 3.5 offline (auto lang) | transcribe-cpp | 6.70 | 4.31 | 10.04 | 7.23 | 659 / 444 | 0 |
| Cohere Transcribe Q4_K_M | transcribe-cpp | 3.09 | **2.06** | 4.98 | 4.03 | 2059 / 1340 | 0 |
| *English-only:* v2 ONNX int8 | transcribe-rs + ORT | n/a | n/a | 4.69 | 3.97 | n/a / 501 | 0 |
| *English-only:* v2 GGUF Q8_0 | transcribe-cpp | n/a | n/a | 4.69 | 4.18 | n/a / 381 | 0 |
| *English-only:* Parakeet Unified EN Q8_0 | transcribe-cpp | n/a | n/a | **4.51** | **3.57** | n/a / 386 | 0 |

Key-up tail (last 4.5 s of 80 clips, i.e. Parla's last phrase plus 2 s of
context): **ONNX v3 284 ms p50 / 314 p90; GGUF Ultra Q8 198 / 206.**

Nemotron 3.5 streaming, 80 clips, 100 ms feeds as Parla sends them:

| att right | finalize p50 / p90 | CPU while talking | it WER | en WER |
|---|---:|---:|---:|---:|
| 13 (1.12 s) | 85 / 104 ms | 9.5 % of realtime | 6.77 | 9.40 |
| 6 | 70 / 80 ms | 11 % | 7.10 | 10.16 |
| 0 | 25 / 29 ms | 34-44 % | 7.81 | 12.51 |
| Nemotron EN-only, 6 | 71 / 82 ms | 11 % | n/a | 8.28 |

### 2b. Windows x64 GitHub runner (AMD EPYC 7763, **1 core / 2 threads**, 8 GB), 100 clips per language

transcribe-cpp was linked statically and built with the default `GGML_NATIVE=ON`
on that runner.

| config | it WER | it no-digit | en WER | en no-digit | ms/clip it / en | realtime x | tail p50 / p90 it |
|---|---:|---:|---:|---:|---:|---:|---:|
| **Parla today**: v3 ONNX int8 | 3.39 | 3.09 | 5.55 | 4.10 | **3168 / 1973** | **4.8** | **964 / 1171** |
| Ultra ONNX int8 | 3.78 | 2.78 | 5.03 | 3.92 | 3223 / 2100 | 4.7 | n/a |
| v3 GGUF Q8_0 | 2.81 | 2.63 | 4.94 | 3.98 | 4956 / 3237 | 3.1 | n/a |
| **Ultra GGUF Q8_0** | 2.43 | 2.12 | 4.61 | 3.74 | 4941 / 3050 | 3.1 | 1438 / 2876 |
| **Ultra GGUF Q4_K_M** | **2.31** | 2.18 | **4.43** | 3.69 | 3755 / 2357 | 4.0 | not run |
| Nemotron 3.5 stream R=6 | 6.82 | 4.65 | 10.30 | 7.72 | finalize 252 | n/a | n/a |

Reading:
- **Accuracy agrees across machines.** The ranking is the same; Ultra GGUF is
  best and has no loops.
- **Speed doesn't agree.** On the shared 2-thread EPYC, ONNX Runtime's int8
  AVX2 kernels beat ggml. On ARM it's the opposite. The runner is ~4x slower
  than your laptop in absolute terms: PARLA-22 measured ~226 ms for the real
  last-phrase inference there, against 964 ms on the runner. So only your
  laptop can settle the speed question.
- Q4_K_M is both **more accurate and faster** than Q8_0 on the runner, and it's
  a 486 MB download against 740 MB. Prefer it if the laptop agrees.

### 2c. Error examples (Linux, English)

| reference | Parla today (ONNX v3 int8) | GGUF |
|---|---|---|
| …compatible with 802.11a, 802.11b and 802.11g… | …eight zero two point one one one one one one one one one one one A, eight zero two point one one one one… | …802.11A, 802.11B and 802.11G… |
| …and form a sort of shell | …in a form of s s s s s sort of shell. | …and form a sort of shell. |
| …foundation of Asunción in 1537… | …foundation of Assumption in fifteen thirty-seven… | …foundation of Asunción in 1537… |

The GGUF of the *same* v3 weights doesn't loop. So the loops come from the
istupakov int8 ONNX quantization (per-tensor dynamic QUInt8) or its decode, not
from the model.

## 3. Windows compatibility

### 3a. Integration prototype (`proto/transcribe-cpp`, merged here)

Diff vs main: `Cargo.toml`, `models.rs`, `provider/parakeet.rs`, `ci.yml` and
`THIRD_PARTY_NOTICES.md`, about +118/-466 lines, mostly `Cargo.lock`.

- `transcribe-rs`, `ort` and `ort-sys` are replaced by
  `transcribe-cpp = { version = "=0.4.0", default-features = false }`: static,
  CPU only.
- The model is a single file, `parakeet-ultra-Q8_0.gguf` (740,363,168 B),
  pinned to HF revision `a1539be1…` with a sha256. It goes through the existing
  download, verify and install code unchanged. Directory:
  `models/parakeet-ultra-q8_0`.
- The provider id is still `parakeet`; the display name is "Parakeet Ultra
  (local)". Settings, phrase streaming, silence trim, wake, release on idle and
  the bench window are untouched.
- `transcribe_after` now uses transcribe-cpp **word** timestamps, keeping words
  with `t0_ms ≥ context − 160 ms`. A new env-gated test
  (`live_context_cut_returns_only_the_new_words`) checks that context words
  are dropped.
- **CI:** Windows `cargo test --locked` and `tauri build` are green, and so is
  Linux fmt, clippy and test (run 38916279060).
  - The artifact step now runs `dumpbin /dependents`. `parla.exe` imports no
    ONNX Runtime or DirectML.
  - It still imports `MSVCP140.dll` and the UCRT, as `main` already does; it
    no longer imports `MSVCP140_1.dll`.
  - The `target/` folder can still hold stale ONNX DLLs restored from `main`'s
    rust-cache. That's why the check reads imports, not the folder.
- **Existing users:** the old `parakeet-tdt-0.6b-v3` folder is ignored, the
  provider shows "model not downloaded", and the user downloads again.
  - A real migration should offer the download and delete the old folder
    (~670 MB).
  - Handy built a "migrate ONNX users to GGUF" UI for exactly this
    (cjpais/Handy#2277).

### 3b. Risks and gotchas found

- **`GGML_NATIVE` must be off for release builds.** The static build defaults
  to `GGML_NATIVE=ON`, which tunes ggml to the CI runner's CPU, and runners
  change hardware. An AVX-512 runner would produce an exe that SIGILLs on the
  i7-1370P, which has no AVX-512. Fix in Parla's CI env:
  `TRANSCRIBE_CMAKE_ARGS=-DGGML_NATIVE=OFF -DGGML_AVX=ON -DGGML_AVX2=ON -DGGML_FMA=ON -DGGML_F16C=ON -DGGML_BMI2=ON`.
  Or use Handy's `dynamic-backends` (per-ISA DLLs picked at runtime), but that
  ships DLLs again. **The prototype CI artifact has this issue; don't hand it
  to anyone else.**
- First builds compile ggml (C++), which needs CMake. VS 2022 Build Tools
  bundle it and CI found it. rust-cache keeps it warm.
- `transcribe-cpp` 0.4 is pre-1.0, so pin it exactly (the prototype does,
  `=0.4.0`).
- On Linux (Arch only) linking needs `-lcblas`. That's a local quirk, not
  relevant to Windows.

### 3c. Effect on open issues

- **PARLA-32** (delay-load DirectML.dll) and **PARLA-34** (ort-sys rebuilds in
  CI): obsolete if the prototype lands.
- **PARLA-22 / PARLA-33 / PARLA-24:** their measurements were made with ONNX.
  Re-measure the tail after a switch.
- The context-1 s idea (PARLA-33) is independent and applies to both engines.

## 4. The laptop kit (`research/asr-bench`)

One binary links **both** engines: Parla's exact transcribe-rs/ORT pins and
transcribe-cpp. That also proves the two can coexist in one MSVC binary, if a
side-by-side transition is wanted.

- `build-kit.ps1 [-Clips 100] [-Vulkan]` builds:
  - `kit\static`: portable AVX2, how Parla would ship.
  - `kit\dl`: dynamic backends, Handy's posture.
  - optionally `kit\vk`: Vulkan, which needs the Vulkan SDK.
  - It also downloads FLEURS clips with the exe itself; no Python needed.
- `kit\run-bench.ps1 [-SkipStream]` downloads about 4.6 GB of models on its
  first run and runs these configs:
  - **Both languages:** today's ONNX, static and dl Ultra Q8, dl Ultra Q4_K_M,
    Nemotron streaming, plus the 4.5 s key-up tails.
  - **Vulkan**, if built.
  - **English only:** v2 ONNX, v2 GGUF and Unified EN, the English-only option.
  - It writes `kit\results\summary.txt` and one TSV per config.
- The modes behind it: `asr-bench onnx|gguf|stream|score|fleurs`. The output
  file comes from `$env:OUT`, and `$env:TAIL_S` crops each clip to its last N
  seconds.

## 5. Licences

| item | licence | obligations |
|---|---|---|
| transcribe.cpp / transcribe-cpp / ggml | MIT | notice (added to THIRD_PARTY_NOTICES on the branch) |
| Parakeet Ultra (moondream), GGUF by handy-computer | CC BY 4.0 | attribution (added) |
| Parakeet TDT v2/v3 (NVIDIA) | CC BY 4.0 | attribution |
| Parakeet Unified EN (NVIDIA) | NVIDIA Open Model License (commercial OK) | keep notice |
| Nemotron 3.5 ASR | OpenMDW-1.1 | keep licence and notices |
| Fluid Speech Mini/Pico | CC BY 4.0 | n/a (CoreML only) |
| Fluid-1 family | personal non-commercial; no research/product/bundling | **don't use** |
| FLEURS (test audio) | CC BY 4.0 | used only for the bench |

## 6. Not verified

- Anything on the real i7-1370P. This is the point of the kit.
- Vulkan on Iris Xe.
- Real dictations through the prototype app: hotkey → paste with Ultra, and
  live phrases with word-timestamp cuts on real speech, not just the fixture.
- Code-switched IT/EN sentences. FLEURS is single-language per clip.
- Accuracy on Parla's vocabulary feature: Parakeet in transcribe-cpp has no
  vocabulary biasing. Neither does today's ONNX path.
- Memory: resident set of the GGUF session vs ORT (~700 MB today).

## 7. Commands and raw data

- Linux raw TSVs: `linux-arm/` (refs in `refs-it.tsv` / `refs-en.tsv`).
- Windows runner TSVs: `windows-runner/`, from run 38911897937; its scoring
  step failed on a script error and was re-scored locally.
- Scorers: `research/asr-bench/wer.py` (Python, includes "no-digit") and
  `asr-bench score` (Rust, used by the kit).
