use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Return platform-appropriate servo binary filename.
fn servo_bin_name() -> &'static str {
    if cfg!(windows) { "servo.exe" } else { "servo" }
}

/// Create an absolute fake binary at the given path and make it executable on Unix.
fn write_fake_binary(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fake binary parent dir");
    }
    fs::write(path, b"fake servo binary").expect("write fake binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(path).expect("stat fake binary").permissions();
        perm.set_mode(0o755);
        fs::set_permissions(path, perm).expect("chmod fake binary");
    }
}

/// Prepare a layout under `root` for latest.json fallback:
/// <root>/third_party/servo-binaries/local/<target>/<profile>/latest.json
/// <root>/third_party/servo-binaries/local/<target>/<profile>/<commit>/servo_path.txt
/// The servo_path.txt contains an absolute path to a fake binary we create.
fn prepare_latest_layout(root: &Path) -> (PathBuf, PathBuf) {
    let target = "x86_64-unknown-linux-gnu";
    let profile = "debug";
    let commit = "abc123def456";
    let slot_dir = root
        .join("third_party")
        .join("servo-binaries")
        .join("local")
        .join(target)
        .join(profile);

    // Write latest.json
    fs::create_dir_all(&slot_dir).expect("create slot dir");
    let latest = slot_dir.join("latest.json");
    let latest_json = format!(
        r#"{{"current":"{commit}","updated_at":"2025-01-01T00:00:00Z"}}"#,
        commit = commit
    );
    fs::write(&latest, latest_json).expect("write latest.json");

    // Create the commit directory and pointer file
    let commit_dir = slot_dir.join(commit);
    fs::create_dir_all(&commit_dir).expect("create commit dir");

    // Create the fake binary at a separate absolute location
    let fake_bin = root.join("fake_bin_dir").join(servo_bin_name());
    write_fake_binary(&fake_bin);
    let fake_bin_abs = fake_bin.canonicalize().unwrap_or(fake_bin.clone());

    // Write pointer file with the absolute path.
    let pointer_path = commit_dir.join("servo_path.txt");
    fs::write(&pointer_path, fake_bin_abs.display().to_string()).expect("write pointer file");

    (slot_dir, fake_bin_abs)
}

#[test]
fn pointer_discovery_uses_latest_json_pointer_file() {
    // Arrange: create temp sandbox with latest.json + commit pointer layout.
    let td = tempfile::TempDir::new().expect("tempdir");
    let root = td.path();
    let (_slot_dir, fake_bin_abs) = prepare_latest_layout(root);

    // Act: run versoview with cwd=root so it scans third_party/.../latest.json and commit/servo_path.txt.
    // Provide no input (stdin=NULL) so the server sees EOF and exits promptly.
    let exe = env!("CARGO_BIN_EXE_photon");
    let output = Command::new(exe)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn versoview");

    // Assert: process should exit successfully after EOF.
    assert!(
        output.status.success(),
        "versoview exited with non-zero status: {:?}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    // The startup should have logged which Servo binary it is using from latest.json path.
    let stderr_str = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr_str.contains("Using Servo binary:"),
        "expected log line announcing discovered servo binary, got:\n{}",
        stderr_str
    );
    assert!(
        stderr_str.contains(&fake_bin_abs.display().to_string()),
        "expected discovered path {} in stderr, got:\n{}",
        fake_bin_abs.display(),
        stderr_str
    );
}
