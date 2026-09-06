#![allow(clippy::missing_docs_in_private_items)] // 13 left to document
//! Accelerating scroll, shared by the keyboard and the mouse wheel.
//!
//! Ported from nx `packages/nx/src/native/tui/scroll_momentum.rs`
//! (MIT, (c) 2017-2026 Narwhal Technologies Inc.)
//!
//! Holding a scroll key accelerates: each event inside the momentum window
//! multiplies the step, so short taps move a line at a time while a held key
//! covers ground quickly.

use std::time::{Duration, Instant};

/// How long after an event momentum can still build on it.
const MOMENTUM_TIMEOUT: Duration = Duration::from_millis(200);
const ACCELERATION_FACTOR: f32 = 1.2;
const INITIAL_MOMENTUM: f32 = 1.0;
/// Events arriving faster than this are dropped, so a key-repeat storm or a
/// high-resolution wheel cannot outrun the renderer.
const IGNORE_EVENTS_UNDER: Duration = Duration::from_millis(50);
/// Events at the ignore threshold needed before the cap is raised.
const SUSTAINED_SCROLL_THRESHOLD: u32 = 2_000 / 50;
const MOMENTUM_CAP: f32 = 25.0;
const SUSTAINED_MOMENTUM_CAP: f32 = 100.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
}

#[derive(Debug)]
pub struct ScrollMomentum {
    momentum: f32,
    last_event: Option<Instant>,
    last_direction: Option<ScrollDirection>,
    scroll_count: u32,
}

impl Default for ScrollMomentum {
    fn default() -> Self {
        Self {
            momentum: INITIAL_MOMENTUM,
            last_event: None,
            last_direction: None,
            scroll_count: 0,
        }
    }
}

impl ScrollMomentum {
    /// Lines to scroll for an event at `now`, or 0 if it should be dropped.
    pub fn scroll(&mut self, direction: ScrollDirection, now: Instant) -> u16 {
        if self.last_direction != Some(direction) {
            self.reset_to(direction, now);
            return self.momentum.round() as u16;
        }

        let elapsed = self.last_event.map(|t| now.duration_since(t));
        match elapsed {
            // Too fast: drop the event entirely without disturbing momentum.
            Some(e) if e < IGNORE_EVENTS_UNDER => {
                self.scroll_count = self.scroll_count.saturating_add(1);
                0
            }
            // Within the window: accelerate.
            Some(e) if e < MOMENTUM_TIMEOUT => {
                self.last_event = Some(now);
                self.scroll_count = self.scroll_count.saturating_add(1);
                let cap = if self.scroll_count > SUSTAINED_SCROLL_THRESHOLD {
                    SUSTAINED_MOMENTUM_CAP
                } else {
                    MOMENTUM_CAP
                };
                self.momentum = (self.momentum * ACCELERATION_FACTOR).min(cap);
                self.momentum.round() as u16
            }
            // Window elapsed, or the first event: start over.
            _ => {
                self.reset_to(direction, now);
                self.momentum.round() as u16
            }
        }
    }

    fn reset_to(&mut self, direction: ScrollDirection, now: Instant) {
        self.momentum = INITIAL_MOMENTUM;
        self.last_direction = Some(direction);
        self.last_event = Some(now);
        self.scroll_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn first_event_moves_one_line() {
        let t0 = Instant::now();
        let mut m = ScrollMomentum::default();
        assert_eq!(m.scroll(ScrollDirection::Down, t0), 1);
    }

    #[test]
    fn events_inside_the_window_accelerate() {
        let t0 = Instant::now();
        let mut m = ScrollMomentum::default();
        m.scroll(ScrollDirection::Down, t0);
        // 1.0 * 1.2 = 1.2 -> 1, then 1.44 -> 1, 1.728 -> 2
        let steps: Vec<u16> = (1..=3)
            .map(|i| m.scroll(ScrollDirection::Down, at(t0, 100 * i)))
            .collect();
        assert_eq!(steps, vec![1, 1, 2]);
    }

    #[test]
    fn events_under_fifty_ms_are_dropped() {
        let t0 = Instant::now();
        let mut m = ScrollMomentum::default();
        m.scroll(ScrollDirection::Down, t0);
        assert_eq!(m.scroll(ScrollDirection::Down, at(t0, 10)), 0);
        assert_eq!(m.scroll(ScrollDirection::Down, at(t0, 20)), 0);
    }

    #[test]
    fn pausing_past_the_window_resets_momentum() {
        let t0 = Instant::now();
        let mut m = ScrollMomentum::default();
        for i in 0..10 {
            m.scroll(ScrollDirection::Down, at(t0, 100 * i));
        }
        // A pause longer than the momentum window drops back to a single line.
        assert_eq!(m.scroll(ScrollDirection::Down, at(t0, 5_000)), 1);
    }

    #[test]
    fn changing_direction_resets_momentum() {
        let t0 = Instant::now();
        let mut m = ScrollMomentum::default();
        for i in 0..10 {
            m.scroll(ScrollDirection::Down, at(t0, 100 * i));
        }
        assert_eq!(m.scroll(ScrollDirection::Up, at(t0, 1_100)), 1);
    }

    #[test]
    fn momentum_is_capped() {
        let t0 = Instant::now();
        let mut m = ScrollMomentum::default();
        // Sustained scrolling at 100ms never crosses the sustained threshold,
        // because only sub-50ms events increment the count that fast.
        let mut last = 0;
        for i in 0..200 {
            last = m.scroll(ScrollDirection::Down, at(t0, 100 * i));
        }
        assert!(last <= SUSTAINED_MOMENTUM_CAP as u16, "got {last}");
        assert!(
            last >= MOMENTUM_CAP as u16,
            "expected acceleration, got {last}"
        );
    }
}
