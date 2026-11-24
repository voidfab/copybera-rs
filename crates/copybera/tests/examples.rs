//! `copybera validate` accepts every workflow under `examples/`.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn validate_accepts_every_example_workflow() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    for (file, name, mode) in [
        ("publish-subtree.toml", "publish-subtree", "squash"),
        ("mirror-iterative.toml", "mirror-docs", "iterative"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_copybera"))
            .args(["validate", dir.join(file).to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{file}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains(&format!("ok: {name} ({mode})")), "{stdout}");
    }
}
