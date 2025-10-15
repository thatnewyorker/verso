Param(
  # Workspace root directory (path to the 'verso' directory). Defaults to the parent of this script's directory.
  [string]$WorkspaceRoot = "$(Split-Path -Parent $PSScriptRoot)"
)

# Windows MSI packaging template for Verso/Servo using WiX (candle.exe, light.exe).
# - Builds or reuses a staged Servo binary using `servo_prep` (metadata-only).
# - Locates the staged binary under third_party/servo-binaries.
# - Generates a minimal WiX .wxs and invokes candle.exe + light.exe to produce an MSI.
# - Optional signing (signtool) can be enabled via environment variables (see "Optional signing" section).
#
# Prerequisites:
# - Rust toolchain (cargo, rustc) in PATH
# - WiX Toolset in PATH: candle.exe and light.exe
# - A Servo checkout at ..\servo relative to the workspace (or set SERVO_SRC)
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File packaging\make_msi.ps1
#
# Output:
#   - MSI at: verso\dist\Verso.msi

$ErrorActionPreference = 'Stop'

function Log {
  param([string]$Message)
  Write-Host "==> $Message"
}
function Warn {
  param([string]$Message)
  Write-Warning $Message
}
function Die {
  param([string]$Message)
  Write-Error $Message
  exit 1
}

function Require-Command {
  param([string]$Name)
  if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
    Die "Required command not found in PATH: $Name"
  }
}

function Get-HostTriple {
  $out = & rustc -vV
  $m = ($out | Select-String -Pattern '^host:\s+(.+)$').Matches
  if ($m.Count -lt 1) {
    Die "Could not parse host triple from 'rustc -vV'"
  }
  return $m[0].Groups[1].Value.Trim()
}

function Stage-ServoBinary {
  param(
    [string]$WorkspaceRoot
  )
  $servoSrc = if ($env:SERVO_SRC) { $env:SERVO_SRC } else { Join-Path $WorkspaceRoot "..\servo" }

  if (-not (Test-Path $servoSrc)) {
    Warn "Servo source directory not found at: $servoSrc"
    Warn "Set SERVO_SRC to an absolute path, or adjust this script."
    Die "Servo sources are required to build or locate the binary."
  }

  Log "Staging Servo binary via servo_prep (metadata-only if already built)…"
  & cargo run -p servo_prep -- --servo-src "$servoSrc" --profile release --metadata-only | Out-Null
}

function Get-StagedDir {
  param(
    [string]$WorkspaceRoot,
    [string]$HostTriple
  )

  $stageBase = Join-Path $WorkspaceRoot "third_party\servo-binaries\local\$HostTriple\release"

  if (-not (Test-Path $stageBase)) {
    Die "Stage base not found: $stageBase. Did servo_prep run successfully?"
  }

  # Prefer 'current' pointer if present.
  $current = Join-Path $stageBase "current"
  if (Test-Path $current) {
    try {
      $resolved = (Resolve-Path $current -ErrorAction Stop).Path
      if (Test-Path $resolved) { return $resolved }
    } catch {
      # Fall through
    }
  }

  # Next, try latest.json
  $latestJson = Join-Path $stageBase "latest.json"
  if (Test-Path $latestJson) {
    try {
      $obj = Get-Content $latestJson | ConvertFrom-Json
      if ($obj.current) {
        $cand = Join-Path $stageBase $obj.current
        if (Test-Path $cand) { return $cand }
      }
    } catch {
      # Fall through
    }
  }

  # Fallback: newest directory by LastWriteTime
  $dir = Get-ChildItem -Directory $stageBase | Sort-Object LastWriteTime -Descending | Select-Object -First 1
  if ($null -ne $dir) {
    return $dir.FullName
  }

  Die "Could not locate staged directory under: $stageBase"
}

function New-WixTemplate {
  param(
    [string]$StagedExe,
    [string]$ProductName = "Verso",
    [string]$Manufacturer = "Example",
    [string]$Version = "1.0.0.0"
  )
@"
<?xml version='1.0' encoding='UTF-8'?>
<Wix xmlns='http://schemas.microsoft.com/wix/2006/wi'>
  <Product Id='*' Name='$ProductName' Language='1033' Version='$Version' Manufacturer='$Manufacturer' UpgradeCode='$(NewGuid)'>
    <Package InstallerVersion='500' Compressed='yes' InstallScope='perMachine' />
    <MediaTemplate />
    <MajorUpgrade DowngradeErrorMessage='A newer version of $ProductName is already installed.' />

    <Directory Id='TARGETDIR' Name='SourceDir'>
      <Directory Id='ProgramFilesFolder'>
        <Directory Id='INSTALLFOLDER' Name='$ProductName' />
      </Directory>
    </Directory>

    <DirectoryRef Id='INSTALLFOLDER'>
      <Component Id='MainExe' Guid='$(NewGuid)'>
        <File Id='MainExecutable' Source='$StagedExe' KeyPath='yes' Checksum='yes' />
      </Component>
    </DirectoryRef>

    <Feature Id='DefaultFeature' Level='1'>
      <ComponentRef Id='MainExe' />
    </Feature>
  </Product>
</Wix>
"@
}

# Optional signing function. To enable, set:
#   $env:SIGN = "1"
#   $env:SIGN_CERT_FILE = "C:\path\to\cert.pfx"
#   $env:SIGN_CERT_PASSWORD = "your_password"   # or use a secure secret store
#   $env:SIGN_TIMESTAMP_URL = "http://timestamp.digicert.com"  # defaults if not set
function Maybe-Sign {
  param(
    [string]$MsiPath
  )

  if ($env:SIGN -ne "1") {
    Log "Signing disabled (set $env:SIGN=1 to enable)."
    return
  }

  $signtool = Get-Command signtool -ErrorAction SilentlyContinue
  if (-not $signtool) {
    Warn "signtool not found in PATH; skipping signing."
    return
  }

  if (-not $env:SIGN_CERT_FILE) {
    Warn "SIGN_CERT_FILE not set; skipping signing."
    return
  }

  $tsUrl = if ($env:SIGN_TIMESTAMP_URL) { $env:SIGN_TIMESTAMP_URL } else { "http://timestamp.digicert.com" }

  Log "Signing MSI with signtool…"
  if ($env:SIGN_CERT_PASSWORD) {
    & $signtool sign /f "$($env:SIGN_CERT_FILE)" /p "$($env:SIGN_CERT_PASSWORD)" /tr $tsUrl /td sha256 /fd sha256 "$MsiPath"
  } else {
    & $signtool sign /f "$($env:SIGN_CERT_FILE)" /tr $tsUrl /td sha256 /fd sha256 "$MsiPath"
  }
  Log "Signed: $MsiPath"
}

# --- Main ---

Log "Workspace root: $WorkspaceRoot"

Require-Command cargo
Require-Command rustc
Require-Command candle.exe
Require-Command light.exe

# 1) Build or reuse staged Servo artifact
Stage-ServoBinary -WorkspaceRoot $WorkspaceRoot

# 2) Determine host triple and staged directory
$hostTriple = Get-HostTriple
$stagedDir  = Get-StagedDir -WorkspaceRoot $WorkspaceRoot -HostTriple $hostTriple

# 3) Validate staged executable
$stagedExe = Join-Path $stagedDir "servo.exe"
if (-not (Test-Path $stagedExe)) {
  Die "Staged binary not found: $stagedExe"
}
Log "Using staged binary: $stagedExe"

# 4) Create WiX .wxs under dist/
$dist = Join-Path $WorkspaceRoot "dist"
New-Item -ItemType Directory -Force -Path $dist | Out-Null

$wxsPath = Join-Path $dist "verso.wxs"
$wxs = New-WixTemplate -StagedExe $stagedExe -ProductName "Verso" -Manufacturer "Verso" -Version "1.0.0.0"
Set-Content -Path $wxsPath -Value $wxs -Encoding UTF8
Log "Generated WiX source: $wxsPath"

# 5) Compile and link with WiX
$wixobj = Join-Path $dist "verso.wixobj"
$msiPath = Join-Path $dist "Verso.msi"

& candle.exe $wxsPath -o $wixobj
& light.exe $wixobj -o $msiPath

Log "MSI created at: $msiPath"

# 6) Optional signing
Maybe-Sign -MsiPath $msiPath

Log "Done."
