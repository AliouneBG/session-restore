<#
.SYNOPSIS
  Gets the Firefox add-on signed by Mozilla, without publishing it anywhere.

.DESCRIPTION
  Release Firefox enforces extension signatures and does not let you turn that off,
  so a temporary add-on loaded through about:debugging is the only unsigned option and
  it is discarded every time Firefox restarts. Signing is what makes it permanent.

  This uses AMO's *unlisted* channel, which means Mozilla signs the file and hands it
  back, and it never appears in the public add-on directory. Nobody but you sees it.
  It is free.

  You need API credentials once, from:
      https://addons.mozilla.org/en-US/developers/addon/api/key/

  Sign in with a Firefox account, press Generate new credentials, and copy both values.
  The secret is shown once.

.PARAMETER ApiKey
  The JWT issuer, which looks like user:12345678:123.

.PARAMETER ApiSecret
  The JWT secret, a long hex string.

.EXAMPLE
  .\scripts\sign-firefox.ps1 -ApiKey "user:12345678:123" -ApiSecret "abc123..."

.EXAMPLE
  $env:AMO_JWT_ISSUER = "user:12345678:123"
  $env:AMO_JWT_SECRET = "abc123..."
  .\scripts\sign-firefox.ps1
#>
[CmdletBinding()]
param(
    [string]$ApiKey = $env:AMO_JWT_ISSUER,
    [string]$ApiSecret = $env:AMO_JWT_SECRET
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$ext = Join-Path $root "extension"

function Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Warn($msg) { Write-Host "    $msg" -ForegroundColor Yellow }

# Native tools report failure through their exit code. Windows PowerShell turns their
# stderr into ErrorRecords, which under ErrorActionPreference = Stop would make ordinary
# progress output fatal.
function Invoke-Native {
    param([Parameter(Mandatory)][string]$Exe, [string[]]$Arguments = @())
    $prev = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        & $Exe @Arguments
        if ($LASTEXITCODE -ne 0) {
            throw "$Exe failed with exit code $LASTEXITCODE"
        }
    } finally {
        $ErrorActionPreference = $prev
    }
}

if (-not $ApiKey -or -not $ApiSecret) {
    Write-Host @"

Missing AMO credentials.

Get them once, free, at:
    https://addons.mozilla.org/en-US/developers/addon/api/key/

Sign in with a Firefox account, press Generate new credentials, then either pass them:

    .\scripts\sign-firefox.ps1 -ApiKey "user:..." -ApiSecret "..."

or set them in the environment:

    `$env:AMO_JWT_ISSUER = "user:..."
    `$env:AMO_JWT_SECRET = "..."

The secret is only shown once, so copy it before leaving the page.

"@ -ForegroundColor Yellow
    exit 1
}

Push-Location $ext
try {
    Step "Building the extension"
    Invoke-Native npm @("run", "build")

    # Catch anything the reviewer would catch, before uploading it.
    Step "Linting"
    Invoke-Native npx @("web-ext", "lint", "--source-dir=dist/firefox", "--output=text")

    $out = Join-Path $root "dist\firefox-signed"
    New-Item -ItemType Directory -Path $out -Force | Out-Null

    # --channel=unlisted is the whole point: signed for self-distribution, never listed
    # in the public directory. Listed submissions go to human review and take days.
    Step "Submitting to Mozilla for signing (unlisted)"
    Invoke-Native npx @(
        "web-ext", "sign",
        "--source-dir=dist/firefox",
        "--artifacts-dir=$out",
        "--channel=unlisted",
        "--api-key=$ApiKey",
        "--api-secret=$ApiSecret"
    )
} finally {
    Pop-Location
}

$xpi = Get-ChildItem (Join-Path $root "dist\firefox-signed") -Filter "*.xpi" -ErrorAction SilentlyContinue |
       Sort-Object LastWriteTime | Select-Object -Last 1

if (-not $xpi) {
    throw "signing reported success but produced no .xpi"
}

Step "Signed"
Write-Host ""
Write-Host "  $($xpi.FullName)" -ForegroundColor Green
Write-Host ""
Write-Host "To install it permanently:" -ForegroundColor Cyan
Write-Host "  1. Open about:addons"
Write-Host "  2. Gear icon, then Install Add-on From File"
Write-Host "  3. Choose the .xpi above"
Write-Host "  4. Still in about:addons, open Session Restore and set"
Write-Host "     Run in Private Windows to Allow"
Write-Host ""
Write-Host "It survives restarts from then on, permission included." -ForegroundColor Cyan
