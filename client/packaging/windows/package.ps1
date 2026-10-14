<#
.SYNOPSIS
  Assemble a self-contained folder (and zip) for the Windows client: the exe, the
  GStreamer DLLs it needs, and a curated set of GStreamer plugins.

.DESCRIPTION
  Needs the same prerequisites as build.ps1 plus `dumpbin` (ships with the MSVC build
  tools; found through vswhere). Run it after build.ps1 has produced the release exe:

      powershell -ExecutionPolicy Bypass -File client\packaging\windows\package.ps1

  Layout of the output folder (dist\simple-screensharing-windows by default):

      simple-screensharing-client.exe
      *.dll                      GStreamer / GLib / codec DLLs (dependency closure of the plugins)
      gst-plugins\gst*.dll       the plugins in windows-plugins.txt
      gst-plugin-scanner.exe

  The exe finds `gst-plugins\` itself (main.rs `use_bundled_gstreamer`), so no launcher
  script and no GStreamer install on the target machine are needed.

  Why the DLL closure is computed instead of copying GStreamer's bin\ folder: bin\ is
  620 MB (it serves every plugin GStreamer has); what the ~20 curated plugins and the exe
  actually import is a fraction of that.
#>
param(
  [string]$Repo = (Resolve-Path (Join-Path $PSScriptRoot "..\..\..")).Path,
  [string]$Gst = $env:GSTREAMER_1_0_ROOT_MSVC_X86_64,
  [string]$TargetDir = "C:\sss-target",
  [string]$Out = (Join-Path (Resolve-Path (Join-Path $PSScriptRoot "..\..\..")).Path "dist\simple-screensharing-windows"),
  [switch]$NoZip
)
$ErrorActionPreference = "Stop"
if (-not $Gst) { $Gst = "C:\Program Files\gstreamer\1.0\msvc_x86_64" }
$Gst = $Gst.TrimEnd("\")
$exe = Join-Path $TargetDir "release\simple-screensharing-client.exe"
if (-not (Test-Path $exe)) { throw "$exe not found; run build.ps1 first" }

# dumpbin, for the import tables.
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$dumpbin = Get-ChildItem "$vs\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe" | Sort-Object FullName | Select-Object -Last 1
if (-not $dumpbin) { throw "dumpbin.exe not found (install the MSVC C++ build tools)" }

$plugins = Get-Content (Join-Path $PSScriptRoot "windows-plugins.txt") |
  ForEach-Object { ($_ -replace "#.*", "").Trim() } | Where-Object { $_ }

if (Test-Path $Out) { Remove-Item -Recurse -Force $Out }
New-Item -ItemType Directory -Force "$Out\gst-plugins" | Out-Null
Copy-Item $exe $Out

$pluginFiles = foreach ($p in $plugins) {
  $f = "$Gst\lib\gstreamer-1.0\gst$p.dll"
  if (-not (Test-Path $f)) { Write-Warning "plugin gst$p.dll is not in the runtime install; skipped"; continue }
  Copy-Item $f "$Out\gst-plugins"
  $f
}
Copy-Item "$Gst\libexec\gstreamer-1.0\gst-plugin-scanner.exe" $Out

# Dependency closure: every DLL that the exe, the plugins and the scanner import and that
# lives in $Gst\bin (Windows system DLLs are not copied).
$have = @{}
$queue = New-Object System.Collections.Queue
foreach ($f in @($exe) + $pluginFiles + @("$Gst\libexec\gstreamer-1.0\gst-plugin-scanner.exe")) { $queue.Enqueue($f) }
while ($queue.Count -gt 0) {
  $f = $queue.Dequeue()
  $imports = & $dumpbin.FullName /nologo /dependents $f | ForEach-Object { if ($_ -match "^\s+(\S+\.dll)\s*$") { $Matches[1] } }
  foreach ($d in $imports) {
    $key = $d.ToLower()
    if ($have.ContainsKey($key)) { continue }
    $src = Join-Path "$Gst\bin" $d
    if (Test-Path $src) { $have[$key] = $src; $queue.Enqueue($src) } else { $have[$key] = $null }
  }
}
$copied = 0
foreach ($kv in $have.GetEnumerator()) {
  if ($kv.Value) { Copy-Item $kv.Value $Out; $copied++ }
}
"{0} plugins, {1} DLLs copied" -f @($pluginFiles).Count, $copied

# Prove the packaged set is complete: the listed elements must resolve with the
# machine's GStreamer disabled.
$inspect = "$Gst\bin\gst-inspect-1.0.exe"
Copy-Item $inspect $Out
$env:GST_PLUGIN_SYSTEM_PATH_1_0 = "$Out\gst-plugins"
$env:GST_PLUGIN_PATH_1_0 = "$Out\gst-plugins"
$env:GST_PLUGIN_SCANNER_1_0 = "$Out\gst-plugin-scanner.exe"
$env:GST_REGISTRY_1_0 = Join-Path $env:TEMP "sss-package-check-registry.bin"
Remove-Item $env:GST_REGISTRY_1_0 -ErrorAction SilentlyContinue
$savedPath = $env:PATH
$env:PATH = "$Out;$env:SystemRoot\System32;$env:SystemRoot"
$missing = @()
foreach ($e in (Get-Content (Join-Path $PSScriptRoot "windows-elements.txt") | ForEach-Object { ($_ -replace "#.*", "").Trim() } | Where-Object { $_ })) {
  & "$Out\gst-inspect-1.0.exe" $e *> $null
  if ($LASTEXITCODE -ne 0) { $missing += $e }
}
$env:PATH = $savedPath
Remove-Item "$Out\gst-inspect-1.0.exe"
if ($missing) { throw "elements missing from the packaged set: $($missing -join ', ')" }
Write-Host "all elements in windows-elements.txt resolve from the packaged set"

$mb = (Get-ChildItem $Out -Recurse -File | Measure-Object Length -Sum).Sum / 1MB
"{0:N0} MB in {1}" -f $mb, $Out
if (-not $NoZip) {
  $zip = "$Out.zip"
  if (Test-Path $zip) { Remove-Item $zip }
  Compress-Archive -Path "$Out\*" -DestinationPath $zip
  "zip: {0} ({1:N0} MB)" -f $zip, ((Get-Item $zip).Length / 1MB)
}
