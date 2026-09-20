# Builds the release binaries and packages them into dist\RadioTrackSplitter-Setup-<version>.exe.
# Requires NSIS (winget install NSIS.NSIS) and the Rust toolchain.
param([switch]$SkipCargo)

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

$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
New-Item -ItemType Directory (Join-Path $root 'dist') -Force | Out-Null

& $makensis /V2 "/DVERSION=$version" (Join-Path $here 'radio_track_splitter.nsi')
if ($LASTEXITCODE) { throw "makensis failed" }
Get-Item (Join-Path $root "dist\RadioTrackSplitter-Setup-$version.exe") | Select-Object FullName, Length
