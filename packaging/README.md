# Verso Packaging Scripts (examples)

This directory contains example scripts you can adapt to package Verso/Servo artifacts on each platform. They are intentionally minimal and demonstrate how to:

- Stage a built binary (via `servo_prep`)
- Bundle resources
- Call platform tools to produce an installer/archive (AppImage/DMG/MSI)

For full background and guidance, see the repository root: `BUILD_PACKAGING.md`.

Important notes:
- These scripts are templates. Replace placeholders (like `<target>`, `<profile>`, `<commit>`) and identifiers (bundle IDs, product GUIDs) with values that make sense for your project.
- They do not perform signing. Add signing steps once you have credentials and a secure process in place.
- These scripts assume you run them from within the Verso workspace (the repo root or the `verso/` directory). Adjust relative paths if your layout is different.


## Directory structure

Recommended layout in your repo:

    verso/
      packaging/
        README.md                  # this file
        make_appimage.sh           # example: Linux AppImage
        make_dmg.sh                # example: macOS DMG
        make_msi.ps1               # example: Windows MSI
      static/                      # example app resources (optional)
      assets/
        icon.png                   # example app icon (optional)
      BUILD_PACKAGING.md           # detailed packaging guide
      tools/servo_prep/            # the packager/stager CLI
      third_party/servo-binaries/  # staged binaries (servo_prep output)
      dist/                        # packaged artifacts (output)


## Prerequisites

- Common:
  - Rust toolchain and Cargo
  - A Servo checkout to build via `servo_prep --servo-src <path>`
  - `git` in PATH

- Linux (AppImage):
  - `appimagetool` in PATH (only needed if you want to produce `.AppImage`)

- macOS (DMG):
  - `hdiutil` (available by default on macOS)

- Windows (MSI):
  - WiX Toolset in PATH (`candle.exe`, `light.exe`)


## Linux: AppImage template (make_appimage.sh)

A minimal script to stage an AppDir and call `appimagetool` if present. Save as `verso/packaging/make_appimage.sh` and make executable.

    #!/usr/bin/env bash
    set -euo pipefail

    # Resolve workspace root (this script is inside verso/packaging/)
    ROOT="$(cd "$(dirname "$0")"/.. && pwd)"
    DIST="${ROOT}/dist"
    APPDIR="${DIST}/Verso.AppDir"

    # 1) Build or reuse the Servo binary via servo_prep
    #    - Use --metadata-only to skip rebuilding when artifact is already available
    cargo run -p servo_prep -- \
      --servo-src "${ROOT}/../servo" \
      --profile release \
      --metadata-only

    # 2) Locate the staged binary (servo_prep standard layout)
    #    Replace placeholders with the actual folder names found under third_party/servo-binaries/
    TARGET_TRIPLE="$(rustc -vV | awk '/host:/ {print $2}')"
    STAGE_BASE="${ROOT}/third_party/servo-binaries/local/${TARGET_TRIPLE}/release"
    COMMIT="$(cat "${STAGE_BASE}"/*/metadata.toml | awk -F' = ' '/^servo_commit/ {gsub(/"/,"",$2); print $2; exit}')"
    STAGED_DIR="${STAGE_BASE}/${COMMIT}"
    STAGED_BIN="${STAGED_DIR}/servo"

    if [[ ! -f "${STAGED_BIN}" ]]; then
      echo "Staged binary not found: ${STAGED_BIN}"
      echo "Ensure servo_prep completed and paths are correct."
      exit 1
    fi

    # 3) Prepare AppDir layout
    rm -rf "${APPDIR}"
    mkdir -p "${APPDIR}/usr/bin" \
             "${APPDIR}/usr/share/applications" \
             "${APPDIR}/usr/share/icons/hicolor/256x256/apps"

    # Copy binary and resources
    cp "${STAGED_BIN}" "${APPDIR}/usr/bin/verso"
    chmod +x "${APPDIR}/usr/bin/verso"
    if [[ -d "${ROOT}/static" ]]; then
      cp -r "${ROOT}/static" "${APPDIR}/usr/bin/static"
    fi

    # Desktop file and icon (adjust fields)
    cat > "${APPDIR}/verso.desktop" <<EOF
    [Desktop Entry]
    Type=Application
    Name=Verso
    Exec=verso
    Icon=verso
    Categories=Utility;
    EOF

    if [[ -f "${ROOT}/assets/icon.png" ]]; then
      cp "${ROOT}/assets/icon.png" "${APPDIR}/usr/share/icons/hicolor/256x256/apps/verso.png"
    fi

    # 4) Produce AppImage (if appimagetool is available)
    mkdir -p "${DIST}"
    if command -v appimagetool >/dev/null 2>&1; then
      appimagetool "${APPDIR}" "${DIST}/Verso-${TARGET_TRIPLE}.AppImage"
      echo "AppImage created at: ${DIST}/Verso-${TARGET_TRIPLE}.AppImage"
    else
      echo "appimagetool not found in PATH. AppDir staged at: ${APPDIR}"
      echo "Install appimagetool or package AppDir manually."
    fi


## macOS: DMG template (make_dmg.sh)

A minimal script to stage a `.app` bundle and call `hdiutil` to create a DMG. Save as `verso/packaging/make_dmg.sh` and make executable. Run on macOS.

    #!/usr/bin/env bash
    set -euo pipefail

    ROOT="$(cd "$(dirname "$0")"/.. && pwd)"
    DIST="${ROOT}/dist"
    APP="${DIST}/Verso.app"

    cargo run -p servo_prep -- \
      --servo-src "${ROOT}/../servo" \
      --profile release \
      --metadata-only

    TARGET_TRIPLE="$(rustc -vV | awk '/host:/ {print $2}')"
    STAGE_BASE="${ROOT}/third_party/servo-binaries/local/${TARGET_TRIPLE}/release"
    COMMIT="$(cat "${STAGE_BASE}"/*/metadata.toml | awk -F' = ' '/^servo_commit/ {gsub(/"/,"",$2); print $2; exit}')"
    STAGED_DIR="${STAGE_BASE}/${COMMIT}"
    STAGED_BIN="${STAGED_DIR}/servo"

    if [[ ! -f "${STAGED_BIN}" ]]; then
      echo "Staged binary not found: ${STAGED_BIN}"
      exit 1
    fi

    rm -rf "${APP}"
    mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Resources"

    cp "${STAGED_BIN}" "${APP}/Contents/MacOS/verso"
    chmod +x "${APP}/Contents/MacOS/verso"
    if [[ -d "${ROOT}/static" ]]; then
      cp -r "${ROOT}/static" "${APP}/Contents/Resources/static"
    fi
    if [[ -f "${ROOT}/assets/icon.png" ]]; then
      cp "${ROOT}/assets/icon.png" "${APP}/Contents/Resources/icon.png"
    fi

    # Minimal Info.plist (adjust identifiers/strings)
    cat > "${APP}/Contents/Info.plist" <<EOF
    <?xml version="1.0" encoding="UTF-8"?>
    <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
    <plist version="1.0">
    <dict>
      <key>CFBundleName</key><string>Verso</string>
      <key>CFBundleIdentifier</key><string>org.example.verso</string>
      <key>CFBundleExecutable</key><string>verso</string>
      <key>CFBundlePackageType</key><string>APPL</string>
      <key>CFBundleVersion</key><string>1.0.0</string>
      <key>CFBundleShortVersionString</key><string>1.0.0</string>
    </dict>
    </plist>
    EOF

    # Create an unsigned DMG
    mkdir -p "${DIST}"
    DMG="${DIST}/Verso.dmg"
    hdiutil create -volname "Verso" -srcfolder "${APP}" -ov -format UDZO "${DMG}"
    echo "DMG created at: ${DMG}"

    # To sign later:
    # codesign --deep --force --sign "Developer ID Application: Your Name (TEAMID)" "${APP}"


## Windows: MSI template (make_msi.ps1)

A minimal PowerShell script to generate a WiX project and build an MSI with `candle.exe` and `light.exe`. Save as `verso/packaging/make_msi.ps1`. Run in a Developer PowerShell where WiX is available in PATH.

    Param(
      [string]$WorkspaceRoot = "$(Split-Path -Parent $PSScriptRoot)"
    )

    $ErrorActionPreference = "Stop"

    # 1) Build or reuse Servo binary (metadata-only reuses last build)
    & cargo run -p servo_prep -- --servo-src "$WorkspaceRoot\..\servo" --profile release --metadata-only

    # 2) Locate staged binary (adjust the path layout if needed)
    $rustcInfo = & rustc -vV
    $host = ($rustcInfo | Select-String -Pattern '^host:\s+(.+)$').Matches.Groups[1].Value
    $stageBase = Join-Path $WorkspaceRoot "third_party\servo-binaries\local\$host\release"

    $commit = ""
    Get-ChildItem -Directory $stageBase | ForEach-Object {
      $meta = Join-Path $_.FullName "metadata.toml"
      if (Test-Path $meta) {
        $line = Get-Content $meta | Where-Object { $_ -match '^servo_commit\s*=\s*".*"$' } | Select-Object -First 1
        if ($line) {
          $commit = ($line -split '=')[1].Trim().Trim('"')
        }
      }
    }
    if (-not $commit) { throw "Could not determine commit from metadata.toml under $stageBase" }

    $stagedDir = Join-Path $stageBase $commit
    $stagedExe = Join-Path $stagedDir "servo.exe"
    if (-not (Test-Path $stagedExe)) { throw "Staged binary not found: $stagedExe" }

    # 3) Generate a minimal .wxs
    $dist = Join-Path $WorkspaceRoot "dist"
    New-Item -ItemType Directory -Force -Path $dist | Out-Null
    $wxsPath = Join-Path $dist "verso.wxs"

    $productGuid = [Guid]::NewGuid().ToString().ToUpper()
    $componentGuid = [Guid]::NewGuid().ToString().ToUpper()
    $upgradeGuid = [Guid]::NewGuid().ToString().ToUpper()

    $wxs = @"
    <?xml version='1.0' encoding='UTF-8'?>
    <Wix xmlns='http://schemas.microsoft.com/wix/2006/wi'>
      <Product Id='*' Name='Verso' Language='1033' Version='1.0.0.0' Manufacturer='Example' UpgradeCode='$upgradeGuid'>
        <Package InstallerVersion='500' Compressed='yes' InstallScope='perMachine' />
        <MediaTemplate />
        <Directory Id='TARGETDIR' Name='SourceDir'>
          <Directory Id='ProgramFilesFolder'>
            <Directory Id='INSTALLFOLDER' Name='Verso' />
          </Directory>
        </Directory>
        <DirectoryRef Id='INSTALLFOLDER'>
          <Component Id='MainExe' Guid='$componentGuid'>
            <File Id='VersoExe' Source='$stagedExe' KeyPath='yes' Checksum='yes' />
          </Component>
        </DirectoryRef>
        <Feature Id='DefaultFeature' Level='1'>
          <ComponentRef Id='MainExe' />
        </Feature>
      </Product>
    </Wix>
    "@

    Set-Content -Path $wxsPath -Value $wxs -Encoding UTF8

    # 4) Compile and link with WiX
    $wixobj = Join-Path $dist "verso.wixobj"
    $msiPath = Join-Path $dist "Verso.msi"

    & candle.exe $wxsPath -o $wixobj
    & light.exe $wixobj -o $msiPath

    Write-Host "MSI created at: $msiPath"

    # To sign later:
    # & signtool sign /f <cert.pfx> /p <pwd> /tr http://timestamp.server /td sha256 /fd sha256 $msiPath


## Usage: quick steps

1) Ensure your Servo checkout is available (e.g., `../servo` relative to the Verso workspace).
2) Run the platform script:
   - Linux: `bash packaging/make_appimage.sh`
   - macOS: `bash packaging/make_dmg.sh`
   - Windows: `powershell -ExecutionPolicy Bypass -File packaging/make_msi.ps1`
3) Artifacts will be produced under `dist/`.

If the platform tool is missing (e.g., `appimagetool`, `candle.exe`), the scripts will either fail with a clear message or leave a staged directory for manual packaging.


## Tips and troubleshooting

- Paths: The scripts use a staged layout from `servo_prep`. If your layout differs, update the path discovery steps accordingly.
- Permissions: On Unix, ensure the packaged binary is executable (chmod +x).
- Icons and metadata: Replace bundle identifiers (macOS), icons, and strings with your own values.
- Antivirus/SmartScreen: On Windows, unsigned binaries/MSIs may trigger warnings. Sign your artifacts for distribution.
- CI: Integrate these scripts or port their steps into your CI workflow. See `BUILD_PACKAGING.md` for a matrix job example.


## Attribution

These templates are inspired by common patterns used in Rust desktop packaging and by the modular approach in Tauri’s packaging utilities. If you adapt code or structure directly from external sources, keep license headers and attribution as appropriate (MIT/Apache-2.0).