# Sanity checks on a finished build. CI runs this after build.ps1; it works locally too.
#   - dist\ holds exactly one installer and its version is the one in Cargo.toml
#   - on a tag build (GITHUB_REF_TYPE=tag) the tag is v<that version>
#   - the exes do not import the Visual C++ runtime. If they do, .cargo\config.toml (which
#     links the C runtime statically) was not applied, and the app would need the
#     redistributable installed on users' machines.
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent

Push-Location $root
try { $metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json; if ($LASTEXITCODE) { throw "cargo metadata failed" } }
finally { Pop-Location }
$version = @($metadata.packages)[0].version

# --- installer -------------------------------------------------------------
$installers = @(Get-ChildItem (Join-Path $root 'dist') -Filter 'RadioTrackSplitter-Setup-*.exe' -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -notlike '*-test.exe' })
if ($installers.Count -ne 1) { throw "expected one installer in dist\, found $($installers.Count)" }
$installer = $installers[0]
if ($installer.Name -ne "RadioTrackSplitter-Setup-$version.exe") {
    throw "$($installer.Name) does not match Cargo.toml version $version (stale build?)"
}
$embedded = $installer.VersionInfo.ProductVersion
if ($embedded -ne $version) { throw "installer says version $embedded, Cargo.toml says $version" }
Write-Host "ok: $($installer.Name) ($([math]::Round($installer.Length / 1MB, 1)) MB), version $version"

# --- tag -------------------------------------------------------------------
if ($env:GITHUB_REF_TYPE -eq 'tag') {
    if ($env:GITHUB_REF_NAME -ne "v$version") {
        throw "tag $($env:GITHUB_REF_NAME) does not match Cargo.toml version $version (expected v$version)"
    }
    Write-Host "ok: tag $($env:GITHUB_REF_NAME) matches Cargo.toml"
}

# --- static C runtime ------------------------------------------------------
foreach ($name in 'radio-track-splitter.exe', 'radio-track-splitter-cli.exe') {
    $path = Join-Path $root "target\release\$name"
    $text = [Text.Encoding]::GetEncoding(28591).GetString([IO.File]::ReadAllBytes($path))
    if ($text.IndexOf('VCRUNTIME140', [StringComparison]::OrdinalIgnoreCase) -ge 0) {
        throw "$name imports VCRUNTIME140.dll: the static C runtime setting in .cargo\config.toml was not applied (is it committed?)"
    }
    Write-Host "ok: $name does not need the Visual C++ runtime"
}
