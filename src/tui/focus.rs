//! Which element has the keyboard.
//!
//! Ported from nx `packages/nx/src/native/tui/app.rs`
//! (MIT, (c) 2017-2026 Narwhal Technologies Inc.)
//!
//! Popups sit *over* a base layer rather than replacing it, so dismissing one
//! returns to whatever had focus before.

/// Maximum number of output panes, as in nx.
pub const MAX_PANES: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    ServiceList,
    Pane(usize),
    HelpPopup,
    CountdownPopup,
}

impl Focus {
    pub fn is_popup(self) -> bool {
        matches!(self, Focus::HelpPopup | Focus::CountdownPopup)
    }

    pub fn pane_index(self) -> Option<usize> {
        match self {
            Focus::Pane(i) => Some(i),
            _ => None,
        }
    }
}

/// A base layer with an optional popup above it.
#[derive(Debug, Clone, Copy)]
pub struct FocusStack {
    base: Focus,
    popup: Option<Focus>,
}

impl Default for FocusStack {
    fn default() -> Self {
        Self {
            base: Focus::ServiceList,
            popup: None,
        }
    }
}

impl FocusStack {
    /// What currently has the keyboard.
    pub fn current(&self) -> Focus {
        self.popup.unwrap_or(self.base)
    }

    /// The layer beneath any popup.
    pub fn base(&self) -> Focus {
        self.base
    }

    /// Replaces the base layer, leaving any popup in place.
    pub fn set_base(&mut self, focus: Focus) {
        debug_assert!(!focus.is_popup(), "popups belong on the popup layer");
        self.base = focus;
    }

    pub fn push_popup(&mut self, focus: Focus) {
        debug_assert!(focus.is_popup(), "only popups belong on the popup layer");
        self.popup = Some(focus);
    }

    pub fn close_popup(&mut self) {
        self.popup = None;
    }

    pub fn popup(&self) -> Option<Focus> {
        self.popup
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_on_the_service_list() {
        let f = FocusStack::default();
        assert_eq!(f.current(), Focus::ServiceList);
        assert!(f.popup().is_none());
    }

    #[test]
    fn a_popup_takes_focus_and_restores_the_base_on_close() {
        let mut f = FocusStack::default();
        f.set_base(Focus::Pane(1));
        f.push_popup(Focus::HelpPopup);
        assert_eq!(f.current(), Focus::HelpPopup);
        assert_eq!(f.base(), Focus::Pane(1));
        f.close_popup();
        assert_eq!(f.current(), Focus::Pane(1));
    }

    #[test]
    fn the_base_can_change_underneath_a_popup() {
        let mut f = FocusStack::default();
        f.push_popup(Focus::HelpPopup);
        f.set_base(Focus::Pane(0));
        assert_eq!(f.current(), Focus::HelpPopup);
        f.close_popup();
        assert_eq!(f.current(), Focus::Pane(0));
    }

    #[test]
    fn pane_index_is_only_reported_for_panes() {
        assert_eq!(Focus::Pane(1).pane_index(), Some(1));
        assert_eq!(Focus::ServiceList.pane_index(), None);
        assert!(Focus::HelpPopup.is_popup());
        assert!(!Focus::ServiceList.is_popup());
    }
}
