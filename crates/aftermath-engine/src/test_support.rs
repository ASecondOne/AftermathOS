//! Compile the native Rust fixture once per unit-test process, then link it
//! into each isolated fixture directory. No interpreted test programs.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub fn install(path: &Path) {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    let binary = BINARY.get_or_init(|| {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let output = manifest
            .join("../../target/test-support")
            .join(std::process::id().to_string());
        fs::create_dir_all(&output).unwrap();
        let binary = output.join("daemon-fixture");
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let status = Command::new(rustc)
            .arg("--edition=2021")
            .arg(manifest.join("tests/support/daemon_fixture.rs"))
            .arg("-o")
            .arg(&binary)
            .status()
            .unwrap();
        assert!(status.success(), "could not compile the Rust test fixture");
        fs::canonicalize(binary).unwrap()
    });
    std::os::unix::fs::symlink(binary, path).unwrap();
}
