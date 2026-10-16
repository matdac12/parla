# Builds the bench kit on a Windows x64 dev machine (Rust MSVC + VS Build Tools,
# CMake comes with VS). Run from this folder:
#   powershell -ExecutionPolicy Bypass -File .\build-kit.ps1 [-Clips 100] [-Vulkan]
# Produces .\kit\{static,dl,vk}\asr-bench.exe and .\kit\data\{it,en} (FLEURS test clips).
param([int]$Clips = 100, [switch]$Vulkan)
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
$kit = Join-Path $PSScriptRoot 'kit'

function Copy-Build($targetDir, $name) {
    $dest = Join-Path $kit $name
    New-Item -ItemType Directory -Force -Path $dest | Out-Null
    Copy-Item "$targetDir\release\asr-bench.exe" $dest -Force
    # ONNX Runtime companions (static/) or ggml backend modules (dl/, vk/).
    Get-ChildItem "$targetDir\release" -Recurse -Filter *.dll |
        Where-Object { $_.FullName -notmatch '\\deps\\' } |
        ForEach-Object { Copy-Item $_.FullName $dest -Force }
    Write-Host "$name -> $((Get-ChildItem $dest).Name -join ', ')"
}

# static: transcribe-cpp linked into the exe, portable AVX2 floor (never GGML_NATIVE:
# a build tuned to an AVX-512 machine would SIGILL elsewhere). This is how Parla would ship.
$env:TRANSCRIBE_CMAKE_ARGS = '-DGGML_NATIVE=OFF -DGGML_AVX=ON -DGGML_AVX2=ON -DGGML_FMA=ON -DGGML_F16C=ON -DGGML_BMI2=ON'
cargo build --release --target-dir target
if ($LASTEXITCODE) { throw 'static build failed' }
Remove-Item Env:TRANSCRIBE_CMAKE_ARGS
Copy-Build 'target' 'static'

# dl: Handy's posture, one ggml-cpu DLL per x86 ISA tier, chosen at runtime.
cargo build --release --features dl --target-dir target-dl
if ($LASTEXITCODE) { throw 'dl build failed' }
Copy-Build 'target-dl' 'dl'

if ($Vulkan) {
    # Needs the Vulkan SDK (VULKAN_SDK set, glslc on PATH): https://vulkan.lunarg.com/sdk/home#windows
    cargo build --release --features vk --target-dir target-vk
    if ($LASTEXITCODE) { throw 'vk build failed (is the Vulkan SDK installed?)' }
    Copy-Build 'target-vk' 'vk'
}

Copy-Item run-bench.ps1 $kit -Force
foreach ($pair in @(@('it_it', 'it'), @('en_us', 'en'))) {
    $dir = Join-Path $kit "data\$($pair[1])"
    if (-not (Test-Path "$dir\refs.tsv")) {
        & "$kit\static\asr-bench.exe" fleurs $pair[0] $Clips $dir
        if ($LASTEXITCODE) { throw "FLEURS download failed for $($pair[0])" }
    }
}
Write-Host "Kit ready: $kit. Next: powershell -ExecutionPolicy Bypass -File .\kit\run-bench.ps1"
