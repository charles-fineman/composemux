#![allow(clippy::missing_docs_in_private_items)] // 25 left to document
//! Frame geometry: where the service list, the output panes and the status bar go.
//!
//! Ported from nx `packages/nx/src/native/tui/components/layout_manager.rs`
//! (MIT, (c) 2017-2026 Narwhal Technologies Inc.)
//!
//! The constants and the auto-mode breakpoints are reproduced exactly, because
//! matching nx's proportions is the point of the tool.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Minimum width at which a side-by-side layout is viable.
const MIN_HORIZONTAL_WIDTH: u16 = 120;
/// Minimum height at which a stacked layout is viable.
const MIN_VERTICAL_HEIGHT: u16 = 30;
/// Gap columns between the service list and the panes.
const HORIZONTAL_PADDING: u16 = 2;
/// Gap rows between the service list and the panes.
const VERTICAL_PADDING: u16 = 1;

/// Below this the UI cannot be drawn legibly and we show a notice instead.
pub const MIN_FRAME_WIDTH: u16 = 40;
pub const MIN_FRAME_HEIGHT: u16 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutMode {
    #[default]
    Auto,
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneArrangement {
    #[default]
    None,
    Single,
    Double,
}

impl PaneArrangement {
    /// The arrangement implied by a number of occupied panes.
    pub fn for_count(count: usize) -> Self {
        match count {
            0 => PaneArrangement::None,
            1 => PaneArrangement::Single,
            _ => PaneArrangement::Double,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListVisibility {
    #[default]
    Visible,
    Hidden,
}

/// Resolved rectangles for one frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LayoutAreas {
    pub service_list: Option<Rect>,
    pub panes: Vec<Rect>,
    pub status_bar: Option<Rect>,
}

#[derive(Debug, Clone, Default)]
pub struct LayoutManager {
    mode: LayoutMode,
    arrangement: PaneArrangement,
    visibility: ListVisibility,
}

impl LayoutManager {
    pub fn arrangement(&self) -> PaneArrangement {
        self.arrangement
    }

    pub fn set_arrangement(&mut self, arrangement: PaneArrangement) {
        self.arrangement = arrangement;
    }

    pub fn visibility(&self) -> ListVisibility {
        self.visibility
    }

    pub fn set_visibility(&mut self, visibility: ListVisibility) {
        self.visibility = visibility;
    }

    pub fn toggle_visibility(&mut self) {
        self.visibility = match self.visibility {
            ListVisibility::Visible => ListVisibility::Hidden,
            ListVisibility::Hidden => ListVisibility::Visible,
        };
    }

    /// Flips between vertical and horizontal. From `Auto`, moves to the opposite
    /// of whatever auto currently resolves to, so the first press always visibly
    /// changes something.
    pub fn toggle_mode(&mut self, area: Rect) {
        self.mode = match self.mode {
            LayoutMode::Auto => {
                if self.prefers_vertical(area.width, area.height) {
                    LayoutMode::Horizontal
                } else {
                    LayoutMode::Vertical
                }
            }
            LayoutMode::Vertical => LayoutMode::Horizontal,
            LayoutMode::Horizontal => LayoutMode::Vertical,
        };
    }

    /// Whether a stacked layout suits these dimensions. Verbatim from nx.
    fn prefers_vertical(&self, width: u16, height: u16) -> bool {
        // Screen is pretty narrow so always prefer vertical layout
        if width < 75 {
            return true;
        }
        let aspect_ratio = width as f32 / height as f32;
        // If very wide and not very tall, prefer horizontal
        if aspect_ratio > 2.0 && height < MIN_VERTICAL_HEIGHT {
            return false;
        }
        // If very tall and not very wide, prefer vertical
        if aspect_ratio < 1.0 && width < MIN_HORIZONTAL_WIDTH {
            return true;
        }
        aspect_ratio < 1.5
    }

    fn resolved_mode(&self, area: Rect) -> LayoutMode {
        match self.mode {
            LayoutMode::Auto => {
                if self.prefers_vertical(area.width, area.height) {
                    LayoutMode::Vertical
                } else {
                    LayoutMode::Horizontal
                }
            }
            explicit => explicit,
        }
    }

    /// Splits `area` into the service list, output panes and status bar.
    pub fn calculate(&self, area: Rect, status_bar_height: u16) -> LayoutAreas {
        let (main, status_bar) = if status_bar_height > 0 && area.height > status_bar_height {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Fill(1), Constraint::Length(status_bar_height)])
                .split(area);
            (chunks[0], Some(chunks[1]))
        } else {
            (area, None)
        };

        // With the list hidden the panes take everything; with no panes the list does.
        if self.visibility == ListVisibility::Hidden && self.arrangement != PaneArrangement::None {
            return LayoutAreas {
                service_list: None,
                panes: self.split_panes(main, self.resolved_mode(area)),
                status_bar,
            };
        }
        if self.arrangement == PaneArrangement::None {
            return LayoutAreas {
                service_list: Some(main),
                panes: Vec::new(),
                status_bar,
            };
        }

        match self.resolved_mode(area) {
            LayoutMode::Vertical => {
                let list_height = if main.height < 3 { 1 } else { main.height / 3 };
                let padding = if main.height > list_height + VERTICAL_PADDING {
                    VERTICAL_PADDING
                } else {
                    0
                };
                let pane_height = main
                    .height
                    .saturating_sub(list_height)
                    .saturating_sub(padding);
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(list_height),
                        Constraint::Length(padding),
                        Constraint::Length(pane_height),
                    ])
                    .split(main);
                LayoutAreas {
                    service_list: Some(chunks[0]),
                    panes: self.split_panes(chunks[2], LayoutMode::Vertical),
                    status_bar,
                }
            }
            _ => {
                let list_width = if main.width < 3 { 1 } else { main.width / 3 };
                let padding = if main.width > list_width + HORIZONTAL_PADDING {
                    HORIZONTAL_PADDING
                } else {
                    0
                };
                let pane_width = main
                    .width
                    .saturating_sub(list_width)
                    .saturating_sub(padding);
                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Length(list_width),
                        Constraint::Length(padding),
                        Constraint::Length(pane_width),
                    ])
                    .split(main);
                LayoutAreas {
                    service_list: Some(chunks[0]),
                    panes: self.split_panes(chunks[2], LayoutMode::Horizontal),
                    status_bar,
                }
            }
        }
    }

    /// Two panes split across the short axis of the layout: side by side when
    /// the list is stacked above them, stacked when the list is beside them.
    fn split_panes(&self, area: Rect, mode: LayoutMode) -> Vec<Rect> {
        match self.arrangement {
            PaneArrangement::None => Vec::new(),
            PaneArrangement::Single => vec![area],
            PaneArrangement::Double => {
                let direction = if mode == LayoutMode::Vertical {
                    Direction::Horizontal
                } else {
                    Direction::Vertical
                };
                Layout::default()
                    .direction(direction)
                    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(area)
                    .to_vec()
            }
        }
    }
}

/// Whether the frame is too small to draw the UI.
pub fn is_too_small(area: Rect) -> bool {
    area.height < MIN_FRAME_HEIGHT || area.width < MIN_FRAME_WIDTH
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(arrangement: PaneArrangement) -> LayoutManager {
        LayoutManager {
            arrangement,
            ..Default::default()
        }
    }

    #[test]
    fn narrow_terminals_always_stack() {
        let m = LayoutManager::default();
        assert!(m.prefers_vertical(74, 100));
        assert!(m.prefers_vertical(74, 10));
    }

    #[test]
    fn wide_and_short_terminals_go_side_by_side() {
        let m = LayoutManager::default();
        // aspect 4.0, height below the vertical minimum
        assert!(!m.prefers_vertical(120, 29));
    }

    #[test]
    fn tall_and_narrow_terminals_stack() {
        let m = LayoutManager::default();
        // aspect 0.8, width below the horizontal minimum
        assert!(m.prefers_vertical(80, 100));
    }

    #[test]
    fn aspect_ratio_decides_the_remaining_cases() {
        let m = LayoutManager::default();
        // 1.4 -> vertical, 1.6 -> horizontal
        assert!(m.prefers_vertical(140, 100));
        assert!(!m.prefers_vertical(160, 100));
    }

    #[test]
    fn list_takes_the_whole_frame_when_no_panes_are_open() {
        let m = manager(PaneArrangement::None);
        let areas = m.calculate(Rect::new(0, 0, 100, 40), 1);
        assert_eq!(areas.service_list, Some(Rect::new(0, 0, 100, 39)));
        assert!(areas.panes.is_empty());
        assert_eq!(areas.status_bar, Some(Rect::new(0, 39, 100, 1)));
    }

    #[test]
    fn horizontal_list_is_one_third_of_the_width_with_a_two_column_gap() {
        let mut m = manager(PaneArrangement::Single);
        m.mode = LayoutMode::Horizontal;
        let areas = m.calculate(Rect::new(0, 0, 120, 40), 1);
        let list = areas.service_list.unwrap();
        assert_eq!(list.width, 40, "list should be floor(width/3)");
        assert_eq!(
            areas.panes[0].x, 42,
            "two columns of padding after the list"
        );
        assert_eq!(areas.panes[0].width, 78);
    }

    #[test]
    fn vertical_list_is_one_third_of_the_height_with_a_one_row_gap() {
        let mut m = manager(PaneArrangement::Single);
        m.mode = LayoutMode::Vertical;
        let areas = m.calculate(Rect::new(0, 0, 60, 40), 1);
        let list = areas.service_list.unwrap();
        assert_eq!(list.height, 13, "list should be floor(height/3)");
        assert_eq!(areas.panes[0].y, 14, "one row of padding after the list");
    }

    #[test]
    fn two_panes_split_the_pane_area_evenly() {
        let mut m = manager(PaneArrangement::Double);
        m.mode = LayoutMode::Horizontal;
        let areas = m.calculate(Rect::new(0, 0, 120, 40), 1);
        assert_eq!(areas.panes.len(), 2);
        // Horizontal layout stacks the two panes vertically.
        assert_eq!(areas.panes[0].width, areas.panes[1].width);
        assert!(areas.panes[0].y < areas.panes[1].y);
    }

    #[test]
    fn hiding_the_list_gives_the_panes_the_whole_frame() {
        let mut m = manager(PaneArrangement::Single);
        m.set_visibility(ListVisibility::Hidden);
        let areas = m.calculate(Rect::new(0, 0, 100, 40), 1);
        assert!(areas.service_list.is_none());
        assert_eq!(areas.panes[0], Rect::new(0, 0, 100, 39));
    }

    #[test]
    fn toggling_from_auto_picks_the_opposite_of_the_resolved_mode() {
        let area = Rect::new(0, 0, 60, 40); // narrow -> auto resolves vertical
        let mut m = LayoutManager::default();
        assert_eq!(m.resolved_mode(area), LayoutMode::Vertical);
        m.toggle_mode(area);
        assert_eq!(m.mode, LayoutMode::Horizontal);
        m.toggle_mode(area);
        assert_eq!(m.mode, LayoutMode::Vertical);
    }

    #[test]
    fn too_small_frames_are_detected() {
        assert!(is_too_small(Rect::new(0, 0, 39, 20)));
        assert!(is_too_small(Rect::new(0, 0, 80, 9)));
        assert!(!is_too_small(Rect::new(0, 0, 40, 10)));
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    fn arrangements() -> impl Strategy<Value = PaneArrangement> {
        prop_oneof![
            Just(PaneArrangement::None),
            Just(PaneArrangement::Single),
            Just(PaneArrangement::Double),
        ]
    }

    fn modes() -> impl Strategy<Value = LayoutMode> {
        prop_oneof![
            Just(LayoutMode::Auto),
            Just(LayoutMode::Vertical),
            Just(LayoutMode::Horizontal),
        ]
    }

    fn visibilities() -> impl Strategy<Value = ListVisibility> {
        prop_oneof![Just(ListVisibility::Visible), Just(ListVisibility::Hidden)]
    }

    fn overlaps(a: Rect, b: Rect) -> bool {
        a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
    }

    fn contains(outer: Rect, inner: Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.x + inner.width <= outer.x + outer.width
            && inner.y + inner.height <= outer.y + outer.height
    }

    proptest! {
        /// `calculate` is public and reachable with any frame, including sizes
        /// below the too-small guard, so it must be total.
        #[test]
        fn layout_never_panics_and_stays_inside_the_frame(
            width in 0u16..300,
            height in 0u16..300,
            arrangement in arrangements(),
            mode in modes(),
            visibility in visibilities(),
            status_bar_height in 0u16..5,
        ) {
            let manager = LayoutManager { mode, arrangement, visibility };
            let area = Rect::new(0, 0, width, height);
            let areas = manager.calculate(area, status_bar_height);

            let mut rects = Vec::new();
            if let Some(list) = areas.service_list {
                rects.push(list);
            }
            rects.extend(areas.panes.iter().copied());
            if let Some(bar) = areas.status_bar {
                rects.push(bar);
            }

            for rect in &rects {
                prop_assert!(contains(area, *rect), "{rect:?} escapes {area:?}");
            }
            for (i, a) in rects.iter().enumerate() {
                for b in rects.iter().skip(i + 1) {
                    // Zero-area rects are degenerate, not overlapping.
                    if a.width == 0 || a.height == 0 || b.width == 0 || b.height == 0 {
                        continue;
                    }
                    prop_assert!(!overlaps(*a, *b), "{a:?} overlaps {b:?}");
                }
            }
        }

        /// The number of panes always matches the requested arrangement.
        #[test]
        fn pane_count_matches_the_arrangement(
            width in 1u16..300,
            height in 1u16..300,
            arrangement in arrangements(),
            mode in modes(),
        ) {
            let manager = LayoutManager {
                mode,
                arrangement,
                visibility: ListVisibility::Visible,
            };
            let areas = manager.calculate(Rect::new(0, 0, width, height), 1);
            let expected = match arrangement {
                PaneArrangement::None => 0,
                PaneArrangement::Single => 1,
                PaneArrangement::Double => 2,
            };
            prop_assert_eq!(areas.panes.len(), expected);
        }
    }
}
