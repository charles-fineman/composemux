//! Plain streaming for when stdout isn't a terminal.
//!
//! The wrapping CLI may run in CI or with output piped, where a full-screen UI
//! is useless (and would emit escape sequences into a log file). In that case we
//! behave like `docker compose logs -f`: one prefixed line per log line.

use crate::docker::{DockerClient, LogSupervisor, SourceEvent};
use anyhow::Result;
use std::collections::HashMap;
use std::io::{self, Write};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthStr;

/// Longest unterminated run held before it is emitted anyway.
///
/// Output that never sends a newline -- a progress bar driving itself with
/// carriage returns, a stuck process spraying binary -- would otherwise buffer
/// for the life of the process. This path is the unattended one, used under CI
/// and wrapper scripts, so growing without bound here fails where nobody is
/// watching.
///
/// It bounds accumulation *across* chunks, not any single one. By the time
/// [`LineAssembler::push`] sees a chunk the daemon's frame has already been
/// allocated upstream, so clamping here would cost a copy without avoiding
/// the spike -- and a chunk larger than this before its first newline is
/// still emitted whole. See #31, which tracks bounding it at the read site.
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
    ///
    /// Measured and padded in terminal columns, not scalars. `{:<width$}`
    /// counts scalars, so a name of double-width glyphs would be padded to
    /// half the space it actually occupies -- which is the alignment this
    /// exists to provide.
    fn format(&mut self, service: &str) -> String {
        let width = service.width();
        self.width = self.width.max(width);
        format!("{service}{:pad$}  | ", "", pad = self.width - width)
    }
}

/// Writes one event's worth of output as whole prefixed lines.
///
/// Split out of [`run`] so the wiring between the assemblers, the prefixes and
/// the sink can be driven directly: `run` owns a channel and a supervisor and
/// needs a Docker daemon, while everything that can actually go wrong here --
/// output attributed to the wrong container, a partial line spliced onto
/// another's, a write that never reaches the reader -- is synchronous.
fn handle_output(
    assemblers: &mut HashMap<(String, u32), LineAssembler>,
    prefixes: &mut Prefixes,
    service: String,
    replica: u32,
    bytes: &[u8],
    out: &mut impl Write,
) -> io::Result<()> {
    // The label names the container -- `web-1` -- rather than the service.
    // That is what `docker compose logs` prints, and imitating it is this
    // module's stated job; printing a bare `web` was the divergence.
    //
    // It is applied to every service, including one that only ever has a
    // single container. Suffixing only scaled services, the way the TUI does,
    // is reachable from here -- `run` holds the client and the project name and
    // could list the containers -- but it would make the *shape* of a name
    // depend on a replica count that moves underneath the stream. Scale `web`
    // up and the container that had been printing `web` starts printing
    // `web-1`, so a reader grepping `^web  |` silently stops matching the very
    // container it was following, and the lines already written cannot be
    // relabelled. The TUI can afford count-dependent names because it redraws
    // the whole list from the current topology on every frame; a log stream
    // has no way to revise what it has already emitted.
    let label = format!("{service}-{replica}");
    let prefix = prefixes.format(&label);
    // Keyed by replica as well as name. Two containers of a scaled service are
    // two independent streams, and a shared assembler splices the tail one of
    // them is still holding onto the head of the other's next chunk, emitting
    // a line neither of them ever wrote.
    let assembler = assemblers.entry((service, replica)).or_default();
    for line in assembler.push(bytes) {
        writeln!(out, "{prefix}{line}")?;
    }
    // `std::io::Stdout` wraps a `LineWriter`, so on the real path the newlines
    // above have already reached the pipe. `out` is any `Write`, though, and
    // nothing here should depend on which one: flush unconditionally so the
    // guarantee belongs to this function rather than to its caller's choice.
    out.flush()
}

/// Streams every service's logs to stdout, one prefixed line at a time, until
/// the stream ends, `cancel` fires, or a write to stdout fails -- the last of
/// which is routine here rather than exotic, since this path exists for piped
/// output and the reader can go away first.
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

    let mut assemblers: HashMap<(String, u32), LineAssembler> = HashMap::new();
    let mut prefixes = Prefixes::default();
    let stdout = std::io::stdout();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            message = rx.recv() => {
                let Some(message) = message else { break };
                if let SourceEvent::Output { service, replica, bytes } = message {
                    let mut lock = stdout.lock();
                    handle_output(
                        &mut assemblers,
                        &mut prefixes,
                        service,
                        replica,
                        &bytes,
                        &mut lock,
                    )?;
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

    /// The cap must not corrupt what it splits. A uniform payload cannot show
    /// this -- every byte is interchangeable, so only a change in total length
    /// is visible -- so this one is non-uniform and reassembled byte for byte.
    #[test]
    fn capped_output_reassembles_to_exactly_what_went_in() {
        let mut a = LineAssembler::default();
        let size = 4 * MAX_PARTIAL + 7;
        // Printable ASCII, no newline: valid UTF-8, so `from_utf8_lossy` is a
        // no-op and the comparison is exact.
        let input: Vec<u8> = (0..size).map(|i| b'a' + (i % 26) as u8).collect();

        let lines = a.push(&input);

        let mut back: Vec<u8> = lines.iter().flat_map(|l| l.bytes()).collect();
        back.extend_from_slice(&a.partial);
        assert_eq!(back.len(), input.len(), "byte count changed");
        assert!(back == input, "the pieces do not reassemble to the input");
    }

    /// The other half of the rule: only *unterminated* runs are capped. A line
    /// that ends in a newline is emitted whole however long it runs, because
    /// splitting a real line would corrupt it for whatever parses the log.
    #[test]
    fn a_terminated_line_longer_than_the_cap_is_emitted_whole() {
        let mut a = LineAssembler::default();
        let size = 3 * MAX_PARTIAL;
        let mut input = vec![b'x'; size];
        input.push(b'\n');

        let lines = a.push(&input);

        assert_eq!(lines.len(), 1, "a terminated line was split");
        assert_eq!(lines[0].len(), size, "the line came back short");
        assert!(a.partial.is_empty(), "nothing should be held back");
    }

    /// The cap is `>`, so reaching it exactly holds rather than flushes. This
    /// pins the boundary in both directions, where off-by-one lives.
    #[test]
    fn the_cap_is_reached_before_it_is_exceeded() {
        let mut exact = LineAssembler::default();
        assert!(
            exact.push(&vec![b'x'; MAX_PARTIAL]).is_empty(),
            "reaching the cap exactly should not flush yet"
        );
        assert_eq!(exact.partial.len(), MAX_PARTIAL);

        let mut over = LineAssembler::default();
        let lines = over.push(&vec![b'x'; MAX_PARTIAL + 1]);
        assert!(lines.iter().all(|l| l.len() <= MAX_PARTIAL));
        let total: usize = lines.iter().map(|l| l.len()).sum::<usize>() + over.partial.len();
        assert_eq!(total, MAX_PARTIAL + 1, "bytes went missing at the boundary");
    }

    /// Crossing the cap across many calls, rather than inside one chunk: the
    /// other test drives the same limit but never leaves `push`.
    #[test]
    fn many_small_chunks_crossing_the_cap_lose_no_bytes() {
        let mut a = LineAssembler::default();
        let chunk = vec![b'x'; 64 * 1024];
        let mut emitted = 0usize;
        for _ in 0..40 {
            for line in a.push(&chunk) {
                // The chunk size divides the cap exactly, so `partial` lands on
                // MAX_PARTIAL and the next call enters the flush loop already
                // full -- a state no single-push test can reach.
                assert!(
                    line.len() <= MAX_PARTIAL,
                    "a cross-call flush emitted {} bytes, past the {MAX_PARTIAL}-byte cap",
                    line.len()
                );
                emitted += line.len();
            }
        }
        assert_eq!(
            emitted + a.partial.len(),
            40 * chunk.len(),
            "bytes went missing across calls"
        );
    }

    /// A chunk boundary can fall between the `\r` and the `\n`, which puts the
    /// two halves of the terminator in different calls -- and the rewrite
    /// moved the newline search from the buffer onto the incoming slice.
    #[test]
    fn a_crlf_split_across_chunks_is_still_trimmed() {
        let mut a = LineAssembler::default();
        assert!(a.push(b"windows\r").is_empty(), "no newline yet");
        assert_eq!(a.push(b"\nnext\n"), vec!["windows", "next"]);
    }

    /// Bytes flushed past the cap are a raw, unterminated run -- not a line --
    /// so a byte that merely looks like a terminator has to survive verbatim.
    ///
    /// The two loops in `push` are structurally alike and only one of them
    /// should trim: giving the cap flush the same `trim_end_matches` as the
    /// newline path silently drops a byte from output that was never a line,
    /// and every other test in this module stays green when it does.
    #[test]
    fn a_capped_flush_keeps_a_trailing_byte_that_looks_like_a_terminator() {
        let mut a = LineAssembler::default();
        let mut input = vec![b'x'; MAX_PARTIAL - 1];
        input.push(b'\r'); // lands exactly on the cap boundary
        input.extend(std::iter::repeat_n(b'x', 10));

        let lines = a.push(&input);

        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].len(),
            MAX_PARTIAL,
            "the trailing byte was dropped from the flushed piece"
        );
        assert!(
            lines[0].ends_with('\r'),
            "the trailing carriage return was trimmed"
        );
    }

    /// The seam between the two loops: one call that both emits a line and
    /// holds a tail.
    #[test]
    fn one_push_can_emit_a_line_and_hold_a_tail() {
        let mut a = LineAssembler::default();
        assert_eq!(a.push(b"one\ntwo"), vec!["one"]);
        assert_eq!(a.push(b"-end\n"), vec!["two-end"]);
    }

    /// Service names are ASCII in practice, but the padding should be honest
    /// about what it measures.
    #[test]
    fn a_wide_name_is_padded_by_the_columns_it_occupies() {
        let mut p = Prefixes::default();
        // Three scalars, six columns.
        assert_eq!(
            p.format("\u{65e5}\u{672c}\u{8a9e}"),
            "\u{65e5}\u{672c}\u{8a9e}  | "
        );
        // The column is six wide now, so a two-column name takes four spaces.
        assert_eq!(p.format("ab"), "ab      | ");
    }

    #[test]
    fn a_chunk_split_mid_line_reassembles_correctly() {
        let mut a = LineAssembler::default();
        a.push(b"start");
        a.push(b"-middle");
        assert_eq!(a.push(b"-end\nnext\n"), vec!["start-middle-end", "next"]);
    }

    /// Stands in for stdout, and counts flushes so the write path can be
    /// checked without a terminal or a pipe.
    #[derive(Default)]
    struct Sink {
        written: Vec<u8>,
        flushes: usize,
    }

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    impl Sink {
        fn lines(&self) -> Vec<String> {
            String::from_utf8_lossy(&self.written)
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    /// The state `run` carries across events, so a test can drive several
    /// containers through the same maps the way the loop does.
    #[derive(Default)]
    struct Stream {
        assemblers: HashMap<(String, u32), LineAssembler>,
        prefixes: Prefixes,
        out: Sink,
    }

    impl Stream {
        fn lines(&self) -> Vec<String> {
            self.out.lines()
        }

        fn emit(&mut self, service: &str, replica: u32, bytes: &[u8]) {
            handle_output(
                &mut self.assemblers,
                &mut self.prefixes,
                service.to_string(),
                replica,
                bytes,
                &mut self.out,
            )
            .expect("writing to a Vec cannot fail");
        }
    }

    /// Three containers writing at once, each caught mid-line: two replicas of
    /// a scaled service and a second service between them. Interleaving is the
    /// whole point -- one container at a time reassembles correctly however the
    /// map is keyed, and keying on the service name alone splices `web-1`'s
    /// held tail onto `web-2`'s next chunk into a line neither ever wrote.
    #[test]
    fn interleaved_partial_writes_from_replicas_and_services_stay_separate() {
        let mut s = Stream::default();

        s.emit("web", 1, b"GET /one");
        s.emit("api", 1, b"listening");
        s.emit("web", 2, b"GET /two");
        s.emit("web", 1, b" 200\n");
        s.emit("api", 1, b" on 8080\n");
        s.emit("web", 2, b" 404\n");

        assert_eq!(
            s.lines(),
            vec![
                "web-1  | GET /one 200",
                "api-1  | listening on 8080",
                "web-2  | GET /two 404",
            ]
        );
    }

    /// The prefix has to distinguish the replicas too. Separating the buffers
    /// but labelling both lines `web` would leave the reader unable to tell
    /// which container it was reading, which is most of the value of keeping
    /// them apart.
    #[test]
    fn replicas_of_one_service_are_labelled_apart() {
        let mut s = Stream::default();

        s.emit("web", 1, b"from one\n");
        s.emit("web", 2, b"from two\n");

        assert_eq!(s.lines(), vec!["web-1  | from one", "web-2  | from two"]);
    }

    /// An unscaled service is labelled `db-1`, not `db`: this path never sees
    /// a service list, so it cannot know a service is single until the run is
    /// over, and a suffix that appeared only when a second replica spoke would
    /// relabel the same container mid-stream.
    #[test]
    fn a_single_replica_is_still_named_by_its_container() {
        let mut s = Stream::default();

        s.emit("db", 1, b"ready\n");

        assert_eq!(s.lines(), vec!["db-1  | ready"]);
    }

    /// The alignment carries through to what is actually written, and each
    /// line keeps its own service's prefix while the column grows.
    #[test]
    fn the_written_prefixes_line_up_as_longer_names_arrive() {
        let mut s = Stream::default();

        s.emit("api", 1, b"up\n");
        s.emit("gateway", 12, b"up\n");
        s.emit("api", 1, b"still up\n");

        assert_eq!(
            s.lines(),
            vec!["api-1  | up", "gateway-12  | up", "api-1       | still up",]
        );
    }

    /// Every event is flushed, including one that completed no line: what is
    /// held back is a partial line, but what came before it is not, and a
    /// reader tailing a pipe should not wait on the next event to see it.
    #[test]
    fn every_event_is_flushed() {
        let mut s = Stream::default();

        s.emit("api", 1, b"one\n");
        assert_eq!(s.out.flushes, 1);

        s.emit("api", 1, b"a partial line");
        assert_eq!(s.out.flushes, 2, "an event emitting no line still flushed");
    }

    /// A dead reader is the ordinary way this path ends -- `run` returns the
    /// error and the process exits -- so the write must not be swallowed.
    #[test]
    fn a_failed_write_is_propagated() {
        struct Closed;

        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader gone"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let err = handle_output(
            &mut HashMap::new(),
            &mut Prefixes::default(),
            "api".to_string(),
            1,
            b"a line\n",
            &mut Closed,
        )
        .expect_err("a broken pipe should surface");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    /// The same failure can arrive on the flush rather than on the write: a
    /// buffered writer accepts bytes into its buffer and only discovers the
    /// closed pipe when it tries to drain them. Returning `flush`'s result is
    /// what makes that reach `run`, and swallowing it is invisible to every
    /// other test here -- the bytes still turn up in the sink.
    #[test]
    fn a_failed_flush_is_propagated() {
        #[derive(Default)]
        struct FlushFails {
            written: Vec<u8>,
        }

        impl Write for FlushFails {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.written.extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader gone"))
            }
        }

        let mut out = FlushFails::default();
        let err = handle_output(
            &mut HashMap::new(),
            &mut Prefixes::default(),
            "api".to_string(),
            1,
            b"a line\n",
            &mut out,
        )
        .expect_err("a broken pipe should surface from the flush");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(
            String::from_utf8_lossy(&out.written),
            "api-1  | a line\n",
            "the write itself succeeded, so only the flush can have failed"
        );
    }
}
