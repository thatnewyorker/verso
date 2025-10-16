use std::fs;
use std::path::PathBuf;
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

/// This test verifies that --strict-pointer causes servo_prep to fail when the explicit pointer
/// path is not writable. We simulate this in a cross-platform way by creating a directory at
/// the path where a file is expected (so the atomic rename/persist will fail).
#[test]
fn strict_pointer_fails_when_pointer_path_is_directory() {
    // Arrange: temp dirs
    let td = tempfile::TempDir::new().expect("tempdir");
    let copy_to_td = tempfile::TempDir::new().expect("copy_to tempdir");
    let profile = "debug";

    let (servo_src, _built) = make_fake_servo_src(&td, profile);

    // Create a directory at the pointer "file" path to force failure on write.
    let pointer_dir_parent = tempfile::TempDir::new().expect("pointer parent tempdir");
    let pointer_path = pointer_dir_parent.path().join("servo_path.txt");
    fs::create_dir_all(&pointer_path).expect("create non-writable pointer path as directory");

    // Act: run servo_prep with --servo-out pointing at a directory and --strict-pointer
    let output = Command::new("cargo")
        .arg("run")
        .arg("-p")
        .arg("servo_prep")
        .arg("--")
        .arg("--servo-src")
        .arg(&servo_src)
        .arg("--metadata-only")
        .arg("--profile")
        .arg(profile)
        // avoid writing to the repo default "versoview/" copy target:
        .arg("--copy-to")
        .arg(copy_to_td.path())
        // avoid creating symlink/manifest in current slot to reduce side effects:
        .arg("--no-current-pointer")
        // write the pointer file to a path that is actually a directory:
        .arg("--servo-out")
        .arg(&pointer_path)
        // require strict failure on pointer write error:
        .arg("--strict-pointer")
        .output()
        .expect("spawn cargo run -p servo_prep");

    // Assert: servo_prep must fail when strict pointer is requested and write fails.
    if output.status.success() {
        panic!(
            "expected servo_prep to fail due to strict pointer write error, but it succeeded.\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
