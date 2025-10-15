# BUILD_PACKAGING.md

Verso packaging guide
Create reproducible, cross-platform distributables for Servo/versoview using the `servo_prep` CLI.

This guide covers:
- Packaging prerequisites (per-OS)
- CLI usage and examples (zip now; AppImage/DMG/MSI planned)
- Reproducible metadata embedded in artifacts
- Suggested directory layout for bundled resources
- CI examples for packaging on Linux/macOS/Windows
- (Optional) example script templates under `verso/packaging/`


## 1) Overview

`servo_prep` builds a Servo binary from a local checkout and stages it under:
- `third_party/servo-binaries/<target>/<profile>/<commit>/servo[.exe]`
- `third_party/servo-binaries/<target>/<profile>/<commit>/metadata.toml`

With packaging flags, it also:
- Stages the built binary and your resources into a temporary layout
- Writes a reproducible `metadata.json` (checksums included)
- Produces a distributable artifact (zip today; AppImage/DMG/MSI in future steps)

Default builds remain unchanged unless you pass `--package`.


## 2) Prerequisites

Common:
- Rust toolchain with Cargo installed
- Git in PATH (to read the Servo commit)
- A Servo checkout to build (`--servo-src` or `SERVO_SRC`)

Per-OS external packaging tools (only needed for the corresponding format):
- Linux (AppImage) — appimagetool in PATH (planned)
- macOS (DMG) — `hdiutil` (built-in) (planned)
- Windows (MSI) — WiX Toolset: `candle.exe` and `light.exe` in PATH (planned)

Signing (optional; future CLI integration):
- macOS: `codesign` and certificates/keys configured locally
- Windows: `signtool` and a code-signing certificate


## 3) CLI usage

Run `servo_prep --help` for the full set of options. Key flags for packaging:

- Build and staging
  - `--servo-src <PATH>`: path to Servo checkout
  - `--profile <debug|release>`: build profile (default: release)
  - `--target <TRIPLE>`: cross-compile target triple
  - `--features <a,b,c>`: comma-separated Cargo features for Servo build
  - `--binary-name <NAME>`: Servo binary target (default: servo)
  - `--metadata-only`: skip building, just stage existing artifact + metadata
  - `--output-dir <PATH>`: base path under repo for staged binaries (default: third_party/servo-binaries/local)

- Packaging
  - `--package <zip|appimage|dmg|msi>`: choose packaging format (zip implemented; others planned)
  - `--bundle-resources <PATH...>`: add files/directories into the package root
  - `--out-dir <PATH>`: destination for packaged artifacts (default: `<workspace>/dist`)

- Misc
  - `--config <PATH>`: optional TOML config (`servo-build-config.toml`) for defaults
  - `--toolchain <STRING>`: rustup toolchain spec for Servo build (e.g., `stable`, `nightly`, `1.77.0`)
  - `--no-current-pointer`: suppress `current` symlink / latest.json pointer
  - `--copy-to <PATH>`: copy staged binary to an additional project path
  - `--verbose`: extra logs


## 4) Quick start: produce a zip on any OS

Example: build and package a zip with extra resources:
```
cargo run -p servo_prep -- \
  --servo-src /absolute/path/to/servo \
  --profile release \
  --package zip \
  --bundle-resources ./static ./assets/icon.png \
  --out-dir ./dist
```

Outputs:
- `dist/servo-<target>-<profile>-<commit>.zip` (e.g., `servo-x86_64-unknown-linux-gnu-release-3ab42c1a2fcd.zip`)
- The zip contains:
  - `servo` or `servo.exe`
  - `metadata.json` (see section 6)
  - Any bundled resources (files & directories copied verbatim into the archive root)


## 5) Bundling resources

Pass any number of paths with `--bundle-resources`:
- Files are copied into the package root with their file name.
- Directories are copied into a top-level directory of the same name, recursively.

Recommended structure in your repo:
```
static/              # static assets to ship
assets/icon.png      # app icon (if any)
```

On the CLI:
```
--bundle-resources ./static ./assets/icon.png
```

Resolution:
- Relative paths are resolved against the workspace root.
- Absolute paths are copied as-is.


## 6) Reproducible metadata (metadata.json)

Every packaged artifact includes `metadata.json` with deterministic fields and file checksums.

Fields:
- `servo_commit`: short SHA of the Servo commit used to build
- `build_profile`: `"debug"` or `"release"`
- `enabled_features`: array of enabled Servo features (if any)
- `timestamp`: RFC3339 UTC timestamp when packaging was created
- `target_triple`: the target triple used for the build
- `rust_toolchain`: the rustc version used (or `--toolchain` if set)
- `binary_name`: the Cargo binary target that was built
- `checksums`: map of relative file paths inside the package to SHA-256 hex digests

Example:
```
{
  "servo_commit": "3ab42c1a2fcd",
  "build_profile": "release",
  "enabled_features": ["webrender", "webgpu"],
  "timestamp": "2025-10-15T10:20:30Z",
  "target_triple": "x86_64-unknown-linux-gnu",
  "rust_toolchain": "rustc 1.77.0 (xxxxxxxxx yyyy-mm-dd)",
  "binary_name": "servo",
  "checksums": {
    "servo": "c6e4d2f0... (sha256)",
    "metadata.json": "2a4c... (sha256)",
    "static/index.html": "86dd... (sha256)",
    "assets/icon.png": "f0ab... (sha256)"
  }
}
```

Use these fields to validate artifacts and trace builds in CI.


## 7) AppImage (Linux), DMG (macOS), MSI (Windows)

Planned implementations in `servo_prep`:
- `--package appimage`: Builds an AppDir and calls `appimagetool` if available
- `--package dmg`: Stages an unsigned `.app` and calls `hdiutil` to produce a DMG
- `--package msi`: Generates WiX sources and calls `candle`/`light` to create an MSI

Current status:
- Zip packaging is implemented today.
- AppImage/DMG/MSI tasks are planned; the CLI will fail gracefully with an actionable message until implemented.

Manual workarounds (for early adopters):
- Linux: Use `appimagetool` with a staged AppDir from your own script (see templates below)
- macOS: Use `hdiutil` to create DMG from a staged `.app`
- Windows: Use WiX Toolset (candle/light) to produce MSI from a simple .wxs template

Signing (manual steps):
- macOS: `codesign --deep --force --sign "<Identity>" <path to .app>`
- Windows: `signtool sign /f <cert.pfx> /p <pwd> /tr http://timestamp.server /td sha256 /fd sha256 <path to .msi or .exe>`


## 8) Example script templates

Consider placing helper scripts in `verso/packaging/` for local DX and CI steps. Below are minimal templates you can adapt.

Linux — AppImage (template):
```
#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")"/.. && pwd)"
DIST="${ROOT}/dist"
APPDIR="${DIST}/Verso.AppDir"

rm -rf "${APPDIR}"
mkdir -p "${APPDIR}/usr/bin" "${APPDIR}/usr/share/applications" "${APPDIR}/usr/share/icons/hicolor/256x256/apps"

# Build binary and copy resources (adjust paths as needed)
cargo run -p servo_prep -- \
  --servo-src "${ROOT}/../servo" \
  --profile release \
  --metadata-only # or actual build flags

# Copy your built/staged binary and resources into ${APPDIR}/usr/bin
cp "${ROOT}/third_party/servo-binaries/<target>/<profile>/<commit>/servo" "${APPDIR}/usr/bin/verso"
cp -r "${ROOT}/static" "${APPDIR}/usr/bin/static"

# Desktop file and icon (replace values)
cat > "${APPDIR}/verso.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Verso
Exec=verso
Icon=verso
Categories=Utility;
EOF

cp "${ROOT}/assets/icon.png" "${APPDIR}/usr/share/icons/hicolor/256x256/apps/verso.png"

# Create AppImage (requires appimagetool)
appimagetool "${APPDIR}" "${DIST}/Verso-x86_64.AppImage"
```

macOS — DMG (template):
```
#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")"/.. && pwd)"
DIST="${ROOT}/dist"
APP="${DIST}/Verso.app"

rm -rf "${APP}"
mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Resources"

# Build binary and copy
cargo run -p servo_prep -- --servo-src "${ROOT}/../servo" --profile release --metadata-only
cp "${ROOT}/third_party/servo-binaries/<target>/<profile>/<commit>/servo" "${APP}/Contents/MacOS/verso"

# Info.plist (replace identifiers/strings)
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

# Create DMG (unsigned)
hdiutil create -volname "Verso" -srcfolder "${APP}" -ov -format UDZO "${DIST}/Verso.dmg"
```

Windows — WiX MSI (template; PowerShell):
```
Param(
  [string]$WorkspaceRoot = "$(Split-Path -Parent $PSScriptRoot)"
)

$dist = Join-Path $WorkspaceRoot "dist"
$staged = Join-Path $WorkspaceRoot "third_party/servo-binaries\<target>\<profile>\<commit>\servo.exe"

# Build binary and stage (metadata-only or full build)
cargo run -p servo_prep -- --servo-src "$WorkspaceRoot\..\servo" --profile release --metadata-only

# Minimal .wxs template
$wxs = @"
<?xml version='1.0' encoding='UTF-8'?>
<Wix xmlns='http://schemas.microsoft.com/wix/2006/wi'>
  <Product Id='*' Name='Verso' Language='1033' Version='1.0.0.0' Manufacturer='Example' UpgradeCode='PUT-GUID-HERE'>
    <Package InstallerVersion='500' Compressed='yes' InstallScope='perMachine' />
    <MediaTemplate />
    <Directory Id='TARGETDIR' Name='SourceDir'>
      <Directory Id='ProgramFilesFolder'>
        <Directory Id='INSTALLFOLDER' Name='Verso' />
      </Directory>
    </Directory>
    <DirectoryRef Id='INSTALLFOLDER'>
      <Component Id='MainExe' Guid='PUT-GUID-HERE'>
        <File Id='VersoExe' Source='$staged' KeyPath='yes' Checksum='yes' />
      </Component>
    </DirectoryRef>
    <Feature Id='DefaultFeature' Level='1'>
      <ComponentRef Id='MainExe' />
    </Feature>
  </Product>
</Wix>
"@

$wxsPath = Join-Path $dist "verso.wxs"
New-Item -ItemType Directory -Force -Path $dist | Out-Null
Set-Content -Path $wxsPath -Value $wxs -Encoding UTF8

# WiX build (requires candle.exe and light.exe in PATH)
candle.exe $wxsPath -o (Join-Path $dist "verso.wixobj")
light.exe (Join-Path $dist "verso.wixobj") -o (Join-Path $dist "Verso.msi")
```


## 9) CI: packaging matrix (zip on all OS)

Example GitHub Actions job fragment (add to your workflow):

```
jobs:
  packaging:
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4

      - uses: Swatinem/rust-cache@v2

      - name: Build & package (zip)
        run: |
          cargo run -p servo_prep -- --servo-src ${{ github.workspace }}/servo --profile release --package zip --bundle-resources ./static --out-dir ./dist

      - name: Upload artifacts
        uses: actions/upload-artifact@v4
        with:
          name: verso-${{ matrix.os }}
          path: dist/*.zip

      # Optional platform packagers (planned; keep as conditional steps once implemented)
      # - name: AppImage (Linux only)
      #   if: matrix.os == 'ubuntu-latest'
      #   run: |
      #     which appimagetool && cargo run -p servo_prep -- --package appimage --bundle-resources ./static --out-dir ./dist || echo "appimagetool not installed; skipping"
      #
      # - name: DMG (macOS only)
      #   if: matrix.os == 'macos-latest'
      #   run: |
      #     cargo run -p servo_prep -- --package dmg --bundle-resources ./static --out-dir ./dist
      #
      # - name: MSI (Windows only)
      #   if: matrix.os == 'windows-latest'
      #   run: |
      #     where candle.exe && where light.exe && cargo run -p servo_prep -- --package msi --bundle-resources .\static --out-dir .\dist || echo "WiX not installed; skipping"
```


## 10) Troubleshooting

- “Could not find built binary”: Ensure `--servo-src` is correct and the targeted binary exists at `servo/target/<triple>/<profile>/<bin>`.
- “zip created, but missing files”: Check your `--bundle-resources` paths; relative paths are resolved from the workspace root, not the working directory.
- Permissions on Unix: The packaged binary is set executable (`0755`). If you unpack and it’s not executable, verify the unzip tool didn’t strip permissions.
- AppImage/DMG/MSI not produced: These formats are planned in `servo_prep`. Use the templates above or ensure external tools are installed once support lands.


## 11) Roadmap and notes

- Zip packaging is implemented now and tested in CI.
- AppImage (Linux), DMG (macOS), and MSI (Windows) packaging will land behind the same `--package` flag with external tool detection and clear error messages.
- Signing is intentionally out-of-scope for now to avoid secret handling in the CLI; use platform tools manually or add a thin wrapper later.


## 12) Attribution

This packaging approach draws inspiration from Tauri’s utils and packaging patterns. If code or text is later adapted directly, preserve license headers and attribution (MIT/Apache-2.0).


---
Last updated: see repo history for changes to `servo_prep` and this guide.