use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Create a fake Servo source tree with:
/// - a git repository (so `git rev-parse` works),
/// - a fake built binary at `target/<profile>/servo[.exe]`,
/// and return the servo_src path and the built binary path.
fn make_fake_servo_src(tempdir: &tempfile::TempDir, profile: &str) -> (PathBuf, PathBuf) {
    let servo_src = tempdir.path().join("servo_src");
    fs::create_dir_all(&servo_src).expect("create servo_src");

    // Initialize a git repo with one commit so `git rev-parse --short=12 HEAD` works.
    run_ok(
        Command::new("git")
            .arg("-C")
            .arg(&servo_src)
            .arg("init")
            .arg("."),
    );
    // Configure local user to allow commits without global config.
    run_ok(
        Command::new("git")
            .arg("-C")
            .arg(&servo_src)
            .arg("config")
            .arg("user.email")
            .arg("test@example.com"),
    );
    run_ok(
        Command::new("git")
            .arg("-C")
            .arg(&servo_src)
            .arg("config")
            .arg("user.name")
            .arg("Test User"),
    );

    // Create a dummy file to commit
    let readme_path = servo_src.join("README.md");
    fs::write(&readme_path, b"# Fake Servo Repo\n").expect("write README");
    run_ok(
        Command::new("git")
            .arg("-C")
            .arg(&servo_src)
            .arg("add")
            .arg("."),
    );
    run_ok(
        Command::new("git")
            .arg("-C")
            .arg(&servo_src)
            .arg("commit")
            .arg("-m")
            .arg("initial"),
    );

    // Create fake built binary path under target/<profile>/servo[.exe].
    let exe_name = if cfg!(windows) { "servo.exe" } else { "servo" };
    let built_bin = servo_src.join("target").join(profile).join(exe_name);
    if let Some(parent) = built_bin.parent() {
        fs::create_dir_all(parent).expect("create target/<profile> dir");
    }
    // Write a minimal file; content is irrelevant since we don't execute it.
    fs::write(&built_bin, b"echo servo").expect("write fake built binary");
    // On Unix, mark it executable to simulate a real binary.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(&built_bin).expect("stat").permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&built_bin, perm).expect("chmod");
    }

    (servo_src, built_bin)
}

/// Run a command and assert it exited successfully.
fn run_ok(cmd: &mut Command) {
    let out = cmd.output().expect("spawn command");
    if !out.status.success() {
        panic!(
            "command failed: {:?}\nstdout:\n{}\nstderr:\n{}",
            cmd,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Discover the slot directory created by servo_prep under the provided absolute output base:
/// <out_base>/<target_triple>/<profile>/
fn discover_slot_dir(out_base: &Path, profile: &str) -> PathBuf {
    // Expect exactly one target_triple directory under out_base
    let mut target_dirs: Vec<PathBuf> = fs::read_dir(out_base)
        .expect("read out_base")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();

    assert!(
        !target_dirs.is_empty(),
        "no target directories found under {}",
        out_base.display()
    );

    // Prefer a deterministic order
    target_dirs.sort();

    // Pick the first one; this test uses a unique out_base so there should be only one.
    let target_dir = target_dirs.remove(0);
    let slot_dir = target_dir.join(profile);
    assert!(
        slot_dir.exists(),
        "expected slot_dir to exist: {}",
        slot_dir.display()
    );
    slot_dir
}

#[test]
fn write_pointer_default_creates_current_or_commit_pointer() {
    // Arrange: temp dirs
    let td = tempfile::TempDir::new().expect("tempdir");
    let copy_to_td = tempfile::TempDir::new().expect("copy_to tempdir");

    // Use a unique, isolated output base to avoid touching repo defaults
    let out_base = td.path().join("servo_stage_out");
    let profile = "debug";
    let (servo_src, _built) = make_fake_servo_src(&td, profile);

    // Run servo_prep with --write-pointer and isolated --output-dir
    let status = Command::new("cargo")
        .arg("run")
        .arg("-p")
        .arg("servo_prep")
        .arg("--")
        .arg("--servo-src")
        .arg(&servo_src)
        .arg("--metadata-only")
        .arg("--profile")
        .arg(profile)
        .arg("--copy-to")
        .arg(copy_to_td.path())
        .arg("--write-pointer")
        .arg("--output-dir")
        .arg(&out_base)
        .status()
        .expect("spawn cargo run -p servo_prep");
    assert!(
        status.success(),
        "servo_prep run failed with status: {:?}",
        status.code()
    );

    // Locate the slot_dir: <out_base>/<target_triple>/<profile>/
    let slot_dir = discover_slot_dir(&out_base, profile);

    // Preferred pointer: <slot_dir>/current/servo_path.txt
    let current_pointer = slot_dir.join("current").join("servo_path.txt");

    // Fallback: latest.json -> <slot_dir>/<commit>/servo_path.txt
    let final_pointer_path = if current_pointer.exists() {
        current_pointer
    } else {
        // If symlink wasn't created, servo_prep writes latest.json with {"current": "<commit>"}
        let latest = slot_dir.join("latest.json");
        assert!(
            latest.exists(),
            "neither `current/servo_path.txt` nor `latest.json` found in {}",
            slot_dir.display()
        );
        let s = fs::read_to_string(&latest).expect("read latest.json");
        let v: serde_json::Value =
            serde_json::from_str(&s).expect("parse latest.json as JSON object");
        let commit = v
            .get("current")
            .and_then(|x| x.as_str())
            .expect("latest.json missing `current` string");
        slot_dir.join(commit).join("servo_path.txt")
    };

    assert!(
        final_pointer_path.exists(),
        "pointer file not found at {}",
        final_pointer_path.display()
    );

    // Validate the pointer file contents
    let ptr_contents = fs::read_to_string(&final_pointer_path).expect("read pointer file content");
    let staged_path = PathBuf::from(ptr_contents.trim());
    assert!(
        staged_path.is_absolute(),
        "pointer file should contain an absolute path, got: {}",
        staged_path.display()
    );
    assert!(
        staged_path.exists(),
        "staged servo binary does not exist at {}",
        staged_path.display()
    );

    // Verify staged file name normalization.
    let fname = staged_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if cfg!(windows) {
        assert_eq!(
            fname, "servo.exe",
            "staged binary filename should be servo.exe on Windows"
        );
    } else {
        assert_eq!(
            fname, "servo",
            "staged binary filename should be `servo` on Unix"
        );
    }

    // Assert: metadata.toml exists next to staged binary (in the commit dir).
    let commit_dir = staged_path
        .parent()
        .expect("staged binary has no parent dir");
    let metadata_toml = commit_dir.join("metadata.toml");
    assert!(
        metadata_toml.exists(),
        "metadata.toml not found at {}",
        metadata_toml.display()
    );
}
