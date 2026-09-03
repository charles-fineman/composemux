//! Binary-level tests.
//!
//! These cover the contract the wrapping CLI depends on: exit status, and the
//! guarantee that a startup failure never leaves escape sequences (or a
//! raw-mode terminal) behind, because it happens before the UI starts.

use std::process::{Command, Stdio};

/// A port nothing listens on, so the daemon connection is refused immediately.
const UNREACHABLE_DOCKER: &str = "tcp://127.0.0.1:1";

fn bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_composemux"));
    cmd.stdin(Stdio::null());
    cmd
}

#[test]
fn an_unreachable_daemon_fails_with_a_useful_message() {
    let output = bin()
        .env("DOCKER_HOST", UNREACHABLE_DOCKER)
        .args(["--project", "anything"])
        .output()
        .expect("the binary should run");

    assert!(
        !output.status.success(),
        "expected a non-zero exit, got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Docker daemon"),
        "stderr should name the cause, got: {stderr}"
    );
}

#[test]
fn a_startup_failure_writes_no_escape_sequences() {
    // The terminal is only put into raw mode after the daemon connection
    // succeeds. If that ordering ever changes, a failing run would corrupt the
    // calling script's terminal.
    let output = bin()
        .env("DOCKER_HOST", UNREACHABLE_DOCKER)
        .args(["--project", "anything"])
        .output()
        .expect("the binary should run");

    assert!(
        !output.stdout.contains(&0x1b),
        "stdout contained an escape sequence"
    );
    assert!(
        !output.stderr.contains(&0x1b),
        "stderr contained an escape sequence"
    );
}

#[test]
fn help_and_version_succeed_without_touching_docker() {
    for flag in ["--help", "--version"] {
        let output = bin()
            .env("DOCKER_HOST", UNREACHABLE_DOCKER)
            .arg(flag)
            .output()
            .expect("the binary should run");
        assert!(output.status.success(), "{flag} should exit zero");
        assert!(!output.stdout.is_empty(), "{flag} should print something");
    }
}

#[test]
fn an_unknown_flag_is_rejected() {
    let output = bin()
        .arg("--definitely-not-a-flag")
        .output()
        .expect("the binary should run");
    assert!(!output.status.success());
}

#[test]
fn a_malformed_config_file_is_reported_rather_than_ignored() {
    let dir = std::env::temp_dir().join(format!("composemux-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bad.yaml");
    // `pined` is a typo for `pinned`; unknown keys must be a loud error.
    std::fs::write(&path, "pined: [api]\n").unwrap();

    let output = bin()
        .env("DOCKER_HOST", UNREACHABLE_DOCKER)
        .args(["--config", path.to_str().unwrap(), "--project", "anything"])
        .output()
        .expect("the binary should run");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("pined") || stderr.contains("unknown field"),
        "stderr should name the bad key, got: {stderr}"
    );

    std::fs::remove_dir_all(&dir).ok();
}
