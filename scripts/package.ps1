<#
.SYNOPSIS
  Builds everything and assembles the distributable.

.DESCRIPTION
  Produces dist\SessionRestore-<version>\ containing the installer, the binaries it
  installs, and the unpacked extension, plus a zip of the same.

  There is no MSI. Session Restore is a per-user application: its data directory, its
  registry keys, its logon task and its Start Menu entry are all per-user (ADR-0001),
  so the installer needs no administrator and an MSI would only add a dependency and
  an elevation prompt for a privilege the product never uses.

.PARAMETER Sign
  Sign the executables with the certificate in $env:SR_SIGN_THUMBPRINT. Unsigned is
  the default because a certificate is a purchase, not a build step.

.EXAMPLE
  .\scripts\package.ps1
  .\scripts\package.ps1 -Sign
#>
[CmdletBinding()]
param(
    [switch]$Sign,
    [switch]$SkipExtension
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

function Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Warn($msg) { Write-Host "    $msg" -ForegroundColor Yellow }

# Runs a native command and fails on its exit code, not on its stderr.
#
# Windows PowerShell wraps every stderr line from a native executable in an
# ErrorRecord, and with ErrorActionPreference = Stop that turns cargo's ordinary
# "Compiling ..." progress into a fatal error. The exit code is the only honest
# signal a native tool gives.
function Invoke-Native {
    param([Parameter(Mandatory)][string]$Exe, [string[]]$Arguments = @())
    $prev = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        & $Exe @Arguments
        if ($LASTEXITCODE -ne 0) {
            throw "$Exe $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
        }
    } finally {
        $ErrorActionPreference = $prev
    }
}

# ---------------------------------------------------------------- version
$cargoToml = Get-Content (Join-Path $root "agent\Cargo.toml") -Raw
if ($cargoToml -notmatch '(?m)^version\s*=\s*"([^"]+)"') {
    throw "could not read the version out of agent\Cargo.toml"
}
$version = $Matches[1]
Step "Session Restore $version"

# ---------------------------------------------------------------- agent
Step "Building the agent (release)"
Push-Location (Join-Path $root "agent")
try {
    Invoke-Native cargo @("build", "--release")
} finally {
    Pop-Location
}

# ---------------------------------------------------------------- extension
if (-not $SkipExtension) {
    Step "Building the extension"
    Push-Location (Join-Path $root "extension")
    try {
        if (-not (Test-Path "node_modules")) { Invoke-Native npm @("install") }
        Invoke-Native npm @("run", "build")
    } finally {
        Pop-Location
    }
}

# ---------------------------------------------------------------- assemble
$stage = Join-Path $root "dist\SessionRestore-$version"
Step "Assembling $stage"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Path $stage -Force | Out-Null

$binDir = Join-Path $root "agent\target\release"
$required = @("sr-setup.exe", "sr-agent.exe", "sr-relay.exe")
foreach ($f in $required) {
    $src = Join-Path $binDir $f
    if (-not (Test-Path $src)) { throw "missing build output: $f" }
    Copy-Item $src $stage
}

# Not optional on the GNU toolchain: without it the agent dies before main with
# 0xC0000135 and no log at all. An MSVC build links it statically and has none.
$loader = Join-Path $binDir "WebView2Loader.dll"
if (Test-Path $loader) {
    Copy-Item $loader $stage
} else {
    Warn "no WebView2Loader.dll next to the binaries (expected on an MSVC build)"
}

if (-not $SkipExtension) {
    $extSrc = Join-Path $root "extension\dist"
    if (Test-Path $extSrc) {
        Copy-Item $extSrc (Join-Path $stage "extension") -Recurse
    }
}

Copy-Item (Join-Path $root "README.md") $stage
$readme = @"
Session Restore $version

To install, run sr-setup.exe. It installs for the current user only and never
asks for administrator.

After it finishes, add the browser extension when the welcome window asks:
the unpacked build is in the extension folder next to this file, and the
welcome window will open each browser's extensions page for you.

To remove it, use Apps and Features, or run:
    sr-setup.exe --uninstall
Captured sessions are kept unless you add --purge.
"@
Set-Content -Path (Join-Path $stage "INSTALL.txt") -Value $readme -Encoding utf8

# ---------------------------------------------------------------- signing
if ($Sign) {
    $thumb = $env:SR_SIGN_THUMBPRINT
    if (-not $thumb) { throw "set SR_SIGN_THUMBPRINT to the signing certificate thumbprint" }

    $signtool = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if (-not $signtool) {
        $found = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin" -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
                 Where-Object { $_.FullName -match "x64" } | Select-Object -Last 1
        if (-not $found) { throw "signtool.exe not found. Install the Windows SDK." }
        $signtool = $found.FullName
    } else {
        $signtool = $signtool.Source
    }

    Step "Signing with $thumb"
    foreach ($f in $required) {
        Invoke-Native $signtool @(
            "sign", "/sha1", $thumb, "/fd", "SHA256",
            "/tr", "http://timestamp.digicert.com", "/td", "SHA256",
            (Join-Path $stage $f)
        )
    }
} else {
    Warn "Unsigned. SmartScreen will warn on other machines until these are signed."
    Warn "See docs/10-distribution.md."
}

# ---------------------------------------------------------------- zip
# The zip format cannot represent a timestamp before 1980, and the WebView2 loader
# that ships inside the webview2-com-sys crate carries a 1973 one. Normalising the
# staged copy is also the only way to get a reproducible archive.
Step "Normalising timestamps"
$stamp = Get-Date
Get-ChildItem $stage -Recurse -File | ForEach-Object {
    if ($_.LastWriteTime.Year -lt 1980) { $_.LastWriteTime = $stamp }
}

$zip = Join-Path $root "dist\SessionRestore-$version.zip"
Step "Writing $zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path "$stage\*" -DestinationPath $zip

# ---------------------------------------------------------------- store bundles
if (-not $SkipExtension) {
    Step "Writing store bundles"
    $chrome = Join-Path $root "dist\extension-chrome-$version.zip"
    $firefox = Join-Path $root "dist\extension-firefox-$version.zip"
    if (Test-Path $chrome) { Remove-Item $chrome -Force }
    if (Test-Path $firefox) { Remove-Item $firefox -Force }
    # Contents at the archive root, which is what both stores require.
    Compress-Archive -Path (Join-Path $root "extension\dist\chrome\*") -DestinationPath $chrome
    Compress-Archive -Path (Join-Path $root "extension\dist\firefox\*") -DestinationPath $firefox
    Write-Host "    $chrome  (Chrome Web Store and Edge Add-ons)"
    Write-Host "    $firefox (addons.mozilla.org)"
}

Step "Done"
Get-ChildItem (Join-Path $root "dist") | Select-Object Name, Length | Format-Table -AutoSize
