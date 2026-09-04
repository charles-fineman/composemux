//! Plain streaming for when stdout isn't a terminal.
//!
//! The wrapping CLI may run in CI or with output piped, where a full-screen UI
//! is useless (and would emit escape sequences into a log file). In that case we
//! behave like `docker compose logs -f`: one prefixed line per log line.

use crate::docker::{DockerClient, LogSupervisor, SourceEvent};
use anyhow::Result;
use std::collections::HashMap;
use std::io::Write;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Longest unterminated run held before it is emitted anyway.
///
/// Output that never sends a newline -- a progress bar driving itself with
/// carriage returns, a stuck process spraying binary -- would otherwise buffer
/// for the life of the process. This path is the unattended one, used under CI
/// and wrapper scripts, so growing without bound here fails where nobody is
/// watching.
const MAX_PARTIAL: usize = 1024 * 1024;

/// Buffers partial lines so a chunk boundary never splits output mid-line.
#[derive(Default)]
struct LineAssembler {
    partial: Vec<u8>,
}

impl LineAssembler {
    /// Returns the complete lines contained in `bytes`, holding back any tail.
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.partial.extend_from_slice(bytes);
        let mut lines = Vec::new();
        while let Some(pos) = self.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line);
            lines.push(text.trim_end_matches(['\n', '\r']).to_string());
        }

        // Nothing terminated the run and it has grown past what is reasonable
        // to hold. Emit it rather than keep buffering: a long line printed early
        // is a far better failure than memory climbing until the process dies.
        if self.partial.len() > MAX_PARTIAL {
            let held = std::mem::take(&mut self.partial);
            lines.push(String::from_utf8_lossy(&held).into_owned());
        }
        lines
    }
}

/// Pads service names so the prefixes line up, the way `docker compose logs`
/// does. The width grows as services appear, since they are not all known up
/// front; earlier lines keep the width they were written at.
#[derive(Default)]
struct Prefixes {
    width: usize,
}

impl Prefixes {
    fn format(&mut self, service: &str) -> String {
        self.width = self.width.max(service.chars().count());
        format!("{service:<width$}  | ", width = self.width)
    }
}

pub async fn run(
    client: &DockerClient,
    project: &str,
    tail: usize,
    cancel: CancellationToken,
) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<SourceEvent>(4096);
    let supervisor = LogSupervisor::new(client, project, tail, tx);
    let supervisor_cancel = cancel.clone();
    tokio::spawn(async move { supervisor.run(supervisor_cancel).await });

    let mut assemblers: HashMap<String, LineAssembler> = HashMap::new();
    let mut prefixes = Prefixes::default();
    let stdout = std::io::stdout();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            message = rx.recv() => {
                let Some(message) = message else { break };
                if let SourceEvent::Output { service, bytes, .. } = message {
                    let prefix = prefixes.format(&service);
                    let assembler = assemblers.entry(service).or_default();
                    let mut lock = stdout.lock();
                    for line in assembler.push(&bytes) {
                        writeln!(lock, "{prefix}{line}")?;
                    }
                    lock.flush()?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_lines_are_emitted_immediately() {
        let mut a = LineAssembler::default();
        assert_eq!(a.push(b"one\ntwo\n"), vec!["one", "two"]);
    }

    #[test]
    fn a_partial_line_is_held_until_its_newline_arrives() {
        let mut a = LineAssembler::default();
        assert!(a.push(b"half").is_empty(), "no newline yet");
        assert_eq!(a.push(b"-line\n"), vec!["half-line"]);
    }

    #[test]
    fn carriage_returns_are_trimmed() {
        let mut a = LineAssembler::default();
        assert_eq!(a.push(b"windows\r\n"), vec!["windows"]);
    }

    #[test]
    fn invalid_utf8_does_not_panic() {
        let mut a = LineAssembler::default();
        let lines = a.push(&[0xff, 0xfe, b'\n']);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn an_unterminated_run_is_emitted_rather_than_buffered_forever() {
        // A progress bar driving itself with carriage returns never sends a
        // newline, so the buffer would grow for the life of the process.
        let mut a = LineAssembler::default();
        let chunk = vec![b'x'; 64 * 1024];
        let mut emitted = 0;
        for _ in 0..40 {
            emitted += a.push(&chunk).len();
        }
        assert!(emitted > 0, "the held run should have been flushed");
        assert!(
            a.partial.len() <= MAX_PARTIAL,
            "buffer grew to {} bytes",
            a.partial.len()
        );
    }

    #[test]
    fn prefixes_line_up_as_services_appear() {
        let mut p = Prefixes::default();
        assert_eq!(p.format("api"), "api  | ");
        // A longer name widens the column for everything after it.
        assert_eq!(p.format("cleanexit"), "cleanexit  | ");
        assert_eq!(p.format("api"), "api        | ");
        assert_eq!(p.format("db"), "db         | ");
    }

    #[test]
    fn a_chunk_split_mid_line_reassembles_correctly() {
        let mut a = LineAssembler::default();
        a.push(b"start");
        a.push(b"-middle");
        assert_eq!(a.push(b"-end\nnext\n"), vec!["start-middle-end", "next"]);
    }
}
