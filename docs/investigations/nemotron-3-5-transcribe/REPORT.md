# Nemotron 3.5 Transcribe as a Parla engine

Date: 2026-10-28
Status: investigation and decision record. No production code.
Source: <https://x.com/nvidiaai/status/2115462300241141882>
Checkpoint: [`nvidia/NVIDIA-Nemotron-3.5-Transcribe-30B-A4B`](https://huggingface.co/nvidia/NVIDIA-Nemotron-3.5-Transcribe-30B-A4B), revision `c421d6a` (2026-10-28)

## Verdict

**No. It is not a drop-in for Parla's local engine, and it should not replace
Parakeet.**

The short version: a 34B-parameter (4.3B active) GPU-only offline speech LLM
with a 67.5 GB BF16 checkpoint cannot replace Parla's 0.6B (640 MiB) CPU-only
ONNX model under the engine rule in `AGENTS.md`. The licence and the provider
interface are compatible; the runtime, hardware, download size and lack of
streaming are not.

A "yes" would need an ONNX or GGUF checkpoint small enough for CPU inference,
loadable by `transcribe-rs`, producing a transcript from a short clip with
sub-second key-up. None of those exist for this model today.

## Why "drop-in" is a hard rule

`AGENTS.md` is explicit:

> Keep the engine (`transcribe-rs` on ONNX Runtime via `ort`) and the native
> dependencies. A new model goes in as a drop-in on that engine; any engine or
> native-dependency swap waits for Mattia's decision.

The current local model contract, from `docs/guide.md` and
`src-tauri/src/models.rs`:

- Four files in the istupakov ONNX layout (encoder, decoder, `nemo128.onnx`,
  `vocab.txt`), 670,619,706 bytes, downloaded with pinned sizes and SHA-256.
- Loaded by `ParakeetModel::load` through `transcribe-rs` and ONNX Runtime;
  inference is CPU-only and needs no GPU.
- About 700 MB resident, warmed in the background, woken on key-down.
- Phrase-by-phrase: key-up tail p50 0.37 s for a 12.7 s clip on an i7-1370P
  (0.86 s for 37 s) with live dictation enabled.

That is the bar a replacement has to clear, not just "can it produce text?".

## What the model is

From the model card and the Hugging Face API, checked 2026-10-28:

| Property | Nemotron 3.5 Transcribe |
| --- | --- |
| Parameters | 34B total, 4.3B active, including the audio encoder |
| Architecture | Mamba-2 + Transformer hybrid MoE language model with a parallel expert encoder (ASR + speaker) |
| Public checkpoint | BF16 `model.safetensors`, 67.5 GB (about 63 GiB) |
| Other precisions | FP8 and NVFP4 variants are linked from the card, but both repos return HTTP 401 today (gated or not yet public) |
| Context | 65,536 tokens; recordings up to one hour |
| Tasks | Transcription, contextual prompting, speaker attribution/diarization, translation, timestamps, language ID, audio QA |
| Languages | 40 |
| Runtime | vLLM 0.32.0 + NeMo Speech main + `VLLM_PLUGINS=nemo_speechlm`; or Hugging Face Transformers |
| Windows | vLLM/Transformers through WSL2; GGML only on native Windows for RTX 5090 / RTX PRO 6000 |
| Deployment in the card | 1× H100 80 GB |
| Streaming | No. The card calls it an **offline** model that processes recordings; requests are `stream=false` |
| Licence | OpenMDW-1.1 (permissive, commercial use allowed, outputs unrestricted) |
| Reported speed | 0.13 s for a 30 s recording on an RTX PRO 6000 with NVIDIA's NIM; the open-source vLLM path is "roughly two times slower" |

Two details matter more than the headline parameter count:

1. **MoE reduces compute, not memory.** All 34B experts must be resident unless
   they are paged from system RAM, which would be slower than realtime. BF16 is
   67.5 GB; an FP8 build would be about 34 GB and NVFP4 about 17 GB, plus the
   encoder, KV cache and runtime.
2. **There is no shipping Windows inference path in the repo.** The card's GGML
   column lists native Windows for an RTX 5090 or RTX PRO 6000, but the repo
   contains only safetensors and a tokenizer. A Hugging Face search on
   2026-10-28 found no GGUF or ONNX conversion of this checkpoint.
   `transcribe-rs` supports Parakeet, Canary, Cohere, Moonshine, SenseVoice,
   GigaAM and Whisper, not this Mamba-2 speech LLM.

NVIDIA's accuracy claims are also aimed at different work than Parla's: 4.71
mean WER on the English Open ASR Leaderboard, 10.98 on long-form, 4.62 on the
multilingual leaderboard, and 15.80 cpWER on multi-speaker meeting sets. Those
are long recordings, meetings and translation. Parla sends 5-30 s single-speaker
dictation clips.

## Scorecard against Parla's local contract

| Parla requirement | Today (Parakeet) | Nemotron 3.5 Transcribe | Pass? |
| --- | --- | --- | --- |
| Same engine (`transcribe-rs` + ONNX Runtime) | Yes | No NeMo/vLLM/GGML path; no ONNX export | **No** |
| Download size a normal user will accept | 640 MiB | 67.5 GB BF16; ~17 GB NVFP4 even if published | **No** |
| Runs on any Windows x64 laptop CPU | Yes | RTX 5090 32 GB / H100 80 GB class; WSL2 for the supported runtimes | **No** |
| No Python, WSL or extra service | Yes | Python + vLLM + NeMo, or a WSL2 distro | **No** |
| Sub-second key-up on a laptop | 0.37 s at 12.7 s | 0.13 s/30 s only on an RTX PRO 6000 with NIM; unmeasured on laptops | **No** |
| Live captions / phrase streaming | Yes | Offline only | **No** |
| Plain transcript for insertion | Yes | Chat-completions response with prompt-controlled formatting | **No**, without an adapter |
| Commercial use and redistribution | CC BY 4.0 | OpenMDW-1.1, allowed with licence/notices | **Yes** |

The licence is the only clean pass. That does not help when the model cannot
run in the product.

## What integration would actually take

### Option A: a remote OpenAI-compatible provider

`SttProvider` is a small trait (`clip` in, `String` out), so a provider that
posts to a user-run vLLM server is maybe 150 lines. But this is a **new
provider**, not a drop-in model:

- The user must run a 67.5 GB model on an NVIDIA GPU with vLLM 0.32.0 and NeMo
  Speech, on Linux or WSL2 (native Windows only through GGML, with no published
  GGUF).
- Parla would need server lifecycle, health checks, first-request warm-up and
  the existing batch fallback; live captions would still be unavailable.
- No hosted NVIDIA endpoint for this checkpoint was found on 2026-10-28
  (`build.nvidia.com` has no page for it), so every user would build the GPU
  stack themselves.

That is a feature for the small subset of users with a 5090/H100 and a WSL2
setup, not a Windows dictation default.

### Option B: wait for a GGUF/ONNX export and `transcribe-rs` support

Even then, a 34B model at NVFP4 is ~17 GB of VRAM, so it stays a 5090-class
feature. A CPU-only quantisation would be far below realtime. This would also
be an engine/native-dependency change, which the product rule says waits for
Mattia.

### Option C: use the small streaming sibling instead

The tweet's model has a 0.6B streaming sibling,
`nvidia/nemotron-3.5-asr-streaming-0.6b`. It was already benchmarked in
PARLA-36: finalize cost was good (70-85 ms), but accuracy was about 2× worse
than Parakeet on both Italian and English (6.77/9.40 WER at the best setting
against 3.81/7.60 for Parla's v3 ONNX on the same ARM clips), and it writes
numbers as words. It was rejected for the same reason then. Nothing about the
30B changes that comparison.

## Recommendation

1. **Do not change the local engine.** Keep Parakeet v3/Ultra and
   `transcribe-rs`/ONNX.
2. **Do not add Nemotron 3.5 Transcribe as a local provider.** The cost is a
   second runtime, a GPU requirement and a 20-67 GB download for a model that
   cannot stream and has no measured laptop latency.
3. **Revisit only if** NVIDIA publishes an ONNX/GGUF checkpoint with a CPU
   path, a sub-1B variant, or native streaming; or if a hosted API appears that
   Parla can call without shipping a GPU stack.
4. **If high-accuracy batch transcription of long multilingual recordings**
   becomes a product goal, treat it as a separate server-side feature and
   benchmark it against Parakeet Ultra and Cohere on Parla's own clips. It is
   not a dictation-engine decision.

## Not tested locally

This checkout has no NVIDIA GPU (`nvidia-smi` is absent) and 15 GB of RAM, so
the checkpoint could not be loaded. Every performance figure above is NVIDIA's
published number or a PARLA-36 measurement of the 0.6B sibling, not a Parla
measurement of the 30B model.

## Sources

- NVIDIA AI announcement, 2026-10-28:
  <https://x.com/nvidiaai/status/2115462300241141882>
- Model card and files:
  <https://huggingface.co/nvidia/NVIDIA-Nemotron-3.5-Transcribe-30B-A4B>
- Licence:
  <https://huggingface.co/nvidia/NVIDIA-Nemotron-3.5-Transcribe-30B-A4B/blob/main/LICENSE>
  and <https://openmdw.ai/license/1-1/>
- `transcribe-rs` supported engines:
  <https://github.com/cjpais/transcribe-rs>
- PARLA-36 report (local models, streaming sibling, laptop/Windows benchmarks):
  `docs/investigations/parla-36/REPORT.md` on `parla-36-local-engine`
- Parla local-model contract: `docs/guide.md` § Local model, `AGENTS.md`,
  `src-tauri/src/models.rs`, `src-tauri/src/provider/parakeet.rs`
