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
}
