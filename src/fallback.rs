#![allow(clippy::missing_docs_in_private_items)] // 2 left to document
//! Plain streaming for when stdout isn't a terminal.
//!
//! The wrapping CLI may run in CI or with output piped, where a full-screen UI
//! is useless (and would emit escape sequences into a log file). In that case we
//! behave like `docker compose logs -f`: one prefixed line per log line.

use crate::docker::stream::MAX_CHUNK_BYTES;
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
/// It bounds accumulation *across* chunks, not any single one. That is only a
/// bound at all because the chunks themselves are bounded where they are read,
/// by `MAX_CHUNK_BYTES` in the Docker layer, and by a good deal less than this.
/// A run this long therefore always arrives in several chunks and is flushed
/// here, rather than turning up whole in one chunk and defeating the cap.
const MAX_PARTIAL: usize = 1024 * 1024;

/// Keeping a forwarded piece under this cap is what bounds the buffer itself.
/// `push` appends whatever it is handed before the cap is consulted, so the
/// buffer peaks at this cap plus one piece; a piece smaller than the cap keeps
/// that peak under twice it. Tuning the two apart is a build error rather than
/// a quiet doubling.
///
/// It is *not* what keeps whole lines whole -- a line is emitted whole as long
/// as its newline arrives before the bytes held for it pass the cap, which no
/// chunk size can change for a line that fits the cap in the first place.
const _: () = assert!(MAX_CHUNK_BYTES < MAX_PARTIAL);

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
        // run a megabyte without a newline. A line whose newline arrives while
        // it is still held is emitted whole however long it runs, since
        // splitting real lines would corrupt them for whatever is parsing the
        // log; only a run that passes the cap before any newline gets cut.
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

/// Prints events from `rx` until the channel closes, `cancel` fires, or a
/// write fails.
///
/// Split from [`run`] so the loop can be driven by a plain channel and a
/// `Vec<u8>`. What stays in `run` -- opening the channel and spawning the
/// supervisor -- needs a Docker daemon and so cannot be reached from a test,
/// but the two decisions that matter here can be: when the loop stops, and
/// what has been written by the time it does.
async fn stream_events(
    rx: &mut mpsc::Receiver<SourceEvent>,
    cancel: &CancellationToken,
    out: &mut impl Write,
) -> io::Result<()> {
    let mut assemblers: HashMap<(String, u32), LineAssembler> = HashMap::new();
    let mut prefixes = Prefixes::default();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            message = rx.recv() => {
                // `None` means every sender is gone -- the supervisor task
                // returned or panicked -- so no further event can arrive. A
                // closed channel yields `None` again immediately, so anything
                // but leaving here spins on it for the life of the process.
                let Some(message) = message else { break };
                if let SourceEvent::Output { service, replica, bytes } = message {
                    handle_output(
                        &mut assemblers,
                        &mut prefixes,
                        service,
                        replica,
                        &bytes,
                        out,
                    )?;
                }
            }
        }
    }
    Ok(())
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

    // `Stdout` takes the lock per write, where this loop used to hold one
    // `stdout.lock()` across each event. Nothing else writes to stdout while
    // this runs -- the supervisor's debug output goes to a file, failures go to
    // stderr, and the TUI is not running on this path -- so there is no second
    // writer for the wider lock to have been excluding. Passing a plain
    // `impl Write` is what puts the loop within reach of a test.
    stream_events(&mut rx, &cancel, &mut std::io::stdout()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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

    /// The stream layer now hands this path a long line in `MAX_CHUNK_BYTES`
    /// pieces where it used to hand it over whole. Anything under the
    /// assembler's own cap has to come back out as one line regardless, or the
    /// bound at the read site would have changed what gets printed.
    #[test]
    fn a_line_arriving_in_forwarded_pieces_still_prints_whole() {
        let mut a = LineAssembler::default();
        // The largest line the buffer is guaranteed to hold, so this exercises
        // the whole of it rather than a comfortable middle. Multi-byte
        // throughout, so the cuts land mid-character -- `from_utf8_lossy` runs
        // on the reassembled buffer, so a cut there must not corrupt anything.
        let unit = "h\u{e9}llo\u{b7}w\u{f6}rld ";
        let body: String = std::iter::repeat_n(unit, MAX_PARTIAL / unit.len()).collect();
        assert!(
            body.len() > MAX_CHUNK_BYTES,
            "the line has to span more than one forwarded piece to test anything"
        );
        let mut input = body.clone().into_bytes();
        input.extend_from_slice(b"\r\n");

        let mut lines = Vec::new();
        for piece in input.chunks(MAX_CHUNK_BYTES) {
            lines.extend(a.push(piece));
        }

        assert_eq!(lines, vec![body], "the split changed what is printed");
        assert!(a.partial.is_empty(), "held {} bytes back", a.partial.len());
    }

    /// Where the split does change what is printed, pinned exactly.
    ///
    /// A terminated line survives whole while the bytes held for it stay
    /// within the cap when each piece lands, so the boundary sits past the cap
    /// rather than on it. Beyond it the line is cut, which is the behaviour
    /// bounding the frame gives up: a line that long used to be printed whole
    /// when the daemon happened to deliver it in one frame, and is now cut
    /// whichever way it arrives.
    ///
    /// The numbers below are the current constants', where a piece divides the
    /// cap exactly and the boundary lands on `MAX_PARTIAL + MAX_CHUNK_BYTES`.
    /// A piece size that does not divide the cap moves it, because the run
    /// then overshoots mid-piece instead of landing on the cap, so retuning
    /// the bound means recomputing this rather than assuming the sum holds.
    #[test]
    fn a_terminated_line_survives_until_the_cap_plus_one_piece() {
        fn pieces_for(len: usize) -> Vec<String> {
            let mut a = LineAssembler::default();
            let mut input = vec![b'x'; len];
            input.push(b'\n');
            let mut lines = Vec::new();
            for piece in input.chunks(MAX_CHUNK_BYTES) {
                lines.extend(a.push(piece));
            }
            lines
        }

        let whole = pieces_for(MAX_PARTIAL + MAX_CHUNK_BYTES - 1);
        assert_eq!(whole.len(), 1, "cut a line that still fits");
        assert_eq!(whole[0].len(), MAX_PARTIAL + MAX_CHUNK_BYTES - 1);

        let cut = pieces_for(MAX_PARTIAL + MAX_CHUNK_BYTES);
        assert_eq!(cut.len(), 2, "kept a line past the point it can be held");
        assert_eq!(cut[0].len(), MAX_PARTIAL);
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
        /// Keeps every byte, so a test can assert on what was written rather
        /// than only that writing succeeded.
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        /// Counts rather than discards: whether the loop flushes is itself
        /// under test, since a piped reader sees nothing until it does.
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    impl Sink {
        /// What was written, split into lines for comparison.
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
        /// Everything printed so far, in order.
        fn lines(&self) -> Vec<String> {
            self.out.lines()
        }

        /// Feeds one container's output through the same call the run loop
        /// makes, so interleaving two containers here interleaves them the
        /// way the daemon would.
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

    /// A sink whose every write fails, standing in for the reader having gone
    /// away -- `head` closing the pipe is the ordinary case on this path.
    struct Closed;

    impl Write for Closed {
        /// Always fails, so the error under test can only be the write.
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader gone"))
        }

        /// Succeeds, so a failure can only have come from the write.
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A dead reader is the ordinary way this path ends -- `run` returns the
    /// error and the process exits -- so the write must not be swallowed.
    #[test]
    fn a_failed_write_is_propagated() {
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
            /// Succeeds, so the error under test can only be the flush.
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.written.extend_from_slice(buf);
                Ok(buf.len())
            }

            /// Fails after the write already succeeded -- the case that gets
            /// swallowed if the flush's result is discarded.
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

    /// The loop must stop when every sender is gone, and must still have
    /// printed what arrived before that.
    ///
    /// `recv` on a closed channel returns `None` immediately and for ever, so
    /// treating it as anything but an exit burns a core until the process is
    /// killed. Nothing else can end the loop here: `cancel` is never fired.
    #[tokio::test]
    async fn a_closed_channel_ends_the_loop_after_printing_what_arrived() {
        let (tx, mut rx) = mpsc::channel(4);
        tx.send(SourceEvent::Output {
            service: "web".to_string(),
            replica: 2,
            bytes: b"served\n".to_vec(),
        })
        .await
        .expect("the receiver is alive");
        // A non-Output event, which the loop skips rather than ends on.
        tx.send(SourceEvent::Topology)
            .await
            .expect("the receiver is alive");
        drop(tx);

        let mut out = Sink::default();
        let cancel = CancellationToken::new();
        let finished = tokio::time::timeout(
            Duration::from_secs(5),
            stream_events(&mut rx, &cancel, &mut out),
        )
        .await
        .expect("the loop did not stop when the last sender was dropped");

        finished.expect("writing to a Vec cannot fail");
        assert_eq!(out.lines(), vec!["web-2  | served"]);
    }

    /// Cancellation has to end the loop while a sender is still open, which is
    /// the ordinary Ctrl-C case: the supervisor is still attached and holding
    /// its `tx`, so the closed-channel exit is not available to stop it.
    #[tokio::test]
    async fn cancellation_ends_the_loop_while_a_sender_is_still_open() {
        let (tx, mut rx) = mpsc::channel::<SourceEvent>(4);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let mut out = Sink::default();
        tokio::time::timeout(
            Duration::from_secs(5),
            stream_events(&mut rx, &cancel, &mut out),
        )
        .await
        .expect("cancellation did not stop the loop")
        .expect("writing to a Vec cannot fail");

        assert!(
            !tx.is_closed(),
            "the sender was open, so only cancel can have ended it"
        );
        assert!(out.lines().is_empty(), "nothing was ever sent");
    }

    /// A dead reader must come back out of the loop rather than being
    /// swallowed per event. It is the third way `run` is documented to end,
    /// and with the sender still open it is the only way this call can return
    /// at all -- so losing the `?` leaves the loop waiting for ever.
    #[tokio::test]
    async fn a_write_failure_ends_the_loop_and_is_returned() {
        let (tx, mut rx) = mpsc::channel(4);
        tx.send(SourceEvent::Output {
            service: "api".to_string(),
            replica: 1,
            bytes: b"a line\n".to_vec(),
        })
        .await
        .expect("the receiver is alive");

        let cancel = CancellationToken::new();
        let err = tokio::time::timeout(
            Duration::from_secs(5),
            stream_events(&mut rx, &cancel, &mut Closed),
        )
        .await
        .expect("the loop did not stop on a write failure")
        .expect_err("a broken pipe should surface");

        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }
}
