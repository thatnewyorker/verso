#!/usr/bin/env bash
# Example script: package Verso/Servo as a macOS .app and DMG.
# This is a template intended for local use and CI. Adjust paths, names, and identifiers as needed.

set -euo pipefail
IFS=$'\n\t'
umask 022

# Resolve workspace root (this script lives under verso/packaging/)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
DIST="${ROOT}/dist"
APP="${DIST}/Verso.app"

# Staged servo binary (created by servo_prep)
PROFILE="release"
TARGET_TRIPLE="$(rustc -vV | awk '/^host:/{print $2}')"
STAGE_BASE="${ROOT}/third_party/servo-binaries/local/${TARGET_TRIPLE}/${PROFILE}"

# Optional resources
RES_DIR="${ROOT}/static"
ICON_SRC="${ROOT}/assets/icon.png"

# Optional signing (set CODESIGN_ID to sign the .app)
: "${CODESIGN_ID:=}"   # e.g., "Developer ID Application: Example, Inc. (TEAMID)"
: "${CODESIGN_OPTS:=--deep --force}"  # additional codesign options

log()  { printf '%s\n' "==> $*"; }
warn() { printf '%s\n' "WARN: $*" >&2; }
die()  { printf '%s\n' "ERROR: $*" >&2; exit 1; }

command_exists() { command -v "$1" >/dev/null 2>&1; }

stage_servo() {
  local servo_src="${SERVO_SRC:-${ROOT}/../servo}"
  if [[ ! -d "${servo_src}" ]]; then
    warn "Servo source directory not found at: ${servo_src}"
    warn "Set SERVO_SRC=/absolute/path/to/servo or adjust this script."
    die "Servo sources are required to build or locate the binary."
  fi

  if ! command_exists cargo; then
    die "cargo is not in PATH. Install Rust toolchain (https://rustup.rs/) and retry."
  fi

  log "Staging Servo binary via servo_prep (metadata-only if already built)…"
  ( set -x
    cargo run -p servo_prep -- \
      --servo-src "${servo_src}" \
      --profile "${PROFILE}" \
      --metadata-only
  )
}

find_newest_dir() {
  local base="$1"
  local newest
  newest="$(find "${base}" -mindepth 1 -maxdepth 1 -type d -print0 2>/dev/null \
    | xargs -0 stat -f '%m %N' \
    | sort -nr \
    | head -n1 \
    | awk '{ $1=""; sub(/^ /,""); print }')"
  if [[ -n "${newest:-}" && -d "${newest}" ]]; then
    printf '%s\n' "${newest}"
    return 0
  fi
  # Fallback: first directory
  for d in "${base}"/*; do
    [[ -d "$d" ]] && { printf '%s\n' "$d"; return 0; }
  done
  return 1
}

locate_staged_dir() {
  local base="${STAGE_BASE}"
  [[ -d "${base}" ]] || die "Stage base not found: ${base}. Did servo_prep run successfully?"

  local staged_dir=""
  if [[ -L "${base}/current" ]]; then
    # Resolve current pointer
    if command_exists realpath; then
      staged_dir="$(realpath -m "${base}/current")"
    else
      staged_dir="${base}/$(readlink "${base}/current")"
    fi
  elif [[ -f "${base}/latest.json" ]]; then
    # Parse simplistic latest.json "current" field
    local cur
    cur="$(grep -oE '"current"\s*:\s*"[^"]+"' "${base}/latest.json" | sed -E 's/.*"current"\s*:\s*"([^"]+)".*/\1/')"
    if [[ -n "${cur:-}" && -d "${base}/${cur}" ]]; then
      staged_dir="${base}/${cur}"
    fi
  fi

  # Fallback to newest directory by mtime
  if [[ -z "${staged_dir:-}" ]]; then
    staged_dir="$(find_newest_dir "${base}")" || true
  fi

  [[ -n "${staged_dir:-}" && -d "${staged_dir}" ]] || die "Could not locate staged directory under: ${base}"
  [[ -f "${staged_dir}/servo" ]] || die "Staged binary not found: ${staged_dir}/servo"
  printf '%s\n' "${staged_dir}"
}

write_info_plist() {
  local plist_path="$1"
  local bundle_name="${2:-Verso}"
  local bundle_id="${3:-org.example.verso}"
  local bundle_version="${4:-1.0.0}"

  cat > "${plist_path}" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>                <string>${bundle_name}</string>
  <key>CFBundleDisplayName</key>         <string>${bundle_name}</string>
  <key>CFBundleIdentifier</key>          <string>${bundle_id}</string>
  <key>CFBundleExecutable</key>          <string>verso</string>
  <key>CFBundlePackageType</key>         <string>APPL</string>
  <key>CFBundleVersion</key>             <string>${bundle_version}</string>
  <key>CFBundleShortVersionString</key>  <string>${bundle_version}</string>
  <key>LSMinimumSystemVersion</key>      <string>10.13</string>
  <key>LSApplicationCategoryType</key>   <string>public.app-category.utilities</string>
</dict>
</plist>
EOF
}

stage_app_bundle() {
  local staged_dir="$1"

  log "Staging .app bundle at: ${APP}"
  rm -rf "${APP}"
  mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Resources"

  # Copy binary and resources
  cp "${staged_dir}/servo" "${APP}/Contents/MacOS/verso"
  chmod +x "${APP}/Contents/MacOS/verso"

  if [[ -d "${RES_DIR}" ]]; then
    cp -R "${RES_DIR}" "${APP}/Contents/Resources/static"
  fi

  if [[ -f "${ICON_SRC}" ]]; then
    # If you have an .icns file, prefer that; else copy png for reference.
    cp "${ICON_SRC}" "${APP}/Contents/Resources/icon.png"
  fi

  # Write Info.plist
  write_info_plist "${APP}/Contents/Info.plist" "Verso" "org.example.verso" "1.0.0"
}

maybe_codesign_app() {
  if [[ -z "${CODESIGN_ID}" ]]; then
    warn "CODESIGN_ID not set — skipping codesign of the .app (expected for local dev)."
    return 0
  fi

  if ! command_exists codesign; then
    die "codesign tool not found in PATH but CODESIGN_ID is set."
  fi

  log "Codesigning .app with identity: ${CODESIGN_ID}"
  ( set -x
    codesign ${CODESIGN_OPTS} -s "${CODESIGN_ID}" --timestamp "${APP}"
    codesign --verify --deep --strict --verbose=2 "${APP}"
  )
}

create_dmg() {
  local dmg="${DIST}/Verso.dmg"
  log "Creating DMG at: ${dmg}"

  # Create DMG (compressed, read-only)
  ( set -x
    hdiutil create -volname "Verso" -srcfolder "${APP}" -ov -format UDZO "${dmg}"
  )

  log "DMG created: ${dmg}"
}

main() {
  mkdir -p "${DIST}"

  stage_servo
  local staged_dir
  staged_dir="$(locate_staged_dir)"

  stage_app_bundle "${staged_dir}"
  maybe_codesign_app
  create_dmg

  log "Done."
}

main "$@"
