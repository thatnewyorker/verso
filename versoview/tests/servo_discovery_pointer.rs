use std::fs;
use std::io::Write;
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
    let mut f = fs::File::create(path).expect("create fake binary");
    let _ = f.write_all(b"fake servo binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(path).expect("stat fake binary").permissions();
        perm.set_mode(0o755);
        fs::set_permissions(path, perm).expect("chmod fake binary");
    }
}

/// Prepare a layout under `root`:
/// <root>/third_party/servo-binaries/local/<target>/<profile>/current/servo_path.txt
/// The servo_path.txt contains an absolute path to a fake binary we create.
fn prepare_pointer_layout(root: &Path) -> (PathBuf, PathBuf) {
    let target = "x86_64-unknown-linux-gnu";
    let profile = "debug";
    let slot_dir = root
        .join("third_party")
        .join("servo-binaries")
        .join("local")
        .join(target)
        .join(profile);
    let current_dir = slot_dir.join("current");
    fs::create_dir_all(&current_dir).expect("create current dir");

    // Create the fake binary in a different absolute path to ensure it's not under current/.
    let fake_bin = root.join("fake_bin_dir").join(servo_bin_name());
    write_fake_binary(&fake_bin);
    let fake_bin_abs = fake_bin.canonicalize().unwrap_or(fake_bin.clone());

    // Write pointer file with the absolute path.
    let pointer_path = current_dir.join("servo_path.txt");
    fs::write(&pointer_path, fake_bin_abs.display().to_string()).expect("write pointer file");

    (slot_dir, fake_bin_abs)
}

#[test]
fn pointer_discovery_uses_current_pointer_file() {
    // Arrange: create temp sandbox with pointer layout.
    let td = tempfile::TempDir::new().expect("tempdir");
    let root = td.path();
    let (_slot_dir, fake_bin_abs) = prepare_pointer_layout(root);

    // Act: run versoview with cwd=root so it scans third_party/.../current/servo_path.txt.
    // Provide no input (stdin=NULL) so the server sees EOF and exits promptly.
    let exe = env!("CARGO_BIN_EXE_versoview");
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

    // The startup should have logged which Servo binary it is using.
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
