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

/// Size used before the first layout pass tells us the real pane geometry.
const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;

pub struct LogStore {
    parser: vt100::Parser,
    /// True once any output at all has been received.
    has_output: bool,
}

impl LogStore {
    pub fn new(scrollback: usize) -> Self {
        Self {
            parser: vt100::Parser::new(INITIAL_ROWS, INITIAL_COLS, scrollback),
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
        let offset = self.parser.screen().scrollback();
        if offset == 0 {
            self.parser.process(bytes);
            self.has_output = true;
            return;
        }

        let before = self.max_scroll();
        self.parser.process(bytes);
        let after = self.max_scroll();
        let added = after.saturating_sub(before);
        self.parser.screen_mut().set_scrollback(offset + added);
        self.has_output = true;
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
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (cur_rows, cur_cols) = self.parser.screen().size();
        let rows = rows.max(1);
        let cols = cols.max(1);
        if (cur_rows, cur_cols) != (rows, cols) {
            self.parser.screen_mut().set_size(rows, cols);
        }
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
    fn resizing_is_idempotent_and_clamps_to_one() {
        let mut s = store_with(5);
        s.resize(20, 60);
        assert_eq!(s.screen().size(), (20, 60));
        s.resize(0, 0);
        assert_eq!(s.screen().size(), (1, 1));
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
