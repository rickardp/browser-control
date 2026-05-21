#!/usr/bin/env pwsh
# PowerShell uninstaller for browser-control on Windows.
#
# Usage:
#   irm https://raw.githubusercontent.com/rickardp/browser-control/main/scripts/uninstall.ps1 | iex
#
# With options:
#   $script = irm https://raw.githubusercontent.com/rickardp/browser-control/main/scripts/uninstall.ps1
#   & ([scriptblock]::Create($script)) -Purge       # also delete user data (%APPDATA%\browser-control)
#   & ([scriptblock]::Create($script)) -InstallDir 'C:\tools\browser-control'
#
# Parameters:
#   -InstallDir  Override the install directory. Default:
#                $env:BROWSER_CONTROL_INSTALL or "$Home\.browser-control".
#   -Purge       Also delete the user data directory (%APPDATA%\browser-control),
#                which holds the registry DB, profiles, and config.toml.
#                Off by default — uninstall is non-destructive to user data.
#   -KeepPath    Do not touch the user PATH (default is to remove the bin dir).
#
# The script is idempotent: re-running it after a successful uninstall is a
# no-op and exits 0.

param(
  [String]$InstallDir = "",
  [Switch]$Purge = $false,
  [Switch]$KeepPath = $false
)

$ErrorActionPreference = "Stop"

if (-not $InstallDir) {
  $InstallDir = if ($env:BROWSER_CONTROL_INSTALL) { $env:BROWSER_CONTROL_INSTALL } else { "${Home}\.browser-control" }
}
$BinDir = Join-Path $InstallDir "bin"
$Dest = Join-Path $BinDir "browser-control.exe"

$DidSomething = $false

# --- Refuse if running ----------------------------------------------------

$running = Get-Process -Name browser-control -ErrorAction SilentlyContinue
if ($running) {
  $matching = $running | Where-Object { $_.Path -and ($_.Path -ieq $Dest) }
  if ($matching) {
    Write-Output "Uninstall failed: browser-control is currently running (PID $($matching.Id -join ', '))."
    Write-Output "Close it (or run: browser-control stop) and re-run the uninstaller."
    exit 1
  }
}

# --- Remove binary --------------------------------------------------------

if (Test-Path $Dest) {
  Remove-Item $Dest -Force
  Write-Output "Removed $Dest"
  $DidSomething = $true
} else {
  Write-Output "No binary at $Dest (already removed)."
}

# --- Remove bin dir if empty, then install dir if empty -------------------

foreach ($d in @($BinDir, $InstallDir)) {
  if ((Test-Path $d) -and -not (Get-ChildItem -Force -LiteralPath $d)) {
    Remove-Item $d -Force
    Write-Output "Removed empty directory $d"
    $DidSomething = $true
  }
}

# --- PATH cleanup (user-level, registry) ----------------------------------

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

  if (-not ("Win32.NativeMethods" -as [Type])) {
    Add-Type -Namespace Win32 -Name NativeMethods -MemberDefinition @"
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
"@
  }
  $result = [UIntPtr]::Zero
  [void][Win32.NativeMethods]::SendMessageTimeout([IntPtr]0xffff, 0x1a, [UIntPtr]::Zero, "Environment", 2, 5000, [ref]$result)
}

if (-not $KeepPath) {
  $current = Get-UserPath
  if ($current) {
    $binFull = try { [IO.Path]::GetFullPath($BinDir) } catch { $BinDir }
    $kept = @()
    $removed = $false
    foreach ($seg in ($current -split ';')) {
      if (-not $seg) { continue }
      $segFull = try { [IO.Path]::GetFullPath($seg) } catch { $seg }
      if ($segFull -ieq $binFull) { $removed = $true; continue }
      $kept += $seg
    }
    if ($removed) {
      Set-UserPath ($kept -join ';')
      Write-Output "Removed $BinDir from user PATH."
      $DidSomething = $true
    }
  }
}

# --- Optional: purge user data -------------------------------------------

if ($Purge) {
  $candidates = @()
  if ($env:BROWSER_CONTROL_CONFIG_DIR) { $candidates += $env:BROWSER_CONTROL_CONFIG_DIR }
  if ($env:BROWSER_CONTROL_DATA_DIR)   { $candidates += $env:BROWSER_CONTROL_DATA_DIR }
  if ($env:APPDATA) { $candidates += (Join-Path $env:APPDATA 'browser-control') }
  $candidates = $candidates | Where-Object { $_ } | Sort-Object -Unique

  foreach ($dir in $candidates) {
    if (Test-Path $dir) {
      Remove-Item -Recurse -Force $dir
      Write-Output "Purged user data at $dir"
      $DidSomething = $true
    }
  }
} else {
  $appdata = if ($env:APPDATA) { Join-Path $env:APPDATA 'browser-control' } else { $null }
  if ($appdata -and (Test-Path $appdata)) {
    Write-Output ""
    Write-Output "User data preserved at $appdata"
    Write-Output "Re-run with -Purge to delete it as well."
  }
}

if (-not $DidSomething) {
  Write-Output "Nothing to do — browser-control was not installed via this script."
}

Write-Output ""
Write-Output "Uninstall complete."
