# Downloads the pinned shared LGPL FFmpeg build (nvenc, amf, libvpl) into vendor\ffmpeg.
# Called by "SuiCap Fetch FFmpeg.bat". Bump $Tag/$Asset together; the major version
# must match the ffmpeg-sys-next major in crates/suicap-core/Cargo.toml.
param([switch]$Force)
$ErrorActionPreference = 'Stop'

$Tag   = 'autobuild-2026-11-12-15-40'
$Asset = 'ffmpeg-n9.0.2-57-gd838f9f9ae-win64-lgpl-shared-9.0'
$Base  = "https://github.com/BtbN/FFmpeg-Builds/releases/download/$Tag"

$Root   = Split-Path -Parent $PSScriptRoot
$Vendor = Join-Path $Root 'vendor'
$Dest   = Join-Path $Vendor 'ffmpeg'
$Dl     = Join-Path $Vendor '_download'

if ((Test-Path (Join-Path $Dest 'include\libavcodec\avcodec.h')) -and -not $Force) {
    Write-Host "FFmpeg already present in vendor\ffmpeg (use -Force to refetch)."
    exit 0
}

New-Item -ItemType Directory -Force $Dl | Out-Null
$Zip  = Join-Path $Dl "$Asset.zip"
$Sums = Join-Path $Dl 'checksums.sha256'

Write-Host "Downloading $Asset.zip ..."
Invoke-WebRequest "$Base/$Asset.zip" -OutFile $Zip
Invoke-WebRequest "$Base/checksums.sha256" -OutFile $Sums

$expected = (Select-String -Path $Sums -Pattern ([regex]::Escape("$Asset.zip"))).Line.Split(' ')[0]
$stream   = [System.IO.File]::OpenRead($Zip)
try { $actual = -join ([System.Security.Cryptography.SHA256]::Create().ComputeHash($stream) | ForEach-Object { $_.ToString('x2') }) }
finally { $stream.Dispose() }
if (-not $expected -or $expected.ToLower() -ne $actual.ToLower()) {
    throw "SHA-256 mismatch for $Asset.zip (expected '$expected', got '$actual')."
}

$Unpack = Join-Path $Dl 'unpack'
if (Test-Path $Unpack) { Remove-Item -Recurse -Force $Unpack }
Expand-Archive $Zip -DestinationPath $Unpack
if (Test-Path $Dest) { Remove-Item -Recurse -Force $Dest }
Move-Item (Join-Path $Unpack $Asset) $Dest
Remove-Item -Recurse -Force $Dl

Write-Host "FFmpeg ready in vendor\ffmpeg ($Asset)."
