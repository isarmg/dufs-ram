use std::process::Command;

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
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
    println!("cargo:rustc-env=DUFS_BUILD_GIT_SHA={}", build_git_sha());
}

fn build_git_sha() -> String {
    if let Ok(value) = std::env::var("DUFS_BUILD_GIT_SHA") {
        let value = value.trim();
        if is_valid_git_sha(value) {
            return value.to_ascii_lowercase();
        }
        panic!("DUFS_BUILD_GIT_SHA must contain 7 to 64 hexadecimal characters");
    }

    Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| is_valid_git_sha(value))
        .unwrap_or_else(|| "unknown".to_string())
}

fn is_valid_git_sha(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
