<#
.SYNOPSIS
  Build simple-screensharing-client (and optionally the relay) on Windows.

.DESCRIPTION
  Prerequisites (see superdoc/features/windows.md):
    * Rust (rustup, stable-x86_64-pc-windows-msvc) and the MSVC C++ build tools
      (Visual Studio / Build Tools with "Desktop development with C++")
    * GStreamer 1.26+ MSVC x86_64 *runtime* and *devel* installers
      (https://gstreamer.freedesktop.org/download/), both with "Complete" /
      ADDLOCAL=ALL so every plugin set (bad: d3d11, mf; libav) is present

  Run from the repository root (or pass -Repo):
      powershell -ExecutionPolicy Bypass -File client\packaging\windows\build.ps1

.PARAMETER Repo       Repository root (default: two levels above this script's folder's parent).
.PARAMETER Gst        GStreamer root (default: $env:GSTREAMER_1_0_ROOT_MSVC_X86_64, else C:\gstreamer\1.0\msvc_x86_64).
.PARAMETER TargetDir  CARGO_TARGET_DIR. Keep it on a local disk, outside the source tree if you sync the tree.
.PARAMETER Debug      Build the dev profile instead of --release.
.PARAMETER Server     Also build the relay (simple-screensharing-server).
#>
param(
  [string]$Repo = (Resolve-Path (Join-Path $PSScriptRoot "..\..\..")).Path,
  [string]$Gst = $env:GSTREAMER_1_0_ROOT_MSVC_X86_64,
  [string]$TargetDir = "C:\sss-target",
  [switch]$Debug,
  [switch]$Server
)
$ErrorActionPreference = "Stop"

if (-not $Gst) { $Gst = "C:\gstreamer\1.0\msvc_x86_64" }
$Gst = $Gst.TrimEnd("\")
if (-not (Test-Path "$Gst\lib\pkgconfig\gstreamer-1.0.pc")) {
  throw "GStreamer devel files not found under '$Gst' (lib\pkgconfig\gstreamer-1.0.pc). Install the *devel* MSI too, or pass -Gst."
}

# gstreamer-rs's build scripts find GStreamer through pkg-config. The devel
# package ships its own pkg-config.exe; point at it so nothing else needs
# installing.
$env:PKG_CONFIG = "$Gst\bin\pkg-config.exe"
$env:PKG_CONFIG_PATH = "$Gst\lib\pkgconfig"
$env:PATH = "$Gst\bin;$env:PATH"
$env:CARGO_TARGET_DIR = $TargetDir

$profileArgs = if ($Debug) { @() } else { @("--release") }
$packages = @("-p", "simple-screensharing-client")
if ($Server) { $packages += @("-p", "simple-screensharing-server") }

Push-Location $Repo
try {
  & cargo build @profileArgs @packages
  if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
} finally { Pop-Location }

$dir = if ($Debug) { "debug" } else { "release" }
Write-Host "Built: $TargetDir\$dir\simple-screensharing-client.exe"
Write-Host "Run it with GStreamer on PATH:  `$env:PATH = '$Gst\bin;' + `$env:PATH"
