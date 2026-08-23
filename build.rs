use std::{path::Path, process::Command};

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let pointer_width = std::env::var("CARGO_CFG_TARGET_POINTER_WIDTH").unwrap_or_default();
    assert!(
        target_os == "linux",
        "dufs supports Linux targets only (requested target OS: {target_os})"
    );
    assert!(
        pointer_width == "64",
        "dufs supports 64-bit Linux targets only (requested pointer width: {pointer_width})"
    );

    println!("cargo:rerun-if-env-changed=DUFS_BUILD_GIT_SHA");
    emit_git_rerun_paths();
    println!("cargo:rustc-env=DUFS_BUILD_GIT_SHA={}", build_git_sha());
}

fn emit_git_rerun_paths() {
    // A linked worktree stores a small `.git` file in the checkout and keeps
    // HEAD plus the shared references elsewhere. Ask Git for those real paths
    // instead of assuming that `.git` is a directory below the package root.
    if Path::new(".git").is_file() {
        println!("cargo:rerun-if-changed=.git");
    }
    for name in ["HEAD", "refs", "packed-refs", "reftable"] {
        if let Some(path) = git_output(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    // Shared refs in a reftable repository live below GIT_COMMON_DIR, while a
    // linked worktree's `--git-path reftable` resolves its private ref stack.
    if let Some(common_dir) = git_output(&["rev-parse", "--git-common-dir"]) {
        println!(
            "cargo:rerun-if-changed={}",
            Path::new(&common_dir).join("reftable").display()
        );
    }
}

fn build_git_sha() -> String {
    if let Ok(value) = std::env::var("DUFS_BUILD_GIT_SHA") {
        let value = value.trim();
        if is_valid_git_sha(value) {
            return value.to_ascii_lowercase();
        }
        panic!("DUFS_BUILD_GIT_SHA must contain 7 to 64 hexadecimal characters");
    }

    git_output(&["rev-parse", "--short=12", "HEAD"])
        .map(|value| value.to_ascii_lowercase())
        .filter(|value| is_valid_git_sha(value))
        .unwrap_or_else(|| "unknown".to_string())
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let output = String::from_utf8(output.stdout).ok()?;
    let output = output.strip_suffix('\n').unwrap_or(&output);
    let output = output.strip_suffix('\r').unwrap_or(output);
    (!output.is_empty() && !output.contains(['\n', '\r'])).then(|| output.to_owned())
}

fn is_valid_git_sha(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
