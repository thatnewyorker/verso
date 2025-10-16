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

#[test]
fn metadata_only_and_pointer_file_are_created() {
    // Arrange: temp dirs
    let td = tempfile::TempDir::new().expect("tempdir");
    let copy_to_td = tempfile::TempDir::new().expect("copy_to tempdir");
    let pointer_dir = tempfile::TempDir::new().expect("pointer tempdir");

    let profile = "debug";
    let (servo_src, _built) = make_fake_servo_src(&td, profile);
    let pointer_path = pointer_dir.path().join("servo_path.txt");

    // Build the command to run servo_prep via cargo so tests don't depend on CARGO_BIN_EXE.
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
        // avoid writing to the repo default "versoview/" copy target:
        .arg("--copy-to")
        .arg(copy_to_td.path())
        // avoid creating symlink/manifest in current slot:
        .arg("--no-current-pointer")
        // write the pointer file to our temp path:
        .arg("--servo-out")
        .arg(&pointer_path)
        .status()
        .expect("spawn cargo run -p servo_prep");
    assert!(
        status.success(),
        "servo_prep metadata-only run failed with status: {:?}",
        status.code()
    );

    // Assert: pointer file exists and contains an absolute path.
    let ptr_contents = fs::read_to_string(&pointer_path).expect("read pointer file content");
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

    // Additionally, assert the staged filename is normalized to "servo[.exe]".
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

    // Finally, verify that the copy-to destination was also populated (directory or file).
    // When a directory is passed to --copy-to, the binary filename is appended.
    let copy_to_bin = {
        let filename = if cfg!(windows) { "servo.exe" } else { "servo" };
        copy_to_td.path().join(filename)
    };
    assert!(
        copy_to_bin.exists(),
        "expected copy-to destination to contain staged binary: {}",
        copy_to_bin.display()
    );
}
