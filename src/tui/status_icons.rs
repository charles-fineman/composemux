//! Status glyphs and the running throbber.
//!
//! Ported from nx `packages/nx/src/native/tui/status_icons.rs`
//! (MIT, (c) 2017-2026 Narwhal Technologies Inc.)

use crate::model::ServiceStatus;

/// Throbber animation characters for the running status.
pub const THROBBER_CHARS: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The bare status character, as used by the service-list rows.
pub fn status_char(status: ServiceStatus, throbber_counter: usize) -> char {
    match status {
        ServiceStatus::Success => '✔',
        ServiceStatus::Failure => '✖',
        ServiceStatus::Unhealthy => '⏭',
        ServiceStatus::Running => THROBBER_CHARS[throbber_counter % THROBBER_CHARS.len()],
        ServiceStatus::Stopped => '◼',
        ServiceStatus::NotStarted => '·',
    }
}
