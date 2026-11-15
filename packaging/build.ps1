# Builds the release exe, the portable zip and the installer into dist\.
#   powershell -ExecutionPolicy Bypass -File packaging\build.ps1 [-NoInstaller]
# Needs: Rust (rust-toolchain.toml), Inno Setup 6 for the installer (winget install JRSoftware.InnoSetup).
param([switch]$NoInstaller)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $version = [regex]::Match((Get-Content Cargo.toml -Raw), '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
    Write-Host "== HWiNFO Viewer $version"
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { $env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path" }

    cargo build --release
    if ($LASTEXITCODE) { throw "cargo build failed" }

    $dist = Join-Path $root "dist"
    $stage = Join-Path $dist "HWViewer"
    Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force $stage | Out-Null
    Copy-Item target\release\hwviewer.exe, README.md $stage

    # portable: portable.txt next to the exe keeps all data in .\data (no installer, no registry)
    $port = Join-Path $dist "portable"
    Remove-Item -Recurse -Force $port -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force $port | Out-Null
    Copy-Item "$stage\*" $port
    Set-Content -Path (Join-Path $port "portable.txt") -Encoding ascii -Value "Data is stored in the data folder next to hwviewer.exe."
    $zip = Join-Path $dist "HWiNFO-Viewer-$version-portable.zip"
    Remove-Item $zip -ErrorAction SilentlyContinue
    Compress-Archive -Path "$port\*" -DestinationPath $zip
    Write-Host "portable: $zip"

    if (-not $NoInstaller) {
        $iscc = @("${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe") |
            Where-Object { Test-Path $_ } | Select-Object -First 1
        if (-not $iscc) { throw "Inno Setup 6 not found (winget install JRSoftware.InnoSetup), or pass -NoInstaller" }
        & $iscc /Q "/DAppVersion=$version" packaging\installer.iss
        if ($LASTEXITCODE) { throw "ISCC failed" }
        Write-Host "installer: $(Join-Path $dist "HWiNFO-Viewer-Setup-$version.exe")"
    }
} finally {
    Pop-Location
}
