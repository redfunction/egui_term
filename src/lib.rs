mod backend;
mod bindings;
mod font;
pub mod socket_pty;
mod theme;
mod types;
mod view;

pub use backend::settings::BackendSettings;
pub use backend::{BackendCommand, DirectHandle, DirectWriter, PtyEvent, StreamHandle, TerminalBackend, TerminalMode};
pub use bindings::{Binding, BindingAction, InputKind, KeyboardBinding};
pub use font::{FontSettings, TerminalFont};
pub use theme::{ColorPalette, TerminalTheme};
pub use view::TerminalView;

// Re-export alacritty types needed for search
pub use alacritty_terminal::term::search::RegexSearch;
pub use alacritty_terminal::index::Point as TermPoint;
/// A search match is a range of terminal grid points.
pub type SearchMatch = std::ops::RangeInclusive<TermPoint>;
