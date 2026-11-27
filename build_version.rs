//! Shared build script helper; rebuilding never increments the checked-in counter.
pub fn emit() {
    let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"))
        .join("../..");
    let file = root.join("BUILD_NUMBER");
    println!("cargo:rerun-if-changed={}", file.display());
    println!("cargo:rerun-if-changed={}", root.join("build_version.rs").display());
    let text = std::fs::read_to_string(&file).expect("read BUILD_NUMBER");
    let number: u64 = text.trim().parse().expect("BUILD_NUMBER must be a positive integer");
    assert!(number > 0 && text.trim() == number.to_string(), "BUILD_NUMBER must be a canonical positive integer");
    println!("cargo:rustc-env=VICTORY_BUILD_NUMBER={number}");
}
