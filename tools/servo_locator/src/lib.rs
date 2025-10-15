/*!
servo_locator: locate a prepared Servo binary and its metadata staged under
verso/third_party/servo-binaries.

Overview
- This small helper library resolves the path to a Servo binary produced by a local
  build-and-stage step (e.g., the `servo_prep` CLI).
- It implements the lookup precedence described in the implementation plan:
  1) VERSO_SERVO_BIN environment variable (highest precedence)
  2) verso-managed directory:
     - <root>/third_party/servo-binaries/local/<target-triple>/<profile>/{current|latest.json|<commit>}/servo[.exe]
     - If no explicit triple is provided, attempt to detect host triple via `rustc -vV` or scan.
     - If no profile specified, try "release" then "debug".
  3) If nothing is found, return None.

Usage
- Integrate this library from versoview or host components to centralize Servo
  binary discovery.
- Example:
    if let Some(art) = servo_locator::locate_prepared_servo(None, None) {
        println!("Servo binary: {}", art.binary.display());
        if let Some(meta) = art.metadata {
            println!("Commit: {}", meta.servo_commit);
        }
    }

Notes
- This crate intentionally avoids panicking; it returns None on failure, logging is left
  to the caller.
- It is tolerant to layout variations (e.g., symlink vs manifest vs direct commit folder).
- It understands the Windows .exe suffix and normalizes the binary filename to "servo[.exe]".

*/

use std::fs;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Metadata written by the prepare tool (servo_prep), stored adjacent to the binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServoMetadata {
    pub servo_commit: String,
    pub build_profile: String,
    pub enabled_features: Vec<String>,
    pub timestamp: String,
    pub target_triple: String,
    pub rust_toolchain: String,
    pub binary_name: String,
}

/// A resolved, ready-to-use Servo artifact.
#[derive(Debug, Clone)]
pub struct PreparedServo {
    /// Full filesystem path to the Servo binary.
    pub binary: PathBuf,
    /// Optional parsed metadata from `metadata.toml` next to the binary.
    pub metadata: Option<ServoMetadata>,
}

/// Locate a prepared Servo binary using common conventions.
///
/// Precedence:
/// - If VERSO_SERVO_BIN is set and exists, return that (with metadata if available).
/// - Else scan the verso-managed directory layout:
///   <root>/third_party/servo-binaries/local/<triple>/<profile>/{current|latest.json|<commit>}/servo[.exe]
///
/// Parameters:
/// - preferred_profile: Some("release" | "debug") or None to try "release" then "debug"
/// - preferred_triple: Some("<target-triple>") or None to detect host triple or scan all
///
/// Returns:
/// - Some(PreparedServo) if found, otherwise None.
pub fn locate_prepared_servo(
    preferred_profile: Option<&str>,
    preferred_triple: Option<&str>,
) -> Option<PreparedServo> {
    // 1) Env override
    if let Some(bin) = env_servo_bin() {
        let meta = read_metadata_for_binary(&bin).ok();
        return Some(PreparedServo {
            binary: bin,
            metadata: meta,
        });
    }

    // 2) Verso-managed layout
    let base = find_local_artifacts_base()?;
    let triples = match preferred_triple {
        Some(triple) => vec![triple.to_string()],
        None => match detect_host_triple() {
            Ok(t) => vec![t],
            Err(_) => list_dir_names(&base).unwrap_or_default(), // fallback to scanning all triples
        },
    };

    let profiles = {
        if let Some(p) = preferred_profile {
            vec![p.to_string()]
        } else {
            vec!["release".to_string(), "debug".to_string()]
        }
    };

    for triple in &triples {
        for profile in &profiles {
            if let Some(art) = locate_in_slot(&base, triple, profile) {
                return Some(art);
            }
        }
    }

    None
}

/// Convenience: return only the binary path if found (drop metadata).
pub fn locate_servo_binary(
    preferred_profile: Option<&str>,
    preferred_triple: Option<&str>,
) -> Option<PathBuf> {
    locate_prepared_servo(preferred_profile, preferred_triple).map(|p| p.binary)
}

/// Lowest-level probing for a given (<base>, <triple>, <profile>) slot.
///
/// Tries the following inside `<base>/<triple>/<profile>`:
/// - "current" symlink pointing to a commit directory,
/// - "latest.json" manifest declaring the commit directory,
/// - otherwise, pick the most recent-looking commit directory by metadata or mtime.
///
/// Additionally tolerates an accidental "current"/"latest.json" one level up at `<base>/<triple>`.
fn locate_in_slot(base: &Path, triple: &str, profile: &str) -> Option<PreparedServo> {
    let slot = base.join(triple).join(profile);
    // First, try the expected location: <base>/<triple>/<profile>/{current|latest.json|<commit>}
    if let Some(art) = locate_within_slot_dir(&slot) {
        return Some(art);
    }
    // Tolerate a misplacement where "current" or "latest.json" was created at <base>/<triple>
    let legacy_slot = base.join(triple);
    locate_within_slot_dir(&legacy_slot)
}

fn locate_within_slot_dir(slot_dir: &Path) -> Option<PreparedServo> {
    // 1) current symlink
    if let Some(commit_dir) = resolve_current_symlink(slot_dir) {
        if let Some(art) = make_artifact_from_commit_dir(&commit_dir) {
            return Some(art);
        }
    }
    // 2) latest.json manifest
    if let Some(commit_dir) = resolve_latest_manifest(slot_dir) {
        if let Some(art) = make_artifact_from_commit_dir(&commit_dir) {
            return Some(art);
        }
    }
    // 3) Pick a commit directory by scanning
    let commit_dirs = list_child_dirs(slot_dir).unwrap_or_default();
    // Prefer newest by metadata timestamp, then by mtime, then by name
    let mut scored: Vec<(i64, i64, String, PathBuf)> = Vec::new(); // (ts, mtime, name, path)
    for dir in commit_dirs {
        let ts = read_metadata_timestamp(&dir).unwrap_or(0);
        let mt = dir
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|e| -(e.as_secs() as i64)) // smaller elapsed => more recent (negate to sort desc)
            .unwrap_or(0);
        let name = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        scored.push((ts, mt, name, dir));
    }
    // Sort by ts desc, then mt desc, then name desc
    scored.sort_by(|a, b| b.cmp(a));
    if let Some((_, _, _, dir)) = scored.into_iter().next() {
        return make_artifact_from_commit_dir(&dir);
    }
    None
}

fn make_artifact_from_commit_dir(commit_dir: &Path) -> Option<PreparedServo> {
    let bin_name = normalize_bin_name("servo");
    let bin = commit_dir.join(&bin_name);
    if bin.exists() {
        let meta = read_metadata_for_binary(&bin).ok();
        return Some(PreparedServo {
            binary: bin,
            metadata: meta,
        });
    }
    None
}

fn resolve_current_symlink(slot_dir: &Path) -> Option<PathBuf> {
    let link = slot_dir.join("current");
    if !link.exists() {
        return None;
    }
    match fs::read_link(&link) {
        Ok(target_rel) => {
            // Symlink target is expected to be a relative dir name (commit sha).
            let commit_dir = slot_dir.join(target_rel);
            if commit_dir.is_dir() {
                Some(commit_dir)
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

#[derive(Deserialize)]
struct LatestManifest {
    current: String,
    #[allow(dead_code)]
    updated_at: Option<String>,
}

fn resolve_latest_manifest(slot_dir: &Path) -> Option<PathBuf> {
    let p = slot_dir.join("latest.json");
    let data = fs::read(&p).ok()?;
    let m: LatestManifest = serde_json::from_slice(&data).ok()?;
    let commit_dir = slot_dir.join(m.current);
    if commit_dir.is_dir() {
        Some(commit_dir)
    } else {
        None
    }
}

/// Attempt to read `metadata.toml` next to a known binary path.
pub fn read_metadata_for_binary(bin: &Path) -> Result<ServoMetadata> {
    let dir = bin
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent directory for {}", bin.display()))?;
    read_metadata(dir)
}

/// Read metadata from a prepared artifact directory.
pub fn read_metadata(dir: &Path) -> Result<ServoMetadata> {
    let p = dir.join("metadata.toml");
    let data = fs::read(&p).with_context(|| format!("read {}", p.display()))?;
    let s = std::str::from_utf8(&data)
        .with_context(|| format!("metadata.toml not utf-8: {}", p.display()))?;
    let meta: ServoMetadata =
        toml::from_str(s).with_context(|| format!("parse {}", p.display()))?;
    Ok(meta)
}

fn read_metadata_timestamp(dir: &Path) -> Option<i64> {
    // Parse RFC3339 timestamp from metadata; return unix-ish coarse score for sorting
    read_metadata(dir)
        .ok()
        .and_then(|m| parse_rfc3339_as_seconds(&m.timestamp).ok())
}

fn parse_rfc3339_as_seconds(s: &str) -> Result<i64> {
    // A coarse parser: try std chrono via RFC3339 w/ time crate not available here — fall back to naive.
    // Since we cannot depend on `time` here, accept a simple heuristic: YYYY-MM-DDTHH:MM:SSZ
    // and compute a lexicographic score. If format unexpected, return 0.
    let cleaned = s.replace(['-', ':', 'T', 'Z', '+', '.'], "");
    let score = cleaned
        .chars()
        .take(14)
        .filter_map(|c| c.to_digit(10))
        .fold(0i64, |acc, d| {
            acc.saturating_mul(10).saturating_add(d as i64)
        });
    Ok(score)
}

/// If set and valid, return the path from VERSO_SERVO_BIN.
fn env_servo_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("VERSO_SERVO_BIN") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Find the base directory where prepared artifacts are stored:
/// - If VERSO_SERVO_LOCAL_DIR is set, use it.
/// - Otherwise, walk up from the current executable to locate a `third_party/servo-binaries/local` directory.
/// - Fallback to `./third_party/servo-binaries/local` relative to current dir.
fn find_local_artifacts_base() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("VERSO_SERVO_LOCAL_DIR") {
        let path = PathBuf::from(p);
        if path.is_dir() {
            return Some(path);
        }
    }
    // try walking up from current_exe
    if let Ok(mut cur) = std::env::current_exe() {
        // current_exe points to the binary file; go to its parent dir
        if cur.pop() {
            for _ in 0..6 {
                let cand = cur.join("third_party").join("servo-binaries").join("local");
                if cand.is_dir() {
                    return Some(cand);
                }
                if !cur.pop() {
                    break;
                }
            }
        }
    }
    // try current directory
    let cwd = std::env::current_dir().ok()?;
    let cand = cwd.join("third_party").join("servo-binaries").join("local");
    if cand.is_dir() { Some(cand) } else { None }
}

fn normalize_bin_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn list_dir_names(p: &Path) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let rd = fs::read_dir(p).ok()?;
    for ent in rd.flatten() {
        if let Ok(md) = ent.metadata() {
            if md.is_dir() {
                if let Some(name) = ent.file_name().to_str() {
                    out.push(name.to_string());
                }
            }
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn list_child_dirs(p: &Path) -> Option<Vec<PathBuf>> {
    let mut out = Vec::new();
    let rd = fs::read_dir(p).ok()?;
    for ent in rd.flatten() {
        if let Ok(md) = ent.metadata() {
            if md.is_dir() {
                out.push(ent.path());
            }
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Attempt to detect the host target triple via `rustc -vV`.
fn detect_host_triple() -> Result<String> {
    let out = Command::new("rustc").arg("-vV").output()?;
    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "rustc -vV failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("host:") {
            return Ok(rest.trim().to_string());
        }
    }
    Err(anyhow::anyhow!(
        "could not parse host triple from rustc -vV"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_bin_name() {
        let n = normalize_bin_name("servo");
        if cfg!(windows) {
            assert_eq!(n, "servo.exe");
        } else {
            assert_eq!(n, "servo");
        }
    }

    #[test]
    fn test_parse_rfc3339_score() {
        // Monotonic-ish scores for increasing timestamps
        let a = parse_rfc3339_as_seconds("2025-01-01T00:00:00Z").unwrap();
        let b = parse_rfc3339_as_seconds("2025-02-01T00:00:00Z").unwrap();
        assert!(b > a);
    }

    #[test]
    fn env_override_not_set() {
        // Should be None if env var not set.
        unsafe {
            std::env::set_var("VERSO_SERVO_BIN", "__verso_test_dne__");
        }
        assert!(env_servo_bin().is_none());
    }
}
