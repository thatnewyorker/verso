use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use clap::{ArgAction, Parser, ValueEnum};
use hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use walkdir::WalkDir;
use which::which;
use zip::write::FileOptions;

#[derive(Debug, Clone, ValueEnum)]
enum BuildProfile {
    Debug,
    Release,
}

impl BuildProfile {
    fn as_dir(&self) -> &'static str {
        match self {
            BuildProfile::Debug => "debug",
            BuildProfile::Release => "release",
        }
    }
}

impl std::fmt::Display for BuildProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_dir())
    }
}

#[derive(Debug, Clone, ValueEnum)]
enum PackageKind {
    Zip,
    AppImage,
    Dmg,
    Msi,
}

#[derive(Debug, Parser)]
#[command(
    name = "servo_prep",
    version,
    about = "Build Servo from a local checkout and stage the artifact under verso/third_party/servo-binaries with metadata."
)]
struct Args {
    /// Path to the Servo source checkout (takes precedence over env and config).
    #[arg(long)]
    servo_src: Option<PathBuf>,

    /// Build profile (debug or release).
    #[arg(long, value_enum)]
    profile: Option<BuildProfile>,

    /// Optional Cargo target triple (for cross builds).
    #[arg(long)]
    target: Option<String>,

    /// Comma-separated list of Cargo feature flags to pass to Servo build.
    #[arg(long)]
    features: Option<String>,

    /// Name of the Cargo binary target to build (e.g., servo, servoshell).
    #[arg(long)]
    binary_name: Option<String>,

    /// Output directory under the Verso repository (relative to workspace root).
    /// Defaults to third_party/servo-binaries/local
    #[arg(long)]
    output_dir: Option<PathBuf>,

    /// Optional rustup toolchain to use (e.g., 1.90.0, stable, nightly).
    /// When provided, this is invoked via `cargo +<toolchain> build`.
    #[arg(long)]
    toolchain: Option<String>,

    /// If set, do not invoke Cargo build; just discover existing Servo artifact and copy + write metadata.
    #[arg(long, action = ArgAction::SetTrue)]
    metadata_only: bool,

    /// Optional explicit path to a build config file (TOML). Defaults to <workspace>/servo-build-config.toml
    #[arg(long)]
    config: Option<PathBuf>,

    /// If set, log extra diagnostics.
    #[arg(long, action = ArgAction::SetTrue)]
    verbose: bool,

    /// If set, do not create or update the `current` symlink or `latest.json` manifest.
    #[arg(long, action = ArgAction::SetTrue)]
    no_current_pointer: bool,

    /// Optionally copy the staged binary to an additional path inside the project.
    /// If a directory is provided, the binary filename will be appended.
    #[arg(long)]
    copy_to: Option<PathBuf>,

    /// Optional packaging format to produce (zip, appimage, dmg, msi).
    #[arg(long, value_enum)]
    package: Option<PackageKind>,

    /// One or more resource paths to include alongside the binary in the package.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    bundle_resources: Vec<PathBuf>,

    /// Output directory for packaged artifacts (defaults to <workspace>/dist).
    #[arg(long)]
    out_dir: Option<PathBuf>,

    /// Optional file to write the absolute path of the staged servo binary.
    /// If provided, this file will be created/overwritten with a single line path.
    #[arg(long)]
    servo_out: Option<PathBuf>,

    /// If set, write a convenience pointer under the staging slot: `current/servo_path.txt`
    /// (or `<commit>/servo_path.txt` if symlinks are unavailable). Best effort.
    #[arg(long, action = ArgAction::SetTrue)]
    write_pointer: bool,

    /// If set, fail the run on pointer write errors (affects --servo-out and --write-pointer).
    #[arg(long, action = ArgAction::SetTrue)]
    strict_pointer: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct FileConfig {
    /// Path to the Servo source checkout.
    servo_src: Option<PathBuf>,
    /// Which Cargo binary to build in the Servo workspace (default "servo")
    target_binary: Option<String>,
    /// Profile string: "debug" or "release".
    profile: Option<String>,
    /// Servo Cargo features to enable.
    features: Option<Vec<String>>,
    /// Output directory relative to Verso workspace root.
    output_dir: Option<PathBuf>,
    /// Recommended Rust toolchain (informational). If set and --toolchain unset, it will be used.
    rust_toolchain: Option<String>,
    /// If true, do not create/update the `current` symlink or `latest.json` manifest.
    no_current_pointer: Option<bool>,
    /// Additional project paths to copy the staged binary into. Interpreted as
    /// directories (append the binary name) unless a filename is provided.
    copy_to: Option<Vec<PathBuf>>,
}

#[derive(Debug, Serialize)]
struct ArtifactMetadata {
    servo_commit: String,
    build_profile: String,
    enabled_features: Vec<String>,
    timestamp: String,
    target_triple: String,
    rust_toolchain: String,
    binary_name: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let ws_root = find_workspace_root().context(
        "Failed to locate Verso workspace root (directory containing Cargo.toml with [workspace])",
    )?;
    let cfg_path = args
        .config
        .clone()
        .unwrap_or_else(|| ws_root.join("servo-build-config.toml"));

    let cfg = read_config(cfg_path.as_path()).unwrap_or_default();

    if args.verbose {
        eprintln!("Workspace root: {}", ws_root.display());
        eprintln!(
            "Using config file: {} (exists: {})",
            cfg_path.display(),
            cfg_path.exists()
        );
    }

    let servo_src = determine_servo_src(&args, &cfg)?;
    if args.verbose {
        eprintln!("Servo source: {}", servo_src.display());
    }

    which("git").context("git is required to read Servo commit")?;
    which("cargo").context("cargo is required to build Servo")?;

    // Determine target triple.
    let target_triple = match &args.target {
        Some(t) => t.clone(),
        None => detect_host_triple()?,
    };

    // Determine build profile with precedence: CLI > config > default Release.
    let profile = args
        .profile
        .clone()
        .or_else(|| {
            cfg.profile.as_deref().and_then(|p| match p {
                "release" => Some(BuildProfile::Release),
                "debug" => Some(BuildProfile::Debug),
                _ => None,
            })
        })
        .unwrap_or(BuildProfile::Release);

    // Determine features.
    let features = determine_features(&args, &cfg);

    // Determine binary target name with precedence: CLI > config > default "servo".
    let binary_name = args
        .binary_name
        .clone()
        .or_else(|| cfg.target_binary.clone())
        .unwrap_or_else(|| "servo".to_string());

    // Determine toolchain string
    let toolchain = args
        .toolchain
        .clone()
        .or_else(|| cfg.rust_toolchain.clone());

    // Determine output base dir
    let output_base = args
        .output_dir
        .clone()
        .or_else(|| cfg.output_dir.clone())
        .unwrap_or_else(|| PathBuf::from("third_party/servo-binaries/local"));

    // Optionally build
    if !args.metadata_only {
        build_servo(
            &servo_src,
            &binary_name,
            &profile,
            &features,
            args.target.as_deref(),
            toolchain.as_deref(),
            args.verbose,
        )?;
    } else if args.verbose {
        eprintln!("--metadata-only set: skipping cargo build");
    }

    // Find the built artifact in Servo target dir.
    let host_exe_name = bin_name_for_host(&binary_name);
    let built_path =
        locate_servo_artifact(&servo_src, &host_exe_name, &profile, args.target.as_deref())?;
    if args.verbose {
        eprintln!("Located built artifact: {}", built_path.display());
    }

    // Gather metadata
    let commit = read_git_commit_short(&servo_src)?;
    let rust_toolchain_version = detect_rustc_version()?;
    let ts = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".into());

    // Compute final output dir: <ws_root>/<output_base>/<target>/<profile>/<commit>/
    let dest_dir = ws_root
        .join(output_base)
        .join(&target_triple)
        .join(profile.as_dir())
        .join(&commit);
    fs::create_dir_all(&dest_dir).with_context(|| format!("create dir {}", dest_dir.display()))?;

    // Copy binary into deterministic filename "servo" (normalize name) plus platform ext.
    let dest_bin_name = bin_name_for_host("servo"); // normalize to "servo[.exe]"
    let dest_bin = dest_dir.join(&dest_bin_name);
    fs::copy(&built_path, &dest_bin)
        .with_context(|| format!("copy {} -> {}", built_path.display(), dest_bin.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(&dest_bin)?.permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&dest_bin, perm)?;
    }

    // Write metadata.toml
    let meta = ArtifactMetadata {
        servo_commit: commit.clone(),
        build_profile: profile.to_string(),
        enabled_features: features.clone(),
        timestamp: ts,
        target_triple: target_triple.clone(),
        rust_toolchain: toolchain
            .clone()
            .unwrap_or_else(|| rust_toolchain_version.clone()),
        binary_name: binary_name.clone(),
    };
    let meta_toml = toml::to_string_pretty(&meta)?;
    let meta_path = dest_dir.join("metadata.toml");
    fs::write(&meta_path, meta_toml)?;

    // Update "current" symlink or latest.json manifest at <ws_root>/<output_base>/<target>/<profile>/
    let slot_dir = dest_dir.parent().map(PathBuf::from).ok_or_else(|| {
        anyhow!(
            "unexpected layout computing slot dir from {}",
            dest_dir.display()
        )
    })?;
    let disable_pointer = args.no_current_pointer || cfg.no_current_pointer.unwrap_or(false);
    if !disable_pointer {
        update_current_pointer(&slot_dir, &commit, args.verbose)?;
    }

    // Optionally copy to additional destination path(s) inside the project.
    // Default: if no copy targets are configured or passed via CLI, copy into "versoview/"
    // relative to the workspace root. Relative targets are resolved from the workspace root.
    let mut extra_targets: Vec<PathBuf> = Vec::new();
    if let Some(cfg_targets) = cfg.copy_to.as_ref() {
        extra_targets.extend(cfg_targets.iter().cloned());
    }
    if let Some(cli_target) = args.copy_to.as_ref() {
        extra_targets.push(cli_target.clone());
    }
    if extra_targets.is_empty() {
        // Default destination inside the project when none specified.
        extra_targets.push(PathBuf::from("versoview"));
    }
    for extra in extra_targets {
        // Resolve relative paths from the workspace root.
        let extra_resolved = if extra.is_absolute() {
            extra.clone()
        } else {
            ws_root.join(&extra)
        };

        let dest_path = if extra_resolved.is_dir() || !extra_resolved.extension().is_some() {
            // Treat as directory; append the binary filename
            let _ = fs::create_dir_all(&extra_resolved);
            extra_resolved.join(&dest_bin_name)
        } else {
            // Treat as a file path
            if let Some(parent) = extra_resolved.parent() {
                let _ = fs::create_dir_all(parent);
            }
            extra_resolved.clone()
        };
        fs::copy(&dest_bin, &dest_path)
            .with_context(|| format!("copy {} -> {}", dest_bin.display(), dest_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = fs::metadata(&dest_path)?.permissions();
            perm.set_mode(0o755);
            fs::set_permissions(&dest_path, perm)?;
        }
    }

    // Write pointer file to staged servo binary if requested
    if let Some(ptr_path) = args.servo_out.as_ref() {
        let staged_abs = dest_bin.canonicalize().unwrap_or(dest_bin.clone());
        if let Some(parent) = ptr_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(e) = fs::write(ptr_path, staged_abs.display().to_string()) {
            if args.strict_pointer {
                return Err(anyhow!(
                    "failed to write servo pointer file {}: {}",
                    ptr_path.display(),
                    e
                ));
            }
            if args.verbose {
                eprintln!(
                    "Warning: failed to write servo pointer file {}: {}",
                    ptr_path.display(),
                    e
                );
            }
        } else if args.verbose {
            eprintln!(
                "Wrote servo pointer file: {} -> {}",
                ptr_path.display(),
                staged_abs.display()
            );
        }
    }

    // Convenience: write pointer in current/ or commit dir when --write-pointer is set
    if args.write_pointer {
        let staged_abs = dest_bin.canonicalize().unwrap_or(dest_bin.clone());
        let current_path = slot_dir.join("current");
        let pointer_path = if !disable_pointer && current_path.exists() {
            current_path.join("servo_path.txt")
        } else {
            // Fall back to commit dir, optionally using latest.json if present.
            let mut commit_dir = slot_dir.join(&commit);
            let latest_path = slot_dir.join("latest.json");
            if latest_path.exists() {
                if let Ok(s) = fs::read_to_string(&latest_path) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                        if let Some(c) = v.get("current").and_then(|x| x.as_str()) {
                            commit_dir = slot_dir.join(c);
                        }
                    }
                }
            }
            commit_dir.join("servo_path.txt")
        };

        if let Some(parent) = pointer_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(e) = fs::write(&pointer_path, staged_abs.display().to_string()) {
            if args.strict_pointer {
                return Err(anyhow!(
                    "failed to write --write-pointer file {}: {}",
                    pointer_path.display(),
                    e
                ));
            }
            if args.verbose {
                eprintln!(
                    "Warning: failed to write --write-pointer file {}: {}",
                    pointer_path.display(),
                    e
                );
            }
        } else if args.verbose {
            eprintln!(
                "Wrote convenience pointer: {} -> {}",
                pointer_path.display(),
                staged_abs.display()
            );
        }
    }

    // Optional packaging step
    if let Some(kind) = args.package.clone() {
        let out_dir = args.out_dir.clone().unwrap_or_else(|| ws_root.join("dist"));
        fs::create_dir_all(&out_dir)
            .with_context(|| format!("create dir {}", out_dir.display()))?;
        match kind {
            PackageKind::Zip => {
                // Stage files in a temporary directory
                let staging = TempDir::new().context("create staging dir")?;
                let stage_root = staging.path();

                // Copy binary into staging root
                let stage_bin = stage_root.join(&dest_bin_name);
                fs::copy(&dest_bin, &stage_bin).with_context(|| {
                    format!(
                        "stage bin {} -> {}",
                        dest_bin.display(),
                        stage_bin.display()
                    )
                })?;

                // Copy user-provided resources into staging (files or directories)
                for r in &args.bundle_resources {
                    let src = if r.is_absolute() {
                        r.clone()
                    } else {
                        ws_root.join(r)
                    };
                    if src.is_file() {
                        let target = stage_root.join(src.file_name().unwrap());
                        if let Some(parent) = target.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::copy(&src, &target).with_context(|| {
                            format!("stage resource {} -> {}", src.display(), target.display())
                        })?;
                    } else if src.is_dir() {
                        let base = src
                            .file_name()
                            .map(|s| s.to_owned())
                            .unwrap_or_else(|| OsStr::new("res").to_owned());
                        for entry in WalkDir::new(&src)
                            .into_iter()
                            .filter_map(Result::ok)
                            .filter(|e| e.file_type().is_file())
                        {
                            let rel = entry.path().strip_prefix(&src).unwrap();
                            let target = stage_root.join(&base).join(rel);
                            if let Some(parent) = target.parent() {
                                fs::create_dir_all(parent)?;
                            }
                            fs::copy(entry.path(), &target).with_context(|| {
                                format!(
                                    "stage resource {} -> {}",
                                    entry.path().display(),
                                    target.display()
                                )
                            })?;
                        }
                    }
                }

                // Build metadata.json with file checksums (SHA-256) for staged contents
                let mut checksums = serde_json::Map::new();
                for entry in WalkDir::new(&stage_root)
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_file())
                {
                    let rel = entry
                        .path()
                        .strip_prefix(&stage_root)
                        .unwrap()
                        .to_string_lossy()
                        .to_string();
                    let mut f = fs::File::open(entry.path())?;
                    use std::io::Read;
                    let mut hasher = Sha256::new();
                    let _ = std::io::copy(&mut f, &mut hasher)?;
                    let digest = hasher.finalize();
                    checksums.insert(rel, serde_json::Value::String(hex::encode(digest)));
                }
                let metadata_json = serde_json::json!({
                    "servo_commit": meta.servo_commit,
                    "build_profile": meta.build_profile,
                    "enabled_features": meta.enabled_features,
                    "timestamp": meta.timestamp,
                    "target_triple": meta.target_triple,
                    "rust_toolchain": meta.rust_toolchain,
                    "binary_name": meta.binary_name,
                    "checksums": checksums,
                });
                fs::write(
                    stage_root.join("metadata.json"),
                    serde_json::to_vec_pretty(&metadata_json)?,
                )?;

                // Create zip archive from staged contents
                let zip_name = format!(
                    "{}-{}-{}-{}.zip",
                    "servo",
                    target_triple,
                    profile.as_dir(),
                    meta.servo_commit
                );
                let zip_path = out_dir.join(zip_name);
                let zip_file = fs::File::create(&zip_path)
                    .with_context(|| format!("create zip {}", zip_path.display()))?;
                let mut zip = zip::ZipWriter::new(zip_file);
                let options =
                    FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
                for entry in WalkDir::new(&stage_root)
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_file())
                {
                    let rel = entry
                        .path()
                        .strip_prefix(&stage_root)
                        .unwrap()
                        .to_string_lossy()
                        .replace("\\", "/");
                    zip.start_file(rel, options)?;
                    let mut f = fs::File::open(entry.path())?;
                    use std::io::Read;
                    use std::io::Write;
                    let mut buf = Vec::new();
                    f.read_to_end(&mut buf)?;
                    zip.write_all(&buf)?;
                }
                let _ = zip.finish()?;
                if args.verbose {
                    eprintln!("Packaged zip: {}", zip_path.display());
                }
            }
            PackageKind::AppImage => {
                // Build AppDir and produce an AppImage with `appimagetool`.
                let staging = TempDir::new().context("create staging dir")?;
                let appdir = staging.path().join("AppDir");

                fs::create_dir_all(appdir.join("usr/bin"))
                    .with_context(|| format!("create dir {}", appdir.join("usr/bin").display()))?;
                fs::create_dir_all(appdir.join("usr/share/applications"))?;
                fs::create_dir_all(appdir.join("usr/share/icons/hicolor/256x256/apps"))?;
                fs::create_dir_all(appdir.join("usr/share/doc/verso"))?;

                // Copy binary into AppDir/usr/bin/verso
                let app_bin_name = if cfg!(windows) { "verso.exe" } else { "verso" };
                let app_bin = appdir.join("usr/bin").join(app_bin_name);
                fs::copy(&dest_bin, &app_bin).with_context(|| {
                    format!("copy {} -> {}", dest_bin.display(), app_bin.display())
                })?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perm = fs::metadata(&app_bin)?.permissions();
                    perm.set_mode(0o755);
                    fs::set_permissions(&app_bin, perm)?;
                }

                // Copy user-provided resources into AppDir/usr/bin
                for r in &args.bundle_resources {
                    let src = if r.is_absolute() {
                        r.clone()
                    } else {
                        ws_root.join(r)
                    };
                    if src.is_file() {
                        let target = appdir.join("usr/bin").join(src.file_name().unwrap());
                        if let Some(parent) = target.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::copy(&src, &target).with_context(|| {
                            format!("stage resource {} -> {}", src.display(), target.display())
                        })?;
                    } else if src.is_dir() {
                        let base = src.file_name().unwrap().to_owned();
                        for entry in WalkDir::new(&src)
                            .into_iter()
                            .filter_map(Result::ok)
                            .filter(|e| e.file_type().is_file())
                        {
                            let rel = entry.path().strip_prefix(&src).unwrap();
                            let target = appdir.join("usr/bin").join(&base).join(rel);
                            if let Some(parent) = target.parent() {
                                fs::create_dir_all(parent)?;
                            }
                            fs::copy(entry.path(), &target).with_context(|| {
                                format!(
                                    "stage resource {} -> {}",
                                    entry.path().display(),
                                    target.display()
                                )
                            })?;
                        }
                    }
                }

                // Write metadata.json with checksums for staged AppDir
                let mut checksums = serde_json::Map::new();
                for entry in WalkDir::new(&appdir)
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_file())
                {
                    let rel = entry
                        .path()
                        .strip_prefix(&appdir)
                        .unwrap()
                        .to_string_lossy()
                        .to_string();
                    let mut f = fs::File::open(entry.path())?;
                    use std::io::Read;
                    let mut hasher = Sha256::new();
                    let _ = std::io::copy(&mut f, &mut hasher)?;
                    let digest = hasher.finalize();
                    checksums.insert(rel, serde_json::Value::String(hex::encode(digest)));
                }
                let metadata_json = serde_json::json!({
                    "servo_commit": meta.servo_commit,
                    "build_profile": meta.build_profile,
                    "enabled_features": meta.enabled_features,
                    "timestamp": meta.timestamp,
                    "target_triple": meta.target_triple,
                    "rust_toolchain": meta.rust_toolchain,
                    "binary_name": meta.binary_name,
                    "package": "AppImage",
                    "checksums": checksums,
                });
                fs::write(
                    appdir.join("usr/share/doc/verso/metadata.json"),
                    serde_json::to_vec_pretty(&metadata_json)?,
                )?;

                // Desktop file and AppRun script
                fs::write(
                    appdir.join("verso.desktop"),
                    "[Desktop Entry]\nType=Application\nName=Verso\nExec=verso\nIcon=verso\nCategories=Utility;\n",
                )?;
                let apprun = appdir.join("AppRun");
                fs::write(
                    &apprun,
                    "#!/usr/bin/env bash\nset -euo pipefail\nHERE=\"$(cd \"$(dirname \"$0\")\" && pwd)\"\nexec \"$HERE/usr/bin/verso\" \"$@\"\n",
                )?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perm = fs::metadata(&apprun)?.permissions();
                    perm.set_mode(0o755);
                    fs::set_permissions(&apprun, perm)?;
                }

                // Produce AppImage
                let appimagetool = which("appimagetool")
                    .context("`appimagetool` not found in PATH. Install AppImageKit appimagetool to produce an AppImage.")?;
                let appimage_out = out_dir.join(format!("Verso-{}.AppImage", target_triple));
                let status = Command::new(appimagetool)
                    .arg(&appdir)
                    .arg(&appimage_out)
                    .status()
                    .with_context(|| "failed to spawn appimagetool")?;
                if !status.success() {
                    return Err(anyhow!(
                        "appimagetool failed with status {}",
                        status.code().unwrap_or(-1)
                    ));
                }
                if args.verbose {
                    eprintln!("Packaged AppImage: {}", appimage_out.display());
                }
            }
            PackageKind::Dmg => {
                // Stage a .app bundle and create a DMG with hdiutil (macOS only).
                if cfg!(target_os = "macos") {
                    let staging = TempDir::new().context("create staging dir")?;
                    let app_bundle = staging.path().join("Verso.app");
                    fs::create_dir_all(app_bundle.join("Contents/MacOS"))?;
                    fs::create_dir_all(app_bundle.join("Contents/Resources"))?;

                    // Copy binary
                    let mac_bin = app_bundle.join("Contents/MacOS/verso");
                    fs::copy(&dest_bin, &mac_bin).with_context(|| {
                        format!("copy {} -> {}", dest_bin.display(), mac_bin.display())
                    })?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let mut perm = fs::metadata(&mac_bin)?.permissions();
                        perm.set_mode(0o755);
                        fs::set_permissions(&mac_bin, perm)?;
                    }

                    // Copy resources into Contents/Resources
                    for r in &args.bundle_resources {
                        let src = if r.is_absolute() {
                            r.clone()
                        } else {
                            ws_root.join(r)
                        };
                        if src.is_file() {
                            let target = app_bundle
                                .join("Contents/Resources")
                                .join(src.file_name().unwrap());
                            if let Some(parent) = target.parent() {
                                fs::create_dir_all(parent)?;
                            }
                            fs::copy(&src, &target)?;
                        } else if src.is_dir() {
                            let base = src.file_name().unwrap().to_owned();
                            for entry in WalkDir::new(&src)
                                .into_iter()
                                .filter_map(Result::ok)
                                .filter(|e| e.file_type().is_file())
                            {
                                let rel = entry.path().strip_prefix(&src).unwrap();
                                let target =
                                    app_bundle.join("Contents/Resources").join(&base).join(rel);
                                if let Some(parent) = target.parent() {
                                    fs::create_dir_all(parent)?;
                                }
                                fs::copy(entry.path(), &target)?;
                            }
                        }
                    }

                    // Minimal Info.plist
                    let info_plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Verso</string>
  <key>CFBundleIdentifier</key><string>org.example.verso</string>
  <key>CFBundleExecutable</key><string>verso</string>
  <key>CFBundlePackageType</</key><string>APPL</string>
  <key>CFBundleVersion</key><string>1.0.0</string>
  <key>CFBundleShortVersionString</key><string>1.0.0</string>
</dict>
</plist>
"#;
                    fs::write(app_bundle.join("Contents/Info.plist"), info_plist)?;

                    // metadata.json in Resources
                    let mut checksums = serde_json::Map::new();
                    for entry in WalkDir::new(app_bundle.join("Contents"))
                        .into_iter()
                        .filter_map(Result::ok)
                        .filter(|e| e.file_type().is_file())
                    {
                        let rel = entry
                            .path()
                            .strip_prefix(app_bundle.join("Contents"))
                            .unwrap()
                            .to_string_lossy()
                            .to_string();
                        let mut f = fs::File::open(entry.path())?;
                        use std::io::Read;
                        let mut hasher = Sha256::new();
                        let _ = std::io::copy(&mut f, &mut hasher)?;
                        let digest = hasher.finalize();
                        checksums.insert(rel, serde_json::Value::String(hex::encode(digest)));
                    }
                    let metadata_json = serde_json::json!({
                        "servo_commit": meta.servo_commit,
                        "build_profile": meta.build_profile,
                        "enabled_features": meta.enabled_features,
                        "timestamp": meta.timestamp,
                        "target_triple": meta.target_triple,
                        "rust_toolchain": meta.rust_toolchain,
                        "binary_name": meta.binary_name,
                        "package": "DMG",
                        "checksums": checksums,
                    });
                    fs::write(
                        app_bundle.join("Contents/Resources/metadata.json"),
                        serde_json::to_vec_pretty(&metadata_json)?,
                    )?;

                    // Create DMG with hdiutil
                    let hdiutil = which("hdiutil")
                        .context("`hdiutil` not found in PATH; DMG packaging requires macOS.")?;
                    let dmg_out = out_dir.join("Verso.dmg");
                    let status = Command::new(hdiutil)
                        .arg("create")
                        .arg("-volname")
                        .arg("Verso")
                        .arg("-srcfolder")
                        .arg(&app_bundle)
                        .arg("-ov")
                        .arg("-format")
                        .arg("UDZO")
                        .arg(&dmg_out)
                        .status()
                        .with_context(|| "failed to spawn hdiutil")?;
                    if !status.success() {
                        return Err(anyhow!(
                            "hdiutil failed with status {}",
                            status.code().unwrap_or(-1)
                        ));
                    }
                    if args.verbose {
                        eprintln!("Packaged DMG: {}", dmg_out.display());
                    }
                } else {
                    return Err(anyhow!("DMG packaging requires macOS (hdiutil)."));
                }
            }
            PackageKind::Msi => {
                // Build an MSI with WiX (Windows only).
                if cfg!(target_os = "windows") {
                    let candle = which("candle.exe")
                        .context("`candle.exe` not found in PATH; install WiX Toolset.")?;
                    let light = which("light.exe")
                        .context("`light.exe` not found in PATH; install WiX Toolset.")?;
                    let staging = TempDir::new().context("create staging dir")?;
                    let wix_dir = staging.path().join("wix");
                    fs::create_dir_all(&wix_dir)?;
                    let wix_src = wix_dir.join("verso.wxs");

                    // Minimal WiX source using the staged binary
                    let wix_xml = format!(
"<?xml version='1.0' encoding='UTF-8'?>
<Wix xmlns='http://schemas.microsoft.com/wix/2006/wi'>
  <Product Id='*' Name='Verso' Language='1033' Version='1.0.0.0' Manufacturer='Verso' UpgradeCode='PUT-GUID-HERE'>
    <Package InstallerVersion='500' Compressed='yes' InstallScope='perMachine' />
    <MediaTemplate />
    <Directory Id='TARGETDIR' Name='SourceDir'>
      <Directory Id='ProgramFilesFolder'>
        <Directory Id='INSTALLFOLDER' Name='Verso' />
      </Directory>
    </Directory>
    <DirectoryRef Id='INSTALLFOLDER'>
      <Component Id='MainExe' Guid='PUT-GUID-HERE'>
        <File Id='VersoExe' Source='{exe}' KeyPath='yes' Checksum='yes' />
      </Component>
    </DirectoryRef>
    <Feature Id='DefaultFeature' Level='1'>
      <ComponentRef Id='MainExe' />
    </Feature>
  </Product>
</Wix>
", exe = dest_bin.display());
                    fs::write(&wix_src, wix_xml)?;

                    // Compile with candle
                    let wixobj = wix_dir.join("verso.wixobj");
                    let status = Command::new(candle)
                        .arg(&wix_src)
                        .arg("-o")
                        .arg(&wixobj)
                        .status()
                        .with_context(|| "failed to spawn candle.exe")?;
                    if !status.success() {
                        return Err(anyhow!(
                            "candle.exe failed with status {}",
                            status.code().unwrap_or(-1)
                        ));
                    }

                    // Link with light
                    let msi_out = out_dir.join("Verso.msi");
                    let status = Command::new(light)
                        .arg(&wixobj)
                        .arg("-o")
                        .arg(&msi_out)
                        .status()
                        .with_context(|| "failed to spawn light.exe")?;
                    if !status.success() {
                        return Err(anyhow!(
                            "light.exe failed with status {}",
                            status.code().unwrap_or(-1)
                        ));
                    }
                    if args.verbose {
                        eprintln!("Packaged MSI: {}", msi_out.display());
                    }
                } else {
                    return Err(anyhow!(
                        "MSI packaging requires Windows with WiX Toolset installed."
                    ));
                }
            }
        }
    }

    println!("{}", dest_bin.display());
    Ok(())
}

fn determine_servo_src(args: &Args, cfg: &FileConfig) -> Result<PathBuf> {
    if let Some(p) = &args.servo_src {
        return Ok(p.clone());
    }
    if let Ok(envp) = std::env::var("SERVO_SRC") {
        let p = PathBuf::from(envp);
        if p.exists() {
            return Ok(p);
        }
    }
    if let Some(p) = &cfg.servo_src {
        if p.exists() {
            return Ok(p.clone());
        }
    }
    Err(anyhow!(
        "Servo source path not provided. Use --servo-src, set SERVO_SRC, or configure `servo_src` in servo-build-config.toml"
    ))
}

fn determine_features(args: &Args, cfg: &FileConfig) -> Vec<String> {
    if let Some(s) = args.features.as_deref() {
        return split_features(s);
    }
    if let Some(v) = &cfg.features {
        return v.clone();
    }
    vec![]
}

fn split_features(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
        .collect()
}

fn build_servo(
    servo_src: &Path,
    binary_name: &str,
    profile: &BuildProfile,
    features: &[String],
    target: Option<&str>,
    toolchain: Option<&str>,
    verbose: bool,
) -> Result<()> {
    let cargo = which("cargo").context("could not find `cargo` in PATH")?;

    // Build command
    let mut cmd = Command::new(&cargo);
    cmd.current_dir(servo_src);
    if let Some(tc) = toolchain {
        cmd.arg(format!("+{}", tc));
    }
    cmd.arg("build");
    cmd.arg("--bin").arg(binary_name);
    if let BuildProfile::Release = profile {
        cmd.arg("--release");
    }
    if !features.is_empty() {
        cmd.arg("--features").arg(features.join(","));
    }
    if let Some(triple) = target {
        cmd.arg("--target").arg(triple);
    }
    if verbose {
        eprintln!("Running: {:?}", cmd);
    }
    let status = cmd
        .status()
        .with_context(|| "failed to spawn cargo build")?;
    if !status.success() {
        return Err(anyhow!(
            "cargo build failed with status {}",
            status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

fn locate_servo_artifact(
    servo_src: &Path,
    bin_name: &str,
    profile: &BuildProfile,
    target: Option<&str>,
) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    let prof_dir = profile.as_dir();

    if let Some(t) = target {
        candidates.push(
            servo_src
                .join("target")
                .join(t)
                .join(prof_dir)
                .join(bin_name),
        );
    }
    // Generic host directory layout
    candidates.push(servo_src.join("target").join(prof_dir).join(bin_name));

    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }

    Err(anyhow!(
        "Could not find built binary. Tried: {}",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn read_git_commit_short(repo: &Path) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("rev-parse")
        .arg("--short=12")
        .arg("HEAD")
        .output()
        .context("git rev-parse failed")?;
    if !out.status.success() {
        return Err(anyhow!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn detect_host_triple() -> Result<String> {
    let out = Command::new("rustc")
        .arg("-vV")
        .output()
        .context("failed to execute rustc -vV")?;
    if !out.status.success() {
        return Err(anyhow!(
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
    Err(anyhow!("could not parse host triple from rustc -vV"))
}

fn detect_rustc_version() -> Result<String> {
    let out = Command::new("rustc")
        .arg("-V")
        .output()
        .context("failed to execute rustc -V")?;
    if !out.status.success() {
        return Err(anyhow!(
            "rustc -V failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn update_current_pointer(slot_dir: &Path, commit: &str, verbose: bool) -> Result<()> {
    let current = slot_dir.join("current");
    let target = slot_dir.join(commit);

    // Best-effort: update a symlink. Fall back to latest.json when not possible.
    match update_symlink(&current, &target) {
        Ok(_) => {
            if verbose {
                eprintln!(
                    "Updated symlink: {} -> {}",
                    current.display(),
                    target.display()
                );
            }
        }
        Err(e) => {
            if verbose {
                eprintln!(
                    "Symlink update failed ({}). Writing latest.json manifest instead.",
                    e
                );
            }
            let latest_path = slot_dir.join("latest.json");
            let manifest = serde_json::json!({
                "current": commit,
                "updated_at": OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_else(|_| "unknown".into()),
            });
            fs::write(&latest_path, serde_json::to_vec_pretty(&manifest)?)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn update_symlink(link: &Path, target: &Path) -> io::Result<()> {
    use std::fs;
    use std::os::unix::fs as unix_fs;

    if link.exists() || link.is_symlink() {
        let _ = fs::remove_file(link);
    }
    unix_fs::symlink(target.file_name().unwrap_or_else(|| OsStr::new("")), link)
}

#[cfg(windows)]
fn update_symlink(link: &Path, target: &Path) -> io::Result<()> {
    // On Windows, creating junctions/symlinks may require privileges. Try file symlink first.
    // If it fails, return error to fall back to manifest.
    std::os::windows::fs::symlink_file(target.file_name().unwrap_or_else(|| OsStr::new("")), link)
}

fn bin_name_for_host(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn read_config(path: &Path) -> Result<FileConfig> {
    if !path.exists() {
        return Err(anyhow!("config file not found: {}", path.display()));
    }
    let s = fs::read_to_string(path)?;
    let cfg: FileConfig = toml::from_str(&s)?;
    Ok(cfg)
}

fn find_workspace_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("cannot read current directory")?;
    for ancestor in cwd.ancestors() {
        let cand = ancestor.join("Cargo.toml");
        if cand.exists() {
            // Check if it contains [workspace]
            let content = fs::read_to_string(&cand)?;
            if content.contains("[workspace]") {
                return Ok(ancestor.to_path_buf());
            }
        }
    }
    Err(anyhow!(
        "Could not find a Cargo workspace root by walking up from {}",
        cwd.display()
    ))
}

// Removed CloneOrFrom helper: precedence is resolved directly where needed.
