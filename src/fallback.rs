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
    ///
    /// The cap is applied while the input is consumed, not after it has been
    /// appended. Checking afterwards bounded what was *kept* rather than what
    /// was allocated: a single unterminated chunk larger than the cap went
    /// into the buffer whole before anything looked at its size.
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        let mut rest = bytes;

        while let Some(pos) = rest.iter().position(|b| *b == b'\n') {
            let (line, tail) = rest.split_at(pos + 1);
            self.partial.extend_from_slice(line);
            let held = std::mem::take(&mut self.partial);
            let text = String::from_utf8_lossy(&held);
            lines.push(text.trim_end_matches(['\n', '\r']).to_string());
            rest = tail;
        }

        // What is left has no newline in it. Hold it back, but take it in
        // cap-sized pieces rather than letting the run grow: a long line
        // printed early is a far better failure than memory climbing until the
        // process dies. A cut can land mid-character, which `from_utf8_lossy`
        // renders as a replacement -- acceptable for output that has already
        // run a megabyte without a newline. A line that *does* terminate is
        // still emitted whole however long it is, since splitting real lines
        // would corrupt them for whatever is parsing the log.
        while self.partial.len() + rest.len() > MAX_PARTIAL {
            let room = MAX_PARTIAL - self.partial.len();
            self.partial.extend_from_slice(&rest[..room]);
            rest = &rest[room..];
            let held = std::mem::take(&mut self.partial);
            lines.push(String::from_utf8_lossy(&held).into_owned());
        }
        self.partial.extend_from_slice(rest);
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
    /// The prefix for one line, widening the column if this name is the
    /// longest seen so far.
    fn format(&mut self, service: &str) -> String {
        self.width = self.width.max(service.chars().count());
        format!("{service:<width$}  | ", width = self.width)
    }
}

/// Streams every service's logs to stdout, one prefixed line at a time, until
/// the stream ends or `cancel` fires.
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

    /// Output that never sends a newline must not buffer for the life of the
    /// process.
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

    /// Services are not all known up front, so the column widens as they
    /// arrive and earlier lines keep the width they were written at.
    #[test]
    fn prefixes_line_up_as_services_appear() {
        let mut p = Prefixes::default();
        assert_eq!(p.format("api"), "api  | ");
        // A longer name widens the column for everything after it.
        assert_eq!(p.format("cleanexit"), "cleanexit  | ");
        assert_eq!(p.format("api"), "api        | ");
        assert_eq!(p.format("db"), "db         | ");
    }

    /// One oversized chunk, rather than the many small ones above: the cap
    /// used to be checked only after the whole chunk had been appended, so it
    /// bounded what was retained and not the allocation itself.
    #[test]
    fn a_single_oversized_chunk_is_broken_up_rather_than_swallowed_whole() {
        let mut a = LineAssembler::default();
        let size = 4 * MAX_PARTIAL + 7;
        let lines = a.push(&vec![b'x'; size]);

        let longest = lines.iter().map(|l| l.len()).max().unwrap_or(0);
        assert!(
            longest <= MAX_PARTIAL,
            "emitted a {longest}-byte line, past the {MAX_PARTIAL}-byte cap"
        );
        assert!(a.partial.len() <= MAX_PARTIAL, "held {}", a.partial.len());
        // Bounding it must not lose any of it.
        let total: usize = lines.iter().map(|l| l.len()).sum::<usize>() + a.partial.len();
        assert_eq!(total, size, "bytes went missing");
    }

    #[test]
    fn a_chunk_split_mid_line_reassembles_correctly() {
        let mut a = LineAssembler::default();
        a.push(b"start");
        a.push(b"-middle");
        assert_eq!(a.push(b"-end\nnext\n"), vec!["start-middle-end", "next"]);
    }
}
