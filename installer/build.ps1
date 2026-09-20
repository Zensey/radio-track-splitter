# Builds the release binaries and packages them into dist\RadioTrackSplitter-Setup-<version>.exe.
# The version comes from Cargo.toml (the single source of truth); nothing else defines it.
# Requires NSIS (winget install NSIS.NSIS) and the Rust toolchain.
#   -SkipCargo       package the binaries already in target\release
#   -ForceDownloads  test build (dist\...-test.exe) that never skips the weights download
#                    just because the file is already on this PC
param([switch]$SkipCargo, [switch]$ForceDownloads)

$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$root = Split-Path $here -Parent

$makensis = (Get-Command makensis -ErrorAction SilentlyContinue).Source
if (-not $makensis) {
    $makensis = @("${env:ProgramFiles(x86)}\NSIS\makensis.exe", "$env:ProgramFiles\NSIS\makensis.exe") |
        Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $makensis) { throw "makensis not found. Install it with: winget install NSIS.NSIS" }

# NScurl gives the installer HTTPS downloads with a progress bar (stock NSISdl has no TLS).
$pluginDll = Join-Path $here 'plugins\x86-unicode\NScurl.dll'
if (-not (Test-Path $pluginDll)) {
    $url  = 'https://github.com/negrutiu/nsis-nscurl/releases/download/v26.8.30.320/NScurl.zip'
    $sha  = 'DEDEFCBBEE724CB463F1000826B688116CB3DB99C26A975E6D04F5CC88D3D2A7'
    $zip  = Join-Path $env:TEMP 'NScurl.zip'
    Write-Host "Fetching NScurl plugin..."
    Invoke-WebRequest $url -OutFile $zip
    if ((Get-FileHash $zip -Algorithm SHA256).Hash -ne $sha) { throw "NScurl.zip hash mismatch" }
    $tmp = Join-Path $env:TEMP 'NScurl-extract'
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory $tmp | Out-Null
    tar -xf $zip -C $tmp Plugins/x86-unicode/NScurl.dll
    if ($LASTEXITCODE) { throw "could not extract NScurl.zip" }
    New-Item -ItemType Directory (Split-Path $pluginDll) -Force | Out-Null
    Copy-Item (Join-Path $tmp 'Plugins\x86-unicode\NScurl.dll') $pluginDll
    Remove-Item $tmp -Recurse -Force
    Remove-Item $zip
}

if (-not $SkipCargo) {
    Push-Location $root
    # cargo reports progress on stderr, which Windows PowerShell 5.1 treats as an error under 'Stop'.
    $ErrorActionPreference = 'Continue'
    try { cargo build --release --bins; if ($LASTEXITCODE) { throw "cargo build failed" } }
    finally { Pop-Location; $ErrorActionPreference = 'Stop' }
}

# Ask cargo rather than parsing Cargo.toml by hand.
Push-Location $root
try { $metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json; if ($LASTEXITCODE) { throw "cargo metadata failed" } }
finally { Pop-Location }
$packages = @($metadata.packages)
if ($packages.Count -ne 1) { throw "expected one package in Cargo.toml, found $($packages.Count)" }
$version = $packages[0].version
if (-not $version) { throw "could not read the package version from Cargo.toml" }
# The Windows version resource only takes numbers: 0.2.0-beta -> 0.2.0.
$versionNum = $version -replace '[-+].*$', ''
if ($versionNum -notmatch '^\d+\.\d+\.\d+$') { throw "Cargo.toml version '$version' is not major.minor.patch" }

New-Item -ItemType Directory (Join-Path $root 'dist') -Force | Out-Null
$suffix = if ($ForceDownloads) { '-test' } else { '' }
$outFile = Join-Path $root "dist\RadioTrackSplitter-Setup-$version$suffix.exe"

$defs = @("/DVERSION=$version", "/DVERSION_NUM=$versionNum", "/DOUTFILE=$outFile")
if ($ForceDownloads) { $defs += '/DFORCE_DOWNLOADS' }
& $makensis /V2 @defs (Join-Path $here 'radio_track_splitter.nsi')
if ($LASTEXITCODE) { throw "makensis failed" }
Get-Item $outFile | Select-Object FullName, Length
