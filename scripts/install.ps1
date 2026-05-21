#!/usr/bin/env pwsh
# PowerShell installer for browser-control on Windows.
#
# Usage (latest):
#   irm https://raw.githubusercontent.com/rickardp/browser-control/main/scripts/install.ps1 | iex
#
# Usage (pinned version, with parameters):
#   $script = irm https://raw.githubusercontent.com/rickardp/browser-control/main/scripts/install.ps1
#   & ([scriptblock]::Create($script)) -Version 0.3.5
#
# Parameters:
#   -Version       Pin a specific release (e.g. "0.3.5" or "v0.3.5"). Default: "latest".
#   -InstallDir    Override the install directory.
#                  Default: $env:BROWSER_CONTROL_INSTALL or "$Home\.browser-control".
#   -NoPathUpdate  Skip adding the bin directory to the user's PATH.
#   -Force         Reinstall even when the requested version is already installed.
#
# The script is idempotent: re-running it upgrades to the latest release if a
# newer one exists, and is a no-op (apart from a PATH sanity check) when the
# requested version is already in place.

param(
  [String]$Version = "latest",
  [String]$InstallDir = "",
  [Switch]$NoPathUpdate = $false,
  [Switch]$Force = $false
)

$ErrorActionPreference = "Stop"

$Repo = "rickardp/browser-control"

# --- Arch + OS check -------------------------------------------------------

$Arch = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment').PROCESSOR_ARCHITECTURE
if ($Arch -ne "AMD64") {
  Write-Output "Install failed: browser-control for Windows is currently only built for x86_64 (AMD64)."
  Write-Output "Detected architecture: $Arch"
  Write-Output "Track https://github.com/$Repo/issues for ARM64 support."
  exit 1
}

$Target = "x86_64-pc-windows-msvc"
$AssetName = "browser-control-$Target.zip"

# --- Resolve install dir ---------------------------------------------------

if (-not $InstallDir) {
  $InstallDir = if ($env:BROWSER_CONTROL_INSTALL) { $env:BROWSER_CONTROL_INSTALL } else { "${Home}\.browser-control" }
}
$BinDir = Join-Path $InstallDir "bin"
$null = New-Item -ItemType Directory -Force -Path $BinDir
$Dest = Join-Path $BinDir "browser-control.exe"

# --- Resolve target version (latest -> concrete tag) -----------------------

function Resolve-LatestTag {
  # Follow the redirect on /releases/latest without consuming the GitHub API
  # rate limit. The Location header is /releases/tag/vX.Y.Z.
  $url = "https://github.com/$Repo/releases/latest"
  try {
    $resp = Invoke-WebRequest -Uri $url -Method Head -MaximumRedirection 0 -UseBasicParsing -ErrorAction Stop
  } catch [System.Net.WebException] {
    $resp = $_.Exception.Response
  } catch {
    # PS 7+ raises a different exception type for non-success responses.
    $resp = $_.Exception.Response
  }
  $location = if ($resp -and $resp.Headers) {
    if ($resp.Headers -is [System.Collections.IDictionary]) { $resp.Headers['Location'] } else { $resp.Headers.Location }
  }
  if (-not $location) { throw "Could not resolve latest release tag from $url" }
  $loc = if ($location -is [array]) { $location[0] } else { $location }
  if ($loc -notmatch '/tag/(v[^/]+)$') { throw "Unexpected redirect target: $loc" }
  return $Matches[1]
}

if ($Version -eq "latest") {
  $Tag = Resolve-LatestTag
} else {
  $Tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
}
$TargetVersion = $Tag.TrimStart('v')
$DownloadUrl = "https://github.com/$Repo/releases/download/$Tag/$AssetName"

# --- Check existing install (idempotency) ---------------------------------

function Get-InstalledVersion {
  if (-not (Test-Path $Dest)) { return $null }
  try {
    $out = & $Dest --version 2>$null
    if ($LASTEXITCODE -ne 0 -or -not $out) { return $null }
    # clap default: "browser-control 0.3.5"
    if ($out -match '(\d+\.\d+\.\d+(?:[-+][^\s]+)?)') { return $Matches[1] }
    return $null
  } catch {
    return $null
  }
}

$Installed = Get-InstalledVersion

if ($Installed -eq $TargetVersion -and -not $Force) {
  Write-Output "browser-control $TargetVersion is already installed at $Dest"
  $SkipDownload = $true
} else {
  if ($Installed) {
    Write-Output "Updating browser-control: $Installed -> $TargetVersion"
  } else {
    Write-Output "Installing browser-control $TargetVersion to $BinDir"
  }
  $SkipDownload = $false
}

# --- Download (skipped when already at target version) --------------------

if (-not $SkipDownload) {
  $Tmp = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP "browser-control-install-$([guid]::NewGuid().ToString('N'))")
  $ZipPath = Join-Path $Tmp $AssetName

  try {
    # curl.exe ships with Windows 10 1803+ and is faster + more reliable than Invoke-WebRequest.
    $curl = Get-Command curl.exe -ErrorAction SilentlyContinue
    if ($curl) {
      & curl.exe "-#fSL" "-o" $ZipPath $DownloadUrl
      if ($LASTEXITCODE -ne 0) { throw "curl.exe exited with code $LASTEXITCODE" }
    } else {
      $prev = $global:ProgressPreference
      $global:ProgressPreference = 'SilentlyContinue'
      try {
        Invoke-WebRequest -Uri $DownloadUrl -OutFile $ZipPath -UseBasicParsing
      } finally {
        $global:ProgressPreference = $prev
      }
    }

    if (-not (Test-Path $ZipPath)) {
      throw "Download did not produce a file at $ZipPath. An antivirus may have removed it."
    }

    # --- Extract -----------------------------------------------------------

    $ExtractDir = Join-Path $Tmp "extract"
    $null = New-Item -ItemType Directory -Force -Path $ExtractDir
    $prev = $global:ProgressPreference
    $global:ProgressPreference = 'SilentlyContinue'
    try {
      Expand-Archive -Path $ZipPath -DestinationPath $ExtractDir -Force
    } finally {
      $global:ProgressPreference = $prev
    }

    # Release zips look like: browser-control-x86_64-pc-windows-msvc/browser-control.exe
    $Exe = Get-ChildItem -Path $ExtractDir -Filter "browser-control.exe" -Recurse | Select-Object -First 1
    if (-not $Exe) {
      throw "browser-control.exe not found inside $AssetName."
    }

    # --- Replace existing binary ------------------------------------------

    if (Test-Path $Dest) {
      try {
        Remove-Item $Dest -Force
      } catch [System.UnauthorizedAccessException] {
        $running = Get-Process -Name browser-control -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $Dest }
        if ($running) {
          Write-Output "Install failed: browser-control is currently running. Close it and re-run the installer."
          exit 1
        }
        throw
      }
    }
    Move-Item -Path $Exe.FullName -Destination $Dest -Force
  } finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $Tmp
  }

  # --- Verify --------------------------------------------------------------

  $Verified = Get-InstalledVersion
  if ($Verified -ne $TargetVersion) {
    Write-Warning "Installed binary reports version '$Verified' but expected '$TargetVersion'."
  } else {
    Write-Output "Installed browser-control $Verified"
  }
}

# --- PATH update (user-level, via registry to avoid %VAR% expansion) ------

function Get-UserPath {
  $key = (Get-Item -Path 'HKCU:').OpenSubKey('Environment')
  try { $key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) } finally { $key.Close() }
}

function Set-UserPath([string]$Value) {
  $key = (Get-Item -Path 'HKCU:').OpenSubKey('Environment', $true)
  try {
    $kind = if ($Value.Contains('%')) { [Microsoft.Win32.RegistryValueKind]::ExpandString } else { [Microsoft.Win32.RegistryValueKind]::String }
    $key.SetValue('Path', $Value, $kind)
  } finally { $key.Close() }

  # Broadcast WM_SETTINGCHANGE so new shells (and Explorer) pick up the change.
  if (-not ("Win32.NativeMethods" -as [Type])) {
    Add-Type -Namespace Win32 -Name NativeMethods -MemberDefinition @"
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
"@
  }
  $result = [UIntPtr]::Zero
  [void][Win32.NativeMethods]::SendMessageTimeout([IntPtr]0xffff, 0x1a, [UIntPtr]::Zero, "Environment", 2, 5000, [ref]$result)
}

if (-not $NoPathUpdate) {
  $current = Get-UserPath
  $segments = $current -split ';' | Where-Object { $_ -ne '' }
  $already = $segments | Where-Object { [IO.Path]::GetFullPath($_) -ieq [IO.Path]::GetFullPath($BinDir) }
  if ($already) {
    Write-Output "PATH already contains $BinDir"
  } else {
    $new = if ($current) { "$BinDir;$current" } else { $BinDir }
    Set-UserPath $new
    Write-Output "Added $BinDir to user PATH."
    Write-Output "Restart your shell (or run: `$env:Path = `"$BinDir;`$env:Path`") for it to take effect."
  }
}

Write-Output ""
Write-Output "Done. Run 'browser-control --help' to get started."
