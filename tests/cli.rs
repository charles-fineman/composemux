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
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;

    /// A stand-in Docker daemon that accepts a connection and then says
    /// nothing.
    ///
    /// It gives the test a real readiness handshake: the accept returns only
    /// once the child has reached its first call to the daemon, which is the
    /// moment after the signal handlers are installed. Never answering keeps
    /// the child parked there until the signal arrives, so the exit status
    /// under test can only have come from the signal path.
    struct StalledDaemon {
        listener: TcpListener,
        port: u16,
    }

    impl StalledDaemon {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
            let port = listener.local_addr().unwrap().port();
            Self { listener, port }
        }

        fn spawn_client(&self) -> Child {
            Command::new(env!("CARGO_BIN_EXE_composemux"))
                .args(["--project", "any-project", "--no-tui"])
                .env("DOCKER_HOST", format!("tcp://127.0.0.1:{}", self.port))
                // Inherited TLS settings would send it somewhere else.
                .env_remove("DOCKER_TLS_VERIFY")
                .env_remove("DOCKER_CERT_PATH")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("the binary should run")
        }

        /// Blocks until the child is waiting on the daemon.
        fn await_client(&self) -> TcpStream {
            let (stream, _) = self.listener.accept().expect("the child should connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = [0u8; 1];
            // It has sent its first request and is now waiting on a reply that
            // will never come.
            let _ = (&stream).read(&mut request);
            stream
        }
    }

    /// A terminating signal should produce the status a supervisor expects,
    /// `128 + signo`, so it can tell its own shutdown from a user quitting.
    ///
    /// Returns the raw status rather than a code, because the difference that
    /// matters here is invisible in the number: killing the process outright
    /// yields `128 + signo` as well, since that is what a shell reports for a
    /// signalled child. Only a *normal* exit carrying that code proves our own
    /// handler ran and got the chance to put the terminal back.
    fn exit_status_for(signal: &str) -> std::process::ExitStatus {
        let daemon = StalledDaemon::start();
        let mut child = daemon.spawn_client();
        // Held open for the rest of the test: dropping it would answer the
        // child with a closed connection and let it exit on its own.
        let _connection = daemon.await_client();

        let killed = Command::new("kill")
            .args([&format!("-{signal}"), &child.id().to_string()])
            .status()
            .expect("kill should run");
        assert!(killed.success(), "could not signal the child");

        child.wait().expect("the child should be reapable")
    }

    #[test]
    fn terminating_signals_use_the_conventional_status() {
        use std::os::unix::process::ExitStatusExt;

        for (signal, expected) in [("TERM", 143), ("HUP", 129), ("INT", 130)] {
            let status = exit_status_for(signal);
            assert_eq!(
                status.signal(),
                None,
                "SIG{signal} killed the process outright; the handler never ran, \
                 so the terminal was left as it was"
            );
            assert_eq!(
                status.code(),
                Some(expected),
                "SIG{signal} should exit {expected}"
            );
        }
    }
}
