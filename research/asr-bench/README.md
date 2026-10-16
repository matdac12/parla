# ASR bench (research)

Compares Parla's current local model path (transcribe-rs + ONNX Runtime,
Parakeet v3 int8) with transcribe-cpp running Parakeet Ultra GGUF and
Nemotron 3.5 streaming, on FLEURS Italian and English clips.

On a Windows laptop: download the `parla-asr-bench-kit` artifact from the
latest "ASR bench (research)" run, extract it, and run

    powershell -ExecutionPolicy Bypass -File .\run-bench.ps1

The first run downloads about 2.6 GB of models. It takes 10-20 minutes;
`results\summary.txt` has WER and per-clip key-up latency for every config.
`-SkipStream` skips Nemotron.
