mod backend;
mod bindings;
mod font;
pub mod socket_pty;
mod theme;
mod types;
mod view;

pub use backend::settings::BackendSettings;
pub use backend::{
    search_all_in_term, visible_regex_match_iter_in_cols, BackendCommand,
    DirectHandle, DirectWriter, GridBounds, PtyEvent, StreamHandle,
    TerminalBackend, TerminalMode,
};
pub use bindings::{Binding, BindingAction, InputKind, KeyboardBinding};
pub use font::{FontSettings, TerminalFont};
pub use theme::{ColorPalette, TerminalTheme};
pub use view::TerminalView;

// Re-export alacritty types needed for search
pub use alacritty_terminal::term::search::RegexSearch;
pub use alacritty_terminal::index::Point as TermPoint;
/// A search match is a range of terminal grid points.
pub type SearchMatch = std::ops::RangeInclusive<TermPoint>;

// Lower-level re-exports so callers can run a full-buffer search
// off-thread without needing to depend on `alacritty_terminal`
// directly. Used by Kubezilla's async search path.
pub use alacritty_terminal::index::{Column as TermColumn, Line as TermLine};
pub use alacritty_terminal::term::search::RegexIter;
pub use alacritty_terminal::index::Direction as TermDirection;
