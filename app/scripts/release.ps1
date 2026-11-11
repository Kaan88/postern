# Builds Postern's Windows installers and lays them out like Bulwark Desktop's releases:
#   Postern-Setup-<version>-<arch>.exe   NSIS, per user, no admin (what most people want)
#   Postern-<version>-<arch>.msi         WiX, per machine (for IT / deployment tools)
#   <file>.sha256                        `sha256sum` format
# Usage (from app/):  powershell -File scripts/release.ps1 [-Arch x64,arm64]
param([string[]]$Arch = @("x64", "arm64"))
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$version = (Get-Content "$root\tauri.conf.json" -Raw | ConvertFrom-Json).version
$dist = Join-Path $root "dist\$version"
New-Item -ItemType Directory -Force $dist | Out-Null
$targets = @{ x64 = "x86_64-pc-windows-msvc"; arm64 = "aarch64-pc-windows-msvc" }
$wixArch = @{ x64 = "x64"; arm64 = "arm64" }

foreach ($a in $Arch) {
    $t = $targets[$a]
    Write-Host "== $a ($t)"
    Push-Location $root
    try {
        cargo tauri build --target $t --bundles nsis,msi
        if ($LASTEXITCODE -ne 0) { throw "build for $a failed" }
    } finally { Pop-Location }
    $bundle = Join-Path $root "target\$t\release\bundle"
    $exe = Get-ChildItem "$bundle\nsis\*.exe" | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    $msi = Get-ChildItem "$bundle\msi\*.msi" | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    Copy-Item $exe.FullName (Join-Path $dist "Postern-Setup-$version-$a.exe") -Force
    Copy-Item $msi.FullName (Join-Path $dist "Postern-$version-$($wixArch[$a]).msi") -Force
}

Get-ChildItem $dist -File | Where-Object { $_.Extension -in ".exe", ".msi" } | ForEach-Object {
    $hash = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower()
    # sha256sum format, LF only, so `sha256sum -c` works on any OS
    [IO.File]::WriteAllText("$($_.FullName).sha256", "$hash  $($_.Name)`n")
}
Get-ChildItem $dist | Format-Table Name, Length
