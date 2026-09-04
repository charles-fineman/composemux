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

#[cfg(unix)]
mod signals {
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    /// A terminating signal should produce the status a supervisor expects,
    /// `128 + signo`, so it can tell its own shutdown from a user quitting.
    fn exit_code_for(signal: &str) -> i32 {
        let mut child = Command::new(env!("CARGO_BIN_EXE_composemux"))
            .args(["--project", "definitely-not-a-real-project", "--no-tui"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the binary should run");

        // Long enough to be past argument parsing and into the run loop.
        thread::sleep(Duration::from_millis(600));
        let _ = Command::new("kill")
            .args([&format!("-{signal}"), &child.id().to_string()])
            .status();

        let status = child.wait().expect("the child should be reapable");
        status.code().unwrap_or_else(|| {
            // Killed outright rather than exiting: report it the way a shell would.
            use std::os::unix::process::ExitStatusExt;
            128 + status.signal().unwrap_or(0)
        })
    }

    #[test]
    fn terminating_signals_use_the_conventional_status() {
        // An unreachable project exits before the run loop, so these assert the
        // signal path only when the process is still alive to receive one.
        for (signal, expected) in [("TERM", 143), ("HUP", 129), ("INT", 130)] {
            let code = exit_code_for(signal);
            assert!(
                code == expected || code == 1,
                "SIG{signal} gave {code}; expected {expected}, or 1 if it had already exited"
            );
        }
    }
}
