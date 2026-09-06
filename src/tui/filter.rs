#![allow(clippy::missing_docs_in_private_items)] // 11 left to document
//! The service-list filter opened with `/`.
//!
//! Ported from nx `packages/nx/src/native/tui/components/search_filter.rs`
//! (MIT, (c) 2017-2026 Narwhal Technologies Inc.)
//!
//! Three states, as in nx: off, being typed, and persisted. Persisting keeps
//! the filter applied but hands the keyboard back to the list, so `q` quits
//! again instead of typing a `q`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterState {
    Off,
    /// The user is typing; every printable key goes into the query.
    Editing,
    /// Confirmed with Enter: still applied, but no longer capturing keys.
    Persisted,
}

#[derive(Debug, Clone, Default)]
pub struct Filter {
    query: String,
    state: Option<FilterStateInner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterStateInner {
    Editing,
    Persisted,
}

impl Filter {
    pub fn state(&self) -> FilterState {
        match self.state {
            None => FilterState::Off,
            Some(FilterStateInner::Editing) => FilterState::Editing,
            Some(FilterStateInner::Persisted) => FilterState::Persisted,
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// True while keystrokes should be captured as query text.
    pub fn is_editing(&self) -> bool {
        self.state == Some(FilterStateInner::Editing)
    }

    /// True when the filter is narrowing the list, typed or persisted.
    pub fn is_active(&self) -> bool {
        self.state.is_some() && !self.query.is_empty()
    }

    /// Opens the filter, preserving any existing query for editing.
    pub fn enter_edit(&mut self) {
        self.state = Some(FilterStateInner::Editing);
    }

    /// Appends to the query. A no-op once persisted, matching nx: you must
    /// press `/` again to resume editing.
    pub fn push(&mut self, c: char) {
        if self.is_editing() {
            self.query.push(c);
        }
    }

    pub fn pop(&mut self) {
        if self.is_editing() {
            self.query.pop();
        }
    }

    /// Enter: keep the filter applied but stop capturing keys.
    pub fn persist(&mut self) {
        if self.state.is_some() {
            self.state = Some(FilterStateInner::Persisted);
        }
    }

    /// Esc: drop the filter entirely.
    pub fn clear(&mut self) {
        self.query.clear();
        self.state = None;
    }

    /// Case-insensitive substring match, as nx does. Not fuzzy.
    pub fn matches(&self, name: &str) -> bool {
        if !self.is_active() {
            return true;
        }
        name.to_lowercase().contains(&self.query.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_off_and_matches_everything() {
        let f = Filter::default();
        assert_eq!(f.state(), FilterState::Off);
        assert!(f.matches("anything"));
        assert!(!f.is_active());
    }

    #[test]
    fn typing_narrows_the_list() {
        let mut f = Filter::default();
        f.enter_edit();
        for c in "api".chars() {
            f.push(c);
        }
        assert_eq!(f.query(), "api");
        assert!(f.matches("api"));
        assert!(f.matches("legacy-api-gateway"));
        assert!(!f.matches("worker"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let mut f = Filter::default();
        f.enter_edit();
        f.push('A');
        f.push('P');
        f.push('I');
        assert!(f.matches("payments-api"));
    }

    #[test]
    fn a_slash_is_literal_text_so_paths_are_filterable() {
        let mut f = Filter::default();
        f.enter_edit();
        for c in "api/worker".chars() {
            f.push(c);
        }
        assert_eq!(f.query(), "api/worker");
        assert!(f.matches("api/worker-1"));
    }

    #[test]
    fn backspace_removes_a_character() {
        let mut f = Filter::default();
        f.enter_edit();
        f.push('a');
        f.push('b');
        f.pop();
        assert_eq!(f.query(), "a");
    }

    #[test]
    fn persisting_keeps_the_filter_but_stops_capturing_keys() {
        let mut f = Filter::default();
        f.enter_edit();
        f.push('a');
        f.persist();
        assert_eq!(f.state(), FilterState::Persisted);
        assert!(f.is_active(), "the filter should still be applied");
        assert!(!f.is_editing());
        // Further keys are ignored until editing is resumed.
        f.push('z');
        assert_eq!(f.query(), "a");
    }

    #[test]
    fn reopening_a_persisted_filter_resumes_editing_with_the_query_intact() {
        let mut f = Filter::default();
        f.enter_edit();
        f.push('a');
        f.persist();
        f.enter_edit();
        assert!(f.is_editing());
        f.push('b');
        assert_eq!(f.query(), "ab");
    }

    #[test]
    fn clearing_drops_the_query_and_the_state() {
        let mut f = Filter::default();
        f.enter_edit();
        f.push('a');
        f.persist();
        f.clear();
        assert_eq!(f.state(), FilterState::Off);
        assert_eq!(f.query(), "");
        assert!(f.matches("anything"));
    }

    #[test]
    fn an_empty_query_is_not_active_even_while_editing() {
        let mut f = Filter::default();
        f.enter_edit();
        assert!(!f.is_active());
        assert!(f.matches("anything"));
    }
}
