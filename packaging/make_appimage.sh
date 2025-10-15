#!/usr/bin/env bash
# Example script: package Verso/Servo as an AppImage on Linux.
# This is a template intended for local use and CI. Adjust paths and names as needed.

set -euo pipefail
IFS=$'\n\t'
umask 022

# Resolve workspace root (this script lives under verso/packaging/)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
DIST="${ROOT}/dist"
APPDIR="${DIST}/Verso.AppDir"

# Paths for staged servo binary created by servo_prep
TARGET_TRIPLE="$(rustc -vV | awk '/^host:/{print $2}')"
STAGE_BASE="${ROOT}/third_party/servo-binaries/local/${TARGET_TRIPLE}/release"

# Resources (optional)
RES_DIR="${ROOT}/static"
ICON_SRC="${ROOT}/assets/icon.png"

# External tool (optional)
APPIMAGETOOL_BIN="$(command -v appimagetool || true)"

command_exists() {
  command -v "$1" >/dev/null 2>&1
}

log() { printf '%s\n' "==> $*"; }
warn() { printf '%s\n' "WARN: $*" >&2; }
die() { printf '%s\n' "ERROR: $*" >&2; exit 1; }

# Stage or reuse the Servo binary via servo_prep.
# You can override SERVO_SRC via environment, otherwise this script assumes ../servo relative to workspace.
stage_servo() {
  local servo_src="${SERVO_SRC:-${ROOT}/../servo}"
  if [[ ! -d "${servo_src}" ]]; then
    warn "Servo source directory not found at: ${servo_src}"
    warn "Set SERVO_SRC=/absolute/path/to/servo or adjust the path in this script."
    die "Servo sources are required to build or locate the binary."
  fi

  if ! command_exists cargo; then
    die "cargo is not in PATH. Install Rust toolchain (https://rustup.rs/) and retry."
  fi

  log "Staging Servo binary via servo_prep (metadata-only if already built)…"
  ( set -x
    cargo run -p servo_prep -- \
      --servo-src "${servo_src}" \
      --profile release \
      --metadata-only
  )
}

# Pick the newest commit directory under STAGE_BASE as a fallback if "current" pointer is missing.
find_newest_dir() {
  local base="$1"
  # GNU find with -printf is available on Linux runners.
  local newest
  newest="$(find "${base}" -mindepth 1 -maxdepth 1 -type d -printf '%T@ %p\n' 2>/dev/null | sort -nr | head -n1 | awk '{print $2}')"
  if [[ -n "${newest:-}" && -d "${newest}" ]]; then
    printf '%s\n' "${newest}"
    return 0
  fi
  # Fallback: first directory in lexicographic order
  for d in "${base}"/*; do
    [[ -d "$d" ]] && { printf '%s\n' "$d"; return 0; }
  done
  return 1
}

# Locate staged directory that contains servo and metadata.toml
locate_staged_dir() {
  local base="${STAGE_BASE}"
  [[ -d "${base}" ]] || die "Stage base not found: ${base}. Did servo_prep run successfully?"

  local staged_dir=""
  if [[ -L "${base}/current" ]]; then
    # Prefer "current" pointer if present
    if command_exists realpath; then
      staged_dir="$(realpath -m "${base}/current")"
    else
      # Try readlink -f (GNU), else resolve manually as best-effort
      if readlink -f "${base}/current" >/dev/null 2>&1; then
        staged_dir="$(readlink -f "${base}/current")"
      else
        staged_dir="${base}/$(readlink "${base}/current")"
      fi
    fi
  elif [[ -f "${base}/latest.json" ]]; then
    # No jq by default: parse the "current" value with sed/grep best-effort
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
  [[ -f "${staged_dir}/metadata.toml" ]] || warn "metadata.toml not found in ${staged_dir}"

  printf '%s\n' "${staged_dir}"
}

prepare_appdir() {
  local appdir="$1"
  log "Preparing AppDir at: ${appdir}"
  rm -rf "${appdir}"
  mkdir -p "${appdir}/usr/bin" \
           "${appdir}/usr/share/applications" \
           "${appdir}/usr/share/icons/hicolor/256x256/apps" \
           "${appdir}/usr/share/doc/verso"

  # AppRun — simple launcher
  cat > "${appdir}/AppRun" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "${HERE}/usr/bin/verso" "$@"
EOF
  chmod +x "${appdir}/AppRun"

  # Desktop entry
  cat > "${appdir}/verso.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=Verso
Comment=Verso app (Servo-based)
Exec=verso
Icon=verso
Categories=Utility;
Terminal=false
EOF
}

copy_payloads_into_appdir() {
  local appdir="$1"
  local staged_dir="$2"

  # Copy binary
  cp "${staged_dir}/servo" "${appdir}/usr/bin/verso"
  chmod +x "${appdir}/usr/bin/verso"

  # Copy metadata (optional, useful for audit)
  if [[ -f "${staged_dir}/metadata.toml" ]]; then
    cp "${staged_dir}/metadata.toml" "${appdir}/usr/share/doc/verso/metadata.toml"
  fi

  # Copy resources if present
  if [[ -d "${RES_DIR}" ]]; then
    cp -r "${RES_DIR}" "${appdir}/usr/bin/static"
  fi

  # Icon if present
  if [[ -f "${ICON_SRC}" ]]; then
    cp "${ICON_SRC}" "${appdir}/usr/share/icons/hicolor/256x256/apps/verso.png"
  fi
}

maybe_build_appimage() {
  local appdir="$1"
  local out_path="$2"

  if [[ -z "${APPIMAGETOOL_BIN}" ]]; then
    warn "appimagetool not found in PATH; leaving staged AppDir at: ${appdir}"
    warn "Install appimagetool to produce an AppImage, e.g.: https://github.com/AppImage/AppImageKit"
    return 0
  fi

  log "Building AppImage with appimagetool…"
  ( set -x
    "${APPIMAGETOOL_BIN}" "${appdir}" "${out_path}"
  )
  log "AppImage created at: ${out_path}"
}

main() {
  mkdir -p "${DIST}"

  stage_servo
  local staged_dir
  staged_dir="$(locate_staged_dir)"

  prepare_appdir "${APPDIR}"
  copy_payloads_into_appdir "${APPDIR}" "${staged_dir}"

  local appimage_out="${DIST}/Verso-${TARGET_TRIPLE}.AppImage"
  maybe_build_appimage "${APPDIR}" "${appimage_out}"

  log "Done."
}

main "$@"
