/// Focus, pinning, key dispatch and the app state the whole UI hangs off.
pub mod app;
/// The individual widgets: service list, output panes, popups, status bar.
pub mod components;
/// The `/` filter over the service list.
pub mod filter;
/// What has focus, and the stack that popups push onto.
pub mod focus;
/// Where the list and panes go at a given terminal size.
pub mod layout_manager;
/// Draws one frame.
pub mod render;
/// Acceleration while a scroll key is held.
pub mod scroll_momentum;
/// The glyph and colour for each service state.
pub mod status_icons;
/// Raw mode, the alternate screen, and putting both back.
pub mod terminal;
/// Named ANSI colours, so the user's own palette shows through.
pub mod theme;
/// Shared formatting helpers.
pub mod utils;
