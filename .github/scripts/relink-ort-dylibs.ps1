# Recreate the DLL links that ort-sys's build script watches, after a cache restore.
#
# ort-sys (feature copy-dylibs) links target/<profile>/{.,deps,examples}/DirectML.dll
# to its download dir and prints `cargo:rerun-if-changed` for each link. rust-cache
# prunes those links when it saves the target dir, so on every cached run cargo
# found them missing, reran ort-sys and rebuilt ort -> transcribe-rs -> parla
# (PARLA-34). With the download dir restored by actions/cache, putting the links
# back keeps ort-sys fresh. Anything missing is skipped: ort-sys then reruns and
# downloads as before, so this can only save work, never break the build.
param([string]$TargetDir = 'src-tauri/target')

$ErrorActionPreference = 'Stop'
$searchPrefix = 'cargo:rustc-link-search=native='
$watchPrefix = 'cargo:rerun-if-changed='
$linked = 0

foreach ($output in @(Get-ChildItem -Path (Join-Path $TargetDir '*/build/ort-sys-*/output') -File -ErrorAction SilentlyContinue)) {
  $lines = Get-Content -LiteralPath $output.FullName
  $search = $lines | Where-Object { $_.StartsWith($searchPrefix) } | Select-Object -First 1
  if (!$search) { continue }
  $binDir = $search.Substring($searchPrefix.Length)

  foreach ($line in $lines | Where-Object { $_.StartsWith($watchPrefix) }) {
    $link = $line.Substring($watchPrefix.Length)
    $source = Join-Path $binDir (Split-Path $link -Leaf)
    if (!(Test-Path -LiteralPath $source -PathType Leaf)) { continue }

    # Cargo reruns the script if a watched file is newer than the script's start
    # (the output file's mtime). The DLL was extracted during the first run, so
    # clamp it. Links resolve to their target's mtime.
    $dll = Get-Item -LiteralPath $source
    if ($dll.LastWriteTimeUtc -gt $output.LastWriteTimeUtc) {
      $dll.LastWriteTimeUtc = $output.LastWriteTimeUtc
    }

    if (Get-Item -LiteralPath $link -Force -ErrorAction SilentlyContinue) { continue }
    New-Item -ItemType Directory -Path (Split-Path $link) -Force | Out-Null
    try {
      New-Item -ItemType SymbolicLink -Path $link -Target $source | Out-Null
    } catch {
      # ort-sys falls back to a copy without symlink rights; match it.
      Copy-Item -LiteralPath $source -Destination $link
    }
    $linked++
  }
}

Write-Host "Relinked $linked ort-sys DLL path(s)."
