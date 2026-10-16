# Parla local-model bench: Parla's current ONNX path vs transcribe-cpp GGUF.
# Run from this folder:  powershell -ExecutionPolicy Bypass -File .\run-bench.ps1
# First run downloads ~4.6 GB of models into .\models. Close other heavy apps;
# plug in the charger. Results: .\results\summary.txt
param([switch]$SkipStream)
$ErrorActionPreference = 'Continue'
$root = $PSScriptRoot
Set-Location $root
$models = Join-Path $root 'models'
$results = Join-Path $root 'results'
New-Item -ItemType Directory -Force -Path $results, "$models\onnx-v3" | Out-Null

function Get-Model($url, $file) {
    if (-not (Test-Path $file) -or (Get-Item $file).Length -eq 0) {
        Write-Host "download $(Split-Path $file -Leaf)"
        curl.exe -fsSL --retry 3 -o $file $url
        if ($LASTEXITCODE -ne 0) { throw "download failed: $url" }
    }
}
$v3 = 'https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce'
foreach ($f in 'encoder-model.int8.onnx', 'decoder_joint-model.int8.onnx', 'nemo128.onnx', 'vocab.txt') {
    Get-Model "$v3/$f" "$models\onnx-v3\$f"
}
$hc = 'https://huggingface.co/handy-computer'
Get-Model "$hc/parakeet-ultra-gguf/resolve/main/parakeet-ultra-Q8_0.gguf" "$models\ultra-Q8_0.gguf"
Get-Model "$hc/parakeet-ultra-gguf/resolve/main/parakeet-ultra-Q4_K_M.gguf" "$models\ultra-Q4_K_M.gguf"
# English-only candidates (the "English only" option).
foreach ($f in 'encoder-model.int8.onnx', 'decoder_joint-model.int8.onnx', 'nemo128.onnx', 'vocab.txt') {
    New-Item -ItemType Directory -Force -Path "$models\onnx-v2" | Out-Null
    Get-Model "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v2-onnx/resolve/main/$f" "$models\onnx-v2\$f"
}
Get-Model "$hc/parakeet-tdt-0.6b-v2-gguf/resolve/main/parakeet-tdt-0.6b-v2-Q8_0.gguf" "$models\v2-Q8_0.gguf"
Get-Model "$hc/parakeet-unified-en-0.6b-gguf/resolve/main/parakeet-unified-en-0.6b-Q8_0.gguf" "$models\unified-en-Q8_0.gguf"
if (-not $SkipStream) {
    Get-Model "$hc/nemotron-3.5-asr-streaming-0.6b-gguf/resolve/main/nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf" "$models\nemo35-Q8_0.gguf"
}

$static = Join-Path $root 'static\asr-bench.exe'
$dl = Join-Path $root 'dl\asr-bench.exe'
$vk = Join-Path $root 'vk\asr-bench.exe'
$hasVk = Test-Path $vk
function Run($exe, $name, $mode, $model, $arg, $lang, $tail) {
    $wavs = @((Get-ChildItem "$root\data\$lang\*.wav").FullName)
    $env:OUT = Join-Path $results "$name-$lang.tsv"
    if ($tail) { $env:TAIL_S = $tail } else { Remove-Item Env:TAIL_S -ErrorAction SilentlyContinue }
    Write-Host "run $name ($lang)"
    & $exe $mode $model $arg @wavs 2>> (Join-Path $results 'log.txt')
    if ($LASTEXITCODE -ne 0) { Write-Warning "$name-$lang exited with $LASTEXITCODE" }
}
foreach ($lang in 'it', 'en') {
    Run $static 'onnx-v3' onnx "$models\onnx-v3" '-' $lang
    Run $static 'static-ultra-q8' gguf "$models\ultra-Q8_0.gguf" '-' $lang
    Run $dl 'dl-ultra-q8' gguf "$models\ultra-Q8_0.gguf" '-' $lang
    Run $dl 'dl-ultra-q4km' gguf "$models\ultra-Q4_K_M.gguf" '-' $lang
    if (-not $SkipStream) { Run $dl 'stream-nemo35-r6' stream "$models\nemo35-Q8_0.gguf" '6' $lang }
    # Key-up cost of Parla's live mode: last phrase (~2.5 s) plus 2 s of context.
    Run $static 'tail-onnx-v3' onnx "$models\onnx-v3" '-' $lang '4.5'
    Run $dl 'tail-dl-ultra-q8' gguf "$models\ultra-Q8_0.gguf" '-' $lang '4.5'
    Run $dl 'tail-dl-ultra-q4km' gguf "$models\ultra-Q4_K_M.gguf" '-' $lang '4.5'
    Run $static 'tail-static-ultra-q8' gguf "$models\ultra-Q8_0.gguf" '-' $lang '4.5'
    if ($hasVk) {
        Run $vk 'vk-ultra-q8' gguf "$models\ultra-Q8_0.gguf" '-' $lang
        Run $vk 'tail-vk-ultra-q8' gguf "$models\ultra-Q8_0.gguf" '-' $lang '4.5'
    }
    if ($lang -eq 'en') {
        Run $static 'en-onnx-v2' onnx "$models\onnx-v2" '-' $lang
        Run $dl 'en-dl-v2-q8' gguf "$models\v2-Q8_0.gguf" '-' $lang
        Run $dl 'en-dl-unified-q8' gguf "$models\unified-en-Q8_0.gguf" '-' $lang
        Run $static 'tail-en-onnx-v2' onnx "$models\onnx-v2" '-' $lang '4.5'
        Run $dl 'tail-en-dl-v2-q8' gguf "$models\v2-Q8_0.gguf" '-' $lang '4.5'
    }
}
Remove-Item Env:OUT, Env:TAIL_S -ErrorAction SilentlyContinue
$cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1)
$summary = @("CPU: $($cpu.Name) ($($cpu.NumberOfCores) cores, $($cpu.NumberOfLogicalProcessors) threads)")
foreach ($lang in 'it', 'en') {
    $files = @((Get-ChildItem "$results\*-$lang.tsv").FullName)
    $summary += "== $lang (WER; p50/p90 = key-up ms per clip, finalize() for stream-*; tail-* rows: latency only, their WER is meaningless)"
    $summary += & $static score "$root\data\$lang\refs.tsv" @files 2>$null
}
$summary | Set-Content -Encoding ascii (Join-Path $results 'summary.txt')
$summary | ForEach-Object { Write-Host $_ }
"devices:" | Out-Host; Get-Content (Join-Path $results 'log.txt') | Select-String 'devices' | Select-Object -First 2 | Out-Host
