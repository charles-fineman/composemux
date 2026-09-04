//! Per-service log buffer.
//!
//! Container output is fed straight into a `vt100` terminal emulator, the same
//! way nx feeds its PTY output. That gets us SGR colour, `\r` progress rewrites
//! and cursor motion handled correctly, plus per-cell access so search matches
//! can be highlighted over already-coloured output.
//!
//! Scrollback semantics follow nx: the offset counts rows *back from the
//! bottom*, so `0` means "tailing live output".

/// Rows of scrollback retained per service. Matches nx's `SCROLLBACK_SIZE`.
pub const DEFAULT_SCROLLBACK: usize = 1_000;

/// Retained bytes per row of scrollback. Generous: a replay must be able to
/// reproduce everything the parser would have kept, and long lines are common.
const RAW_BYTES_PER_ROW: usize = 1024;
const MIN_RAW_CAP: usize = 256 * 1024;
const MAX_RAW_CAP: usize = 8 * 1024 * 1024;

fn raw_cap_for(scrollback: usize) -> usize {
    scrollback
        .saturating_mul(RAW_BYTES_PER_ROW)
        .clamp(MIN_RAW_CAP, MAX_RAW_CAP)
}

/// Floor on the emulated screen size.
///
/// `vt100` underflows in `col_wrap` on very narrow grids, so this is a crash
/// guard rather than a cosmetic minimum. It matches the floor the pane's own
/// geometry already applies, so it never binds in the render path.
const MIN_ROWS: u16 = 3;
const MIN_COLS: u16 = 20;

/// Size used before the first layout pass tells us the real pane geometry.
const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;

pub struct LogStore {
    parser: vt100::Parser,
    /// Rows of scrollback the parser retains; needed to rebuild it on resize.
    scrollback_len: usize,
    /// Normalised bytes as fed to the parser, replayed when the width changes.
    ///
    /// `vt100` stores rows already wrapped and does not reflow them, so the
    /// only way to rewrap history is to parse it again at the new width.
    raw: Vec<u8>,
    /// Upper bound on `raw`, trimmed at a line boundary. Sized to comfortably
    /// exceed `scrollback_len` lines so a replay still reproduces everything
    /// the parser would have retained.
    raw_cap: usize,
    /// Whether the previous chunk ended on a carriage return, so a `\r\n` split
    /// across chunks isn't mistaken for a bare newline.
    pending_cr: bool,
    /// True once any output at all has been received.
    has_output: bool,
}

impl LogStore {
    pub fn new(scrollback: usize) -> Self {
        Self {
            parser: vt100::Parser::new(INITIAL_ROWS, INITIAL_COLS, scrollback),
            scrollback_len: scrollback,
            raw: Vec::new(),
            raw_cap: raw_cap_for(scrollback),
            pending_cr: false,
            has_output: false,
        }
    }

    pub fn has_output(&self) -> bool {
        self.has_output
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Feeds raw container output to the emulator.
    ///
    /// A view at offset 0 keeps tailing. A view scrolled up stays anchored to
    /// the *content* it is showing: the offset is advanced by however many rows
    /// the write pushed into scrollback.
    ///
    /// This is a deliberate deviation from nx, which preserves the raw offset
    /// and therefore lets the view drift as new output arrives. Nx task output
    /// is short and finite, so drift is cosmetic there; container logs are
    /// unbounded, and drifting away from a stack trace while reading it is
    /// exactly the wrong behaviour.
    ///
    /// # Limitation
    ///
    /// Anchoring holds only while the buffer is still filling. Once it reaches
    /// its row limit, every new row evicts the oldest, and the offset — which
    /// counts back from the bottom and is clamped to the retained depth — can no
    /// longer be advanced to compensate. `vt100` exposes neither a scroll
    /// callback nor an absolute row counter, so there is no signal to measure
    /// the eviction against; past saturation the view drifts as nx's does. A
    /// larger `scrollback` widens the window in which anchoring works, at a
    /// linear memory cost.
    pub fn process(&mut self, bytes: &[u8]) {
        let normalised = self.normalise_newlines(bytes);
        self.retain(&normalised);

        let offset = self.parser.screen().scrollback();
        if offset == 0 {
            self.parser.process(&normalised);
            self.has_output = true;
            return;
        }

        let before = self.max_scroll();
        self.parser.process(&normalised);
        let after = self.max_scroll();
        let added = after.saturating_sub(before);
        self.parser.screen_mut().set_scrollback(offset + added);
        self.has_output = true;
    }

    /// Turns a lone `\n` into `\r\n`, the way a terminal driver's ONLCR would.
    ///
    /// Container logs are LF-terminated. Fed to the emulator raw, a bare `\n`
    /// moves the cursor down without returning it to column 0, so every line
    /// starts where the last one ended and the output walks off to the right.
    ///
    /// The carry flag matters: output arrives in arbitrary chunks, so a `\r` can
    /// end one and its `\n` begin the next. Deciding per chunk would insert a
    /// spurious `\r` at that seam.
    fn normalise_newlines(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        let mut prev_cr = self.pending_cr;
        for &byte in input {
            if byte == b'\n' && !prev_cr {
                out.push(b'\r');
            }
            out.push(byte);
            prev_cr = byte == b'\r';
        }
        self.pending_cr = prev_cr;
        out
    }

    /// Keeps the bytes needed to rewrap on resize, bounded and cut at a line
    /// boundary so a replay never begins mid-escape-sequence.
    fn retain(&mut self, bytes: &[u8]) {
        self.raw.extend_from_slice(bytes);
        if self.raw.len() <= self.raw_cap {
            return;
        }
        let excess = self.raw.len() - self.raw_cap;
        let cut = self.raw[excess..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|offset| excess + offset + 1)
            // No line boundary anywhere in the tail, which happens with output
            // that only ever emits carriage returns -- a progress bar, say.
            // Cut at the cap regardless: the first replayed row may be garbled
            // if the cut lands inside an escape sequence, which is a far better
            // outcome than discarding the buffer and blanking the pane.
            .unwrap_or(excess);
        self.raw.drain(..cut);
    }

    /// Rows of scrollback currently retained above the visible window.
    fn max_scroll(&mut self) -> usize {
        let current = self.parser.screen().scrollback();
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let max = self.parser.screen().scrollback();
        self.parser.screen_mut().set_scrollback(current);
        max
    }

    /// Resizes the emulated terminal to the pane's inner area.
    ///
    /// A width change rebuilds the parser and replays the retained bytes,
    /// because `vt100` keeps rows at the width they arrived at and will not
    /// rewrap them. A height-only change needs no replay.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(MIN_ROWS);
        let cols = cols.max(MIN_COLS);
        let (cur_rows, cur_cols) = self.parser.screen().size();
        if (cur_rows, cur_cols) == (rows, cols) {
            return;
        }

        let old_offset = self.parser.screen().scrollback();

        // Rebuilding from an empty buffer would blank a pane that currently
        // has content, so keep what is on screen and forgo the rewrap instead.
        if cols != cur_cols && !(self.raw.is_empty() && self.has_output) {
            let mut rebuilt = vt100::Parser::new(rows, cols, self.scrollback_len);
            rebuilt.process(&self.raw);
            self.parser = rebuilt;
        } else {
            self.parser.screen_mut().set_size(rows, cols);
        }

        // Losing height moves the bottom of the window up under a scrolled-up
        // reader, so pull the offset back by the rows lost. Anything else keeps
        // its position.
        let target = if rows < cur_rows && old_offset > 0 {
            old_offset.saturating_sub((cur_rows - rows) as usize)
        } else {
            old_offset
        };
        self.parser.screen_mut().set_scrollback(target);
    }

    /// Rows scrolled back from the bottom. `0` means tailing.
    pub fn scroll_offset(&self) -> usize {
        self.parser.screen().scrollback()
    }

    pub fn scroll_up(&mut self, lines: u16) {
        let target = self.scroll_offset().saturating_add(lines as usize);
        self.parser.screen_mut().set_scrollback(target);
    }

    pub fn scroll_down(&mut self, lines: u16) {
        let target = self.scroll_offset().saturating_sub(lines as usize);
        self.parser.screen_mut().set_scrollback(target);
    }

    pub fn scroll_to_top(&mut self) {
        // vt100 clamps to the number of retained rows.
        self.parser.screen_mut().set_scrollback(usize::MAX);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }

    /// The full retained buffer as plain text, for clipboard copy.
    ///
    /// Walks the scrollback from the top down. Each window overlaps the next by
    /// all but `advance` rows, so only the first `advance` rows of each window
    /// are new — taking the whole window would duplicate content whenever the
    /// scrollback depth is not an exact multiple of the pane height.
    pub fn all_text(&mut self) -> String {
        let saved = self.scroll_offset();
        let (rows, cols) = self.parser.screen().size();

        self.parser.screen_mut().set_scrollback(usize::MAX);
        let mut offset = self.parser.screen().scrollback();

        let mut lines: Vec<String> = Vec::new();
        loop {
            self.parser.screen_mut().set_scrollback(offset);
            let window = self.parser.screen().rows(0, cols);
            if offset == 0 {
                lines.extend(window);
                break;
            }
            let advance = (rows as usize).min(offset);
            lines.extend(window.take(advance));
            offset -= advance;
        }

        self.parser.screen_mut().set_scrollback(saved);

        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        let mut out = lines.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }

    /// Visible rows as plain strings.
    ///
    /// Test-only: rendering blits cells straight from the emulator, and the
    /// scrollbar measures the screen's geometry, so nothing in the running
    /// program needs the text materialised.
    #[cfg(test)]
    pub fn visible_lines(&self) -> Vec<String> {
        let (_, cols) = self.parser.screen().size();
        self.parser.screen().rows(0, cols).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The view is tailing when it sits at the bottom of the buffer.
    fn tailing(store: &LogStore) -> bool {
        store.scroll_offset() == 0
    }

    /// Visible rows with the blank padding removed.
    fn non_empty(store: &LogStore) -> Vec<String> {
        store
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }

    fn store_with(lines: usize) -> LogStore {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        for i in 0..lines {
            s.process(format!("line {i}\r\n").as_bytes());
        }
        s
    }

    #[test]
    fn a_new_store_reports_no_output() {
        let s = LogStore::new(DEFAULT_SCROLLBACK);
        assert!(!s.has_output());
    }

    #[test]
    fn processing_marks_output_and_tails() {
        let s = store_with(3);
        assert!(s.has_output());
        assert!(tailing(&s), "a fresh store should be pinned to the bottom");
    }

    #[test]
    fn newest_lines_are_visible_when_tailing() {
        let s = store_with(50);
        let visible = s.visible_lines();
        assert!(
            visible.iter().any(|l| l.contains("line 49")),
            "expected the newest line, got: {visible:?}"
        );
        assert!(!visible.iter().any(|l| l.contains("line 0")));
    }

    #[test]
    fn scrolling_up_then_down_returns_to_tailing() {
        let mut s = store_with(50);
        s.scroll_up(5);
        assert_eq!(s.scroll_offset(), 5);
        assert!(!tailing(&s));
        s.scroll_down(5);
        assert!(tailing(&s));
    }

    #[test]
    fn scrolling_down_past_the_bottom_clamps() {
        let mut s = store_with(50);
        s.scroll_up(3);
        s.scroll_down(999);
        assert_eq!(s.scroll_offset(), 0);
    }

    #[test]
    fn scrolling_up_past_the_top_clamps_to_retained_rows() {
        let mut s = store_with(50);
        s.scroll_up(u16::MAX);
        let max = s.scroll_offset();
        assert!(max > 0 && max < 50, "expected a bounded top, got {max}");
        // Already at the top: going further changes nothing.
        s.scroll_up(10);
        assert_eq!(s.scroll_offset(), max);
    }

    #[test]
    fn top_and_bottom_helpers_reach_the_extremes() {
        let mut s = store_with(50);
        s.scroll_to_top();
        assert!(!tailing(&s));
        s.scroll_to_bottom();
        assert!(tailing(&s));
    }

    #[test]
    fn a_tailing_view_keeps_tailing_as_output_arrives() {
        let mut s = store_with(20);
        s.process(b"newest\r\n");
        assert!(tailing(&s));
        assert!(s.visible_lines().iter().any(|l| l.contains("newest")));
    }

    #[test]
    fn a_scrolled_view_stays_on_the_same_content_as_output_arrives() {
        let mut s = store_with(20);
        s.scroll_up(4);
        let before: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();

        s.process(b"newest\r\n");

        let after: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        assert_eq!(
            before, after,
            "a scrolled-up reader should not be dragged along by new output"
        );
        assert_eq!(s.scroll_offset(), 5, "the offset absorbs the new row");
    }

    #[test]
    fn a_scrolled_view_drifts_once_the_buffer_saturates() {
        // Pins the documented limitation so it stays a known trade rather than
        // becoming an accidental regression: with the ring buffer full there is
        // no room left to advance the offset into.
        let mut s = LogStore::new(64);
        s.resize(10, 40);
        for i in 0..200 {
            s.process(format!("line {i}\r\n").as_bytes());
        }
        s.scroll_up(5);
        let before = s.scroll_offset();
        let content_before: Vec<String> = s.visible_lines();
        for i in 0..20 {
            s.process(format!("more {i}\r\n").as_bytes());
        }
        assert_eq!(
            s.scroll_offset(),
            before,
            "the offset cannot advance past the retained depth"
        );
        assert_ne!(
            content_before,
            s.visible_lines(),
            "so the content underneath it has moved on"
        );
    }

    #[test]
    fn a_scrolled_view_absorbs_a_burst_of_output() {
        let mut s = store_with(30);
        s.scroll_up(6);
        let before: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        for i in 0..25 {
            s.process(format!("burst {i}\r\n").as_bytes());
        }
        let after: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        assert_eq!(before, after, "content should hold still through a burst");
    }

    // ---- newline normalisation (#11) ----

    #[test]
    fn bare_line_feeds_start_at_column_zero() {
        // Container logs are LF-terminated. Fed raw to the emulator, each line
        // would start where the last one ended and walk off to the right.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(6, 40);
        s.process(b"first line\nsecond line\nthird line\n");
        let lines: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines, ["first line", "second line", "third line"]);
    }

    #[test]
    fn carriage_return_line_feed_is_left_alone() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(6, 40);
        s.process(b"alpha\r\nbeta\r\n");
        let lines: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines, ["alpha", "beta"]);
    }

    #[test]
    fn a_crlf_split_across_chunks_is_not_treated_as_a_bare_newline() {
        // The seam case: deciding per chunk would insert a spurious \r here.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(6, 40);
        s.process(b"alpha\r");
        s.process(b"\nbeta\r\n");
        let lines: Vec<String> = s
            .visible_lines()
            .iter()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines, ["alpha", "beta"]);
    }

    #[test]
    fn a_line_split_mid_word_across_chunks_still_reads_correctly() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(6, 40);
        s.process(b"data");
        s.process(b"base ready\n");
        assert!(s
            .visible_lines()
            .iter()
            .any(|l| l.trim_end() == "database ready"));
    }

    // ---- reflow on resize (#8) ----

    #[test]
    fn widening_rewraps_existing_output() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        let long = "A".repeat(30) + &"B".repeat(30);
        s.process(format!("{long}\n").as_bytes());
        // At 40 columns it needs two rows.
        assert_eq!(non_empty(&s).len(), 2);

        s.resize(10, 100);
        // At 100 it fits on one, which only happens if history was reparsed.
        let after = non_empty(&s);
        assert_eq!(after.len(), 1, "history should rewrap, got {after:?}");
        assert_eq!(after[0], long);
    }

    #[test]
    fn narrowing_rewraps_existing_output() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 100);
        let long = "A".repeat(30) + &"B".repeat(30);
        s.process(format!("{long}\n").as_bytes());
        assert_eq!(non_empty(&s).len(), 1);

        s.resize(10, 40);
        assert_eq!(non_empty(&s).len(), 2, "narrowing should rewrap too");
    }

    #[test]
    fn a_height_only_change_keeps_the_content() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        s.process(b"stable line\n");
        let before = non_empty(&s);
        s.resize(20, 40);
        assert_eq!(before, non_empty(&s));
    }

    #[test]
    fn scroll_position_survives_a_widen() {
        let mut s = store_with(60);
        s.scroll_up(5);
        s.resize(10, 100);
        assert_eq!(s.scroll_offset(), 5, "widening should not move the reader");
    }

    #[test]
    fn losing_height_pulls_a_scrolled_reader_back_by_the_rows_lost() {
        let mut s = store_with(60);
        s.scroll_up(10);
        s.resize(6, 40); // same width, four rows shorter
        assert_eq!(s.scroll_offset(), 6);
    }

    #[test]
    fn a_tailing_reader_keeps_tailing_across_a_resize() {
        let mut s = store_with(60);
        assert!(tailing(&s));
        s.resize(10, 100);
        assert!(tailing(&s), "a reader at the bottom should stay there");
    }

    #[test]
    fn the_retained_buffer_is_bounded_and_cut_at_a_line_boundary() {
        let mut s = LogStore::new(16);
        s.resize(5, 40);
        for i in 0..5_000 {
            s.process(format!("line {i} with some padding to take up room\n").as_bytes());
        }
        assert!(s.raw.len() <= s.raw_cap, "raw buffer must stay bounded");
        assert!(
            s.raw.starts_with(b"line "),
            "a trim must land on a line boundary, not mid-sequence"
        );
        // And it still rewraps correctly after trimming.
        s.resize(5, 100);
        assert!(non_empty(&s).iter().any(|l| l.contains("line 4999")));
    }

    #[test]
    fn output_without_newlines_survives_a_resize() {
        // A \r-driven progress bar emits no newline at all, so the retained
        // buffer can pass its cap with no line boundary to trim at.
        let mut s = LogStore::new(16);
        s.resize(5, 40);
        s.process(b"visible content\n");
        let blob = vec![b'X'; s.raw_cap + 1];
        s.process(&blob);

        assert!(
            !s.raw.is_empty(),
            "an oversized record must not discard every retained byte"
        );
        s.resize(5, 100);
        assert!(
            !non_empty(&s).is_empty(),
            "the pane went blank after a resize"
        );
    }

    #[test]
    fn a_resize_never_blanks_a_pane_that_has_content() {
        // Belt and braces for the case above: even if the retained buffer were
        // empty, what is already on screen must survive a width change.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(5, 40);
        s.process(b"still here\n");
        s.raw.clear();
        s.resize(5, 100);
        assert!(non_empty(&s).iter().any(|l| l.contains("still here")));
    }

    #[test]
    fn ansi_colour_is_interpreted_not_printed() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        s.process(b"\x1b[31mRED\x1b[0m\r\n");
        let visible = s.visible_lines();
        assert!(visible.iter().any(|l| l.contains("RED")));
        assert!(
            !visible.iter().any(|l| l.contains("\x1b")),
            "escape sequences should be consumed by the emulator"
        );
        let cell = s.screen().cell(0, 0).unwrap();
        assert_eq!(cell.fgcolor(), vt100::Color::Idx(1));
    }

    #[test]
    fn carriage_returns_rewrite_the_line() {
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        s.process(b"50%\r100%\r\n");
        let visible = s.visible_lines();
        assert!(visible.iter().any(|l| l.starts_with("100%")));
    }

    #[test]
    fn resizing_is_idempotent_and_clamps_to_a_safe_floor() {
        let mut s = store_with(5);
        s.resize(20, 60);
        assert_eq!(s.screen().size(), (20, 60));
        // vt100 underflows on very narrow grids, so the floor is a crash guard.
        s.resize(0, 0);
        assert_eq!(s.screen().size(), (MIN_ROWS, MIN_COLS));
    }

    #[test]
    fn collapsing_to_nothing_does_not_panic_while_replaying() {
        // Regression: rebuilding the parser replays retained output, and
        // replaying into a one-column grid panicked inside vt100.
        let mut s = store_with(40);
        for cols in [1, 2, 5, 19, 20, 21, 200, 1] {
            s.resize(1, cols);
        }
        assert!(s.screen().size().1 >= MIN_COLS);
    }

    #[test]
    fn all_text_does_not_duplicate_lines_at_an_uneven_scrollback_depth() {
        // 45 lines in a 10-row window leaves 35 rows of scrollback, which is not
        // a multiple of the window height - the case where a naive chunked walk
        // re-reads overlapping windows.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        for i in 0..45 {
            s.process(format!("line {i}\r\n").as_bytes());
        }
        let text = s.all_text();
        for i in 0..45 {
            let needle = format!("line {i}");
            let hits = text.lines().filter(|l| l.trim_end() == needle).count();
            assert_eq!(hits, 1, "expected exactly one 'line {i}', found {hits}");
        }
    }

    #[test]
    fn all_text_covers_more_than_the_retained_window() {
        // Beyond DEFAULT_SCROLLBACK the oldest lines are evicted, but everything
        // still retained must appear exactly once.
        let mut s = LogStore::new(DEFAULT_SCROLLBACK);
        s.resize(10, 40);
        for i in 0..1500 {
            s.process(format!("line {i}\r\n").as_bytes());
        }
        let text = s.all_text();
        assert!(
            !text.contains("line 0\n"),
            "oldest lines should have been evicted"
        );
        assert!(text.contains("line 1499"), "newest line must be present");
        assert_eq!(
            text.lines().filter(|l| l.trim_end() == "line 1400").count(),
            1
        );
        assert_eq!(s.scroll_offset(), 0, "copying must not move the view");
    }

    #[test]
    fn all_text_includes_scrolled_off_lines() {
        let mut s = store_with(40);
        let text = s.all_text();
        assert!(text.contains("line 0"), "expected scrollback in the copy");
        assert!(text.contains("line 39"), "expected the newest line too");
        assert!(tailing(&s), "copying must not disturb the scroll position");
    }
}
