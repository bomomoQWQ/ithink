//! The provider is a protocol process; without a bootstrap it must fail fast
//! instead of starting an incomplete handshake. This also keeps the binary in
//! the workspace test build so host-level tests can spawn it as a sibling.

use std::process::{Command, Stdio};

#[test]
fn missing_bootstrap_fails_cleanly() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ithink-model-provider"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("provider binary starts");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("provider exits");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "stdout is protocol-only");
    assert!(String::from_utf8_lossy(&output.stderr).contains("config_error"));
}
