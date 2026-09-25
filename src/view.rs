use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Point as TerminalGridPoint;
use alacritty_terminal::term::cell;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use egui::epaint::RectShape;
use egui::Modifiers;
use egui::MouseWheelUnit;
use egui::Shape;
use egui::Widget;
use egui::{Align2, Painter, Pos2, Rect, Response, Stroke, Vec2};
use egui::{CornerRadius, Key};
use egui::{Id, PointerButton};

use crate::backend::BackendCommand;
use crate::backend::TerminalBackend;
use crate::backend::{LinkAction, MouseButton, SelectionType};
use alacritty_terminal::term::search::RegexSearch;
use crate::bindings::Binding;
use crate::bindings::{BindingAction, BindingsLayout, InputKind};
use crate::font::TerminalFont;
use crate::theme::TerminalTheme;
use crate::types::Size;

const EGUI_TERM_WIDGET_ID_PREFIX: &str = "egui_term::instance::";

#[derive(Clone, Copy, PartialEq)]
enum HighlightKind {
    None,
    Match,
    Current,
}

#[derive(Debug, Clone)]
enum InputAction {
    BackendCall(BackendCommand),
    WriteToClipboard(String),
    Ignore,
}

#[derive(Clone, Default)]
pub struct TerminalViewState {
    is_dragged: bool,
    scroll_pixels: f32,
    /// Modifiers of the wheel event(s) that filled `scroll_pixels`.
    scroll_modifiers: Modifiers,
    current_mouse_position_on_grid: TerminalGridPoint,
    scrollbar_dragging: bool,
    /// Y offset from click point to thumb top, so thumb doesn't snap on grab
    scrollbar_grab_offset: f32,
    /// Live position of the user's selected ("current") search
    /// match, tracked across frames. Each render picks the visible
    /// match closest to this Point and *updates* this Point to the
    /// chosen match's start — so as streaming content shifts grid
    /// Points by ±1 line, the orange highlight follows the same
    /// physical match instead of flickering to whichever match
    /// happens to be closest to a stale anchor. Reset whenever the
    /// caller's `current_match_start` differs from this (i.e. user
    /// pressed F3 and explicitly chose a new match).
    tracked_current: Option<TerminalGridPoint>,
    /// Last `current_match_start` seen from the caller. Used to
    /// detect "user navigated" so we know to reset tracking.
    last_caller_current: Option<TerminalGridPoint>,
    cached_shapes: Option<Vec<Shape>>,
    cached_rect: Option<Rect>,
    /// Caller-supplied search state hash that produced the cached
    /// shapes. When this differs from the next frame's hash the
    /// cache is treated as stale (e.g. user typed a new query).
    cached_search_key: u64,
    /// Visible viewport rect for the cached frame. The scrollbar
    /// position is rendered relative to this rect, so when the
    /// window resizes (or horizontal pan changes) we must rebuild
    /// the cache even if `cached_rect` (the grid's allocated rect)
    /// hasn't moved.
    cached_visible: Option<Rect>,
    /// Horizontal column offset of the cached frame. Wrap-off
    /// mode pans the visible column band of a wide grid via
    /// `set_horizontal_offset_cols`; cache must invalidate the
    /// instant the user drags the scrollbar so the new column
    /// slice paints immediately rather than waiting on the
    /// `last_render_at` throttle.
    cached_h_offset: usize,
    /// Caller-supplied "current" match anchor for the cached frame.
    /// F3 navigation only mutates this Point — the search query and
    /// flags are unchanged, so `cached_search_key` matches and fast
    /// path #1 would otherwise reuse stale shapes with the orange
    /// highlight on the previous match. Including this Point in the
    /// cache key invalidates immediately on F3, so the next frame
    /// repaints orange on the new match.
    cached_current_match: Option<TerminalGridPoint>,
    /// Last time we built a fresh shape list. Used to cap render
    /// frequency on viewports where new content arrives constantly
    /// (multi-pod log streams) — `is_dirty()` is essentially always
    /// true in those cases, so we use a short-window cache as the
    /// real throttle.
    last_render_at: Option<std::time::Instant>,
    /// Whether the last render included search highlights (to invalidate cache on change).
    had_highlights: bool,
    /// Grep/filter mode scroll position: number of *filtered* lines
    /// scrolled up from the bottom of the filtered list. 0 = follow
    /// the bottom (sticky for streaming logs). The raw grid's
    /// `display_offset` is deliberately never touched in filter
    /// mode, so toggling the filter off restores the exact prior
    /// raw view. Clamped against the filtered total each render.
    filter_offset: usize,
    /// Whether the cached frame was rendered with the filter on.
    cached_filter_active: bool,
    /// `filter_offset` of the cached frame — wheel/scrollbar moves
    /// in filter mode don't set `is_dirty`, so the offset must be
    /// part of the cache key to repaint on scroll.
    cached_filter_offset: usize,
    /// Scroll position of the cached frame.
    ///
    /// Scrolling sets `is_dirty` like any other change, which put it
    /// behind the 33ms rebuild throttle below — a throttle meant for
    /// streaming logs, where the dirty bit is set on every batch
    /// flush. A wheel spins faster than 30fps, so the view advanced
    /// in uneven two- and four-line jumps instead of a step per
    /// notch. Measured off a 60fps capture: 39 visual updates for 60
    /// notches. Making the offset part of the cache key takes a
    /// scroll off that throttle — it is a new frame, not a redraw of
    /// the same one.
    cached_display_offset: usize,
    /// The grid lines (raw `Line.0` values, top to bottom) rendered
    /// by the last grep/filter frame. Input handling maps pointer
    /// pixels through this to translate a click on packed screen
    /// row N into the real grid line it shows, so selection works
    /// under the remap. Only meaningful while the filter is active
    /// (one frame behind `filter_rows`, which is fine — it always
    /// describes what's actually on screen).
    filter_rows_grid: Vec<i32>,
}

pub struct TerminalView<'a> {
    widget_id: Id,
    has_focus: bool,
    size: Vec2,
    backend: &'a mut TerminalBackend,
    font: TerminalFont,
    theme: TerminalTheme,
    bindings_layout: BindingsLayout,
    /// Regex for search highlighting (searched on visible area each frame).
    search_regex: Option<RegexSearch>,
    /// The absolute point of the "current" match start (highlighted differently).
    current_match_start: Option<TerminalGridPoint>,
    /// When true, keyboard input is not sent to the terminal (log/read-only mode).
    read_only: bool,
    /// When true, the cursor block is not drawn.
    hide_cursor: bool,
    /// Optional override for the alacritty grid's column count.
    /// When set, the widget keeps its rendered allocation at
    /// `self.size` (viewport width) but resizes the internal grid
    /// to this many columns — used by Kubezilla's "no-wrap" log
    /// view to render long lines without wrapping. Combine with
    /// `set_horizontal_offset_cols` to scroll horizontally without
    /// an outer `ScrollArea`.
    grid_columns_override: Option<usize>,
    /// Column offset applied when rendering. Cells with column
    /// `< offset` or `>= offset + visible_cols` are skipped; the
    /// remaining cells are drawn at `(col - offset) × cell_width`,
    /// so the rendered output looks like a horizontally-panned
    /// view of the wide grid.
    horizontal_offset_cols: usize,
    /// Opaque caller-supplied hash of the current search state
    /// (query + flags). Included in the render cache key so the
    /// cache invalidates the moment the user changes their search
    /// — without this, fast-path #1 can't fire while search is
    /// active because the widget can't tell whether the matches
    /// it cached are still correct. Defaults to 0 (no search).
    search_key: u64,
    /// Grep/filter mode: when `Some`, only these raw grid lines
    /// (`Line.0` values, sorted ascending) are rendered, packed
    /// consecutively from the top like `grep` output. Typically
    /// derived from a full-buffer `search_all_in_term` run by the
    /// embedder; the view merges in a fresh scan of the bottom
    /// `screen_lines + 5` rows each frame so a streaming tail stays
    /// current. `Some(vec![])` is valid (query matched nothing →
    /// blank grid). `None` = normal rendering, byte-identical to
    /// the pre-filter code path.
    line_filter: Option<std::sync::Arc<Vec<i32>>>,
}

impl Widget for TerminalView<'_> {
    fn ui(self, ui: &mut egui::Ui) -> Response {
        let (layout, painter) =
            ui.allocate_painter(self.size, egui::Sense::click());

        let widget_id = self.widget_id;
        let mut state = ui.memory(|m| {
            m.data
                .get_temp::<TerminalViewState>(widget_id)
                .unwrap_or_default()
        });

        // Capture the clip rect *now*, while we still have a `&Ui`.
        // When the widget is inside a `ScrollArea` (Kubezilla's no-
        // wrap log mode), `layout.rect` extends beyond the visible
        // viewport — but the clip rect is exactly the visible
        // window. We pin the vertical scrollbar to the *clip's*
        // right edge so it stays on-screen no matter how the user
        // pans horizontally.
        let visible_rect = ui.clip_rect().intersect(layout.rect);

        self.focus(&layout)
            .resize(&layout)
            .process_input(&layout, &visible_rect, &mut state)
            .show(&mut state, &layout, &visible_rect, &painter);

        ui.memory_mut(|m| m.data.insert_temp(widget_id, state));
        layout
    }
}

impl<'a> TerminalView<'a> {
    pub fn new(ui: &mut egui::Ui, backend: &'a mut TerminalBackend) -> Self {
        let widget_id = ui.make_persistent_id(format!(
            "{}{}",
            EGUI_TERM_WIDGET_ID_PREFIX,
            backend.id()
        ));

        Self {
            widget_id,
            has_focus: false,
            size: ui.available_size(),
            backend,
            font: TerminalFont::default(),
            theme: TerminalTheme::default(),
            bindings_layout: BindingsLayout::new(),
            search_regex: None,
            current_match_start: None,
            read_only: false,
            hide_cursor: false,
            grid_columns_override: None,
            horizontal_offset_cols: 0,
            search_key: 0,
            line_filter: None,
        }
    }

    /// Caller-supplied hash of the current search state. Used as
    /// part of the render cache key so the cache invalidates when
    /// the user types a new query, toggles case sensitivity, etc.
    /// Pass 0 when no search is active. The widget never inspects
    /// the value beyond `==` comparison with the cached one.
    #[inline]
    pub fn set_search_key(mut self, key: u64) -> Self {
        self.search_key = key;
        self
    }

    /// Override the alacritty grid's column count — the rendered
    /// widget still occupies only `self.size`, but internally the
    /// terminal can hold lines longer than the viewport. Pair with
    /// `set_horizontal_offset_cols` to pan a window across the
    /// wider grid without using an outer `ScrollArea`.
    #[inline]
    pub fn set_grid_columns(mut self, cols: Option<usize>) -> Self {
        self.grid_columns_override = cols;
        self
    }

    /// Render starting at this column. Cells in `[offset, offset +
    /// visible_cols)` are drawn at `(col - offset) * cell_width`;
    /// other cells are skipped. Used together with
    /// `set_grid_columns` for a no-wrap log view.
    #[inline]
    pub fn set_horizontal_offset_cols(mut self, offset: usize) -> Self {
        self.horizontal_offset_cols = offset;
        self
    }

    #[inline]
    pub fn set_theme(mut self, theme: TerminalTheme) -> Self {
        self.theme = theme;
        self
    }

    #[inline]
    pub fn set_font(mut self, font: TerminalFont) -> Self {
        self.font = font;
        self
    }

    #[inline]
    pub fn set_focus(mut self, has_focus: bool) -> Self {
        self.has_focus = has_focus;
        self
    }

    #[inline]
    pub fn set_size(mut self, size: Vec2) -> Self {
        self.size = size;
        self
    }

    #[inline]
    pub fn add_bindings(
        mut self,
        bindings: Vec<(Binding<InputKind>, BindingAction)>,
    ) -> Self {
        self.bindings_layout.add_bindings(bindings);
        self
    }

    /// Set a search regex for highlighting visible matches each frame.
    #[inline]
    pub fn set_search(mut self, regex: Option<RegexSearch>) -> Self {
        self.search_regex = regex;
        self
    }

    /// Set the start point of the "current" match (highlighted in a different color).
    #[inline]
    pub fn set_current_match(mut self, point: Option<TerminalGridPoint>) -> Self {
        self.current_match_start = point;
        self
    }

    /// Grep/filter mode. `Some(lines)` renders only these raw grid
    /// lines (sorted `Line.0` values), packed consecutively — rows
    /// not in the list are hidden, like `grep`. The list normally
    /// comes from the embedder's `search_all_in_term` results
    /// (every visual row a match touches; continuation rows of a
    /// wrapped logical line that the match doesn't touch are not
    /// included). The view keeps the bottom `screen_lines + 5` rows
    /// fresh itself via `tail_matching_lines` using `search_regex`.
    /// `Some(vec![])` renders a blank grid; `None` disables the
    /// filter (normal rendering). Link hover, mouse reports and the
    /// cursor are suppressed while active; selection works via a
    /// pointer remap, and the raw grid scroll position is left
    /// untouched.
    ///
    /// Cache contract: the widget's shape cache does NOT hash this
    /// list — the caller must fold the list's identity (e.g. a
    /// generation counter bumped on every recompute) into
    /// `set_search_key`, and must recompute the list after any grid
    /// reflow (resize or column-override change shifts every line
    /// index).
    #[inline]
    pub fn set_line_filter(
        mut self,
        lines: Option<std::sync::Arc<Vec<i32>>>,
    ) -> Self {
        self.line_filter = lines;
        self
    }

    #[inline]
    pub fn set_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    #[inline]
    pub fn set_hide_cursor(mut self, hide: bool) -> Self {
        self.hide_cursor = hide;
        self
    }

    fn focus(self, layout: &Response) -> Self {
        if self.has_focus {
            // Only claim focus when no *other* widget already holds it —
            // otherwise the terminal yanks focus away from e.g. an open search
            // box on every frame. When focus is free (or already ours) we take
            // it so keystrokes reach the PTY.
            let taken_by_other = layout
                .ctx
                .memory(|m| m.focused().is_some_and(|f| f != layout.id));
            if !taken_by_other {
                layout.request_focus();
            }
        } else {
            layout.surrender_focus();
        }

        self
    }

    fn resize(self, layout: &Response) -> Self {
        let font_size = self.font.font_measure(&layout.ctx);
        // When the caller has overridden the grid column count,
        // resize the alacritty grid to that wider width. The
        // rendered widget still occupies `layout.rect.size()` but
        // internally the grid can hold longer lines so the no-wrap
        // log view doesn't fold them.
        let logical_size = if let Some(cols) = self.grid_columns_override {
            let cell_w = font_size.width;
            let grid_w = (cols as f32 * cell_w).max(layout.rect.size().x);
            egui::vec2(grid_w, layout.rect.size().y)
        } else {
            layout.rect.size()
        };
        self.backend.process_command(BackendCommand::Resize(
            Size::from(logical_size),
            font_size,
        ));

        self
    }

    fn process_input(
        self,
        layout: &Response,
        visible_rect: &egui::Rect,
        state: &mut TerminalViewState,
    ) -> Self {
        let has_focus = layout.has_focus();
        let has_pointer = layout.contains_pointer();

        if !has_focus && !has_pointer {
            return self;
        }

        // Stop egui's focus system from stealing Tab / arrow keys /
        // Escape away from the terminal when it has focus. Without
        // this, pressing Tab moves focus to the next widget (e.g. a
        // toolbar button highlighting briefly) instead of being
        // delivered to the PTY, and arrow keys do the same via
        // egui's directional focus navigation. The widget still
        // receives these as ordinary Key events. Note: we lock on
        // `layout.id` (the Response id used by request_focus) — using
        // the persistent widget id here is silently ignored.
        if has_focus {
            layout.ctx.memory_mut(|m| {
                m.set_focus_lock_filter(
                    layout.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                );
            });
        }

        // Scrollbar occupies the rightmost 8px of the *visible*
        // viewport (the clip rect intersected with our allocation).
        // When the widget extends past the visible area inside a
        // ScrollArea, this keeps the scrollbar pinned where the
        // user can actually see and click it.
        let scrollbar_x = visible_rect.max.x - 8.0;

        let modifiers = layout.ctx.input(|i| i.modifiers);
        let events = layout.ctx.input(|i| i.events.clone());
        // Grep/filter mode: the pixel→grid-point mapping is wrong
        // under the line remap, so selection, mouse reports and
        // link hover are suppressed, and the wheel scrolls the
        // filtered window instead of the raw grid.
        let filter_active = self.line_filter.is_some();
        for event in events {
            let mut input_actions = vec![];

            match event {
                // Keyboard events: require focus only; skip in read-only mode
                egui::Event::Text(_)
                | egui::Event::Key { .. }
                | egui::Event::Copy
                | egui::Event::Paste(_) => {
                    if !has_focus || self.read_only {
                        continue;
                    }
                    // Grep mode: the copy chord must honor the
                    // filtered rows — the raw selection range spans
                    // hidden lines the user never saw. Plain ^C
                    // (no shift) still falls through to the PTY.
                    let filter_copy = filter_active
                        && matches!(event, egui::Event::Copy)
                        && (cfg!(any(target_os = "ios", target_os = "macos"))
                            || modifiers
                                .contains(Modifiers::COMMAND | Modifiers::SHIFT));
                    if filter_copy {
                        let lines = self
                            .line_filter
                            .as_ref()
                            .map(|v| v.as_slice())
                            .unwrap_or(&[]);
                        input_actions.push(InputAction::WriteToClipboard(
                            self.backend.selectable_content_filtered(lines),
                        ));
                    } else {
                        input_actions.push(process_keyboard_event(
                            event,
                            self.backend,
                            &self.bindings_layout,
                            modifiers,
                        ))
                    }
                },
                // Mouse wheel: require pointer over widget
                egui::Event::MouseWheel {
                    unit,
                    delta,
                    modifiers: wheel_modifiers,
                } => {
                    if !has_pointer {
                        continue;
                    }
                    // Ctrl/Cmd+wheel is the embedder's zoom binding. egui
                    // reports it as `zoom_delta` and leaves the event in
                    // place, so without this a notch would zoom *and*
                    // scroll — or, under mouse tracking, reach the program
                    // as a Ctrl-wheel report. A binding wins over both the
                    // grid and the program, as in alacritty. Same predicate
                    // egui uses for the zoom (`matches_any(COMMAND)`, i.e.
                    // ctrl or cmd on every platform), so the two agree.
                    if wheel_modifiers.matches_any(Modifiers::COMMAND) {
                        continue;
                    }
                    // Reports are paid out over several frames; carry the
                    // modifiers the notch was made with, not whatever is
                    // held when a later frame spends the remainder.
                    state.scroll_modifiers = wheel_modifiers;
                    let tracking = self
                        .backend
                        .last_content()
                        .terminal_mode
                        .intersects(TermMode::MOUSE_MODE);
                    // A wheel event carries no position, and the grid
                    // point is otherwise refreshed only on PointerMoved:
                    // it is (0, 0) until the pointer first moves inside
                    // this widget. A program that routes the wheel by
                    // cell (tmux panes, k9s split views) would scroll the
                    // wrong pane, so take it from the hover position now.
                    // Skipped under the grep/filter remap, where the
                    // pixel→grid mapping does not hold (see PointerMoved).
                    if !filter_active {
                        if let Some(pos) = layout.hover_pos() {
                            let content = self.backend.last_content();
                            state.current_mouse_position_on_grid =
                                TerminalBackend::selection_point(
                                    pos.x - layout.rect.min.x,
                                    pos.y - layout.rect.min.y,
                                    &content.terminal_size,
                                    content.display_offset,
                                );
                        }
                    }
                    // Under mouse tracking a notch is relayed to the
                    // program one report per row, so it is worth one row
                    // (alacritty forces its multiplier to 1 there) and the
                    // program applies its own scroll speed. This snapshot
                    // is only a hint; the routing itself reads the live
                    // mode in the backend.
                    let rows_per_notch = if tracking && !filter_active {
                        1.0
                    } else {
                        ROWS_PER_NOTCH
                    };
                    // Bank the distance; the frame loop below spends it.
                    // Nothing is emitted here, so several events landing in
                    // one frame cannot stack into a single jump.
                    bank_wheel_pixels(
                        state,
                        self.font.font_type().size,
                        unit,
                        delta,
                        rows_per_notch,
                    );
                },
                // Mouse button: require pointer over widget (or dragging for release)
                egui::Event::PointerButton {
                    button,
                    pressed,
                    modifiers,
                    pos,
                    ..
                } => {
                    if !has_pointer && !(state.is_dragged && !pressed) {
                        continue;
                    }
                    // Skip if clicking in scrollbar area or dragging scrollbar
                    if pos.x >= scrollbar_x || state.scrollbar_dragging {
                        continue;
                    }
                    // Grep mode: drive alacritty's selection with
                    // explicit grid points mapped through the
                    // remapped rows. Mouse reports and link opens
                    // stay disabled (their coordinates are raw-grid
                    // based and would target hidden rows).
                    if filter_active {
                        if button == PointerButton::Primary {
                            if pressed {
                                let content = self.backend.last_content();
                                if let Some((point, side)) = filter_selection_point(
                                    state,
                                    layout,
                                    &content.terminal_size,
                                    self.horizontal_offset_cols,
                                    pos,
                                ) {
                                    state.is_dragged = true;
                                    let selection_type = if layout.double_clicked() {
                                        SelectionType::Semantic
                                    } else if layout.triple_clicked() {
                                        SelectionType::Lines
                                    } else {
                                        SelectionType::Simple
                                    };
                                    input_actions.push(InputAction::BackendCall(
                                        BackendCommand::SelectStartAtPoint(
                                            selection_type,
                                            point,
                                            side,
                                        ),
                                    ));
                                }
                            } else {
                                state.is_dragged = false;
                            }
                        }
                    } else {
                        input_actions.push(process_button_click(
                            state,
                            layout,
                            self.backend,
                            &self.bindings_layout,
                            button,
                            pos,
                            &modifiers,
                            pressed,
                        ))
                    }
                },
                // Mouse move: require pointer over widget
                egui::Event::PointerMoved(pos) => {
                    if !has_pointer && !state.is_dragged {
                        continue;
                    }
                    if state.scrollbar_dragging || pos.x >= scrollbar_x {
                        continue;
                    }
                    // Grep mode: only selection-drag updates, mapped
                    // through the remapped rows. Link hover and
                    // mouse reports stay disabled.
                    if filter_active {
                        if state.is_dragged {
                            let content = self.backend.last_content();
                            if let Some((point, side)) = filter_selection_point(
                                state,
                                layout,
                                &content.terminal_size,
                                self.horizontal_offset_cols,
                                pos,
                            ) {
                                input_actions.push(InputAction::BackendCall(
                                    BackendCommand::SelectUpdateAtPoint(point, side),
                                ));
                            }
                        }
                    } else {
                        input_actions = process_mouse_move(
                            state,
                            layout,
                            self.backend,
                            pos,
                            &modifiers,
                        )
                    }
                },
                _ => {},
            };

            for action in input_actions {
                match action {
                    InputAction::BackendCall(cmd) => {
                        self.backend.process_command(cmd);
                    },
                    InputAction::WriteToClipboard(data) => {
                        layout.ctx.copy_text(data);
                    },
                    InputAction::Ignore => {},
                }
            }
        }

        // Spend the bank, a few rows per FRAME rather than all of a notch at
        // once, and keep the frames coming until it is empty. A grid can only
        // move by whole rows, so a notch used to land as one jump; next to a
        // browser, which animates the same distance across many pixel steps,
        // that reads as stepping rather than scrolling.
        let cell = self.font.font_type().size;
        if state.scroll_pixels.abs() >= cell {
            let lines = drain_banked_scroll(state, cell);
            if lines != 0 {
                if self.line_filter.is_some() {
                    state.filter_offset =
                        state.filter_offset.saturating_add_signed(lines as isize);
                } else {
                    // Not `Scroll`: only the wheel may become mouse
                    // reports or alternate-scroll arrow keys.
                    self.backend.process_command(BackendCommand::WheelScroll(
                        lines,
                        state.scroll_modifiers,
                        state.current_mouse_position_on_grid,
                    ));
                }
            }
            layout.ctx.request_repaint();
        } else if state.scroll_pixels != 0.0 {
            // Less than a row left over: keep it for the next notch rather
            // than rounding it away, so slow trackpad travel still adds up.
            state.scroll_pixels %= cell.max(1.0);
        }

        self
    }

    fn show(
        mut self,
        state: &mut TerminalViewState,
        layout: &Response,
        visible_rect: &egui::Rect,
        painter: &Painter,
    ) {
        let _has_search = self.search_regex.is_some();

        // Scrollbar lives at the right edge of the visible viewport
        // (so it stays on-screen even when the grid is wider than
        // the ScrollArea's clip rect).
        let scrollbar_x = visible_rect.max.x - 8.0;
        let pointer_on_scrollbar = layout.ctx.input(|i| {
            if let Some(pos) = i.pointer.hover_pos() {
                (i.pointer.primary_pressed() || i.pointer.primary_down())
                    && pos.x >= scrollbar_x
                    && visible_rect.contains(pos)
            } else {
                false
            }
        });

        // Fast path #1: nothing changed since last frame — reuse
        // the cached shapes verbatim. Cache key is BOTH the grid
        // rect (so resize triggers rebuild) AND the visible rect
        // (so window resize / horizontal pan also rebuilds — the
        // scrollbar position is computed relative to `visible_rect`
        // and would otherwise stick at the previous viewport edge).
        // Rounded to integer pixels because egui's `ScrollArea`
        // clip rect drifts by sub-pixel amounts every frame on
        // some layouts; an exact `Rect == Rect` comparison would
        // miss the cache forever and force a full render per
        // frame in no-wrap mode.
        // Read from the grid, not from `last_content`: that only moves
        // when we sync, and the whole point is to decide whether we
        // may skip the sync.
        let display_offset = self.backend.display_offset();
        let key_layout = round_rect_int(layout.rect);
        let key_visible = round_rect_int(*visible_rect);
        let cache_key_matches = state.cached_rect == Some(key_layout)
            && state.cached_visible == Some(key_visible)
            && state.cached_h_offset == self.horizontal_offset_cols
            && state.cached_search_key == self.search_key
            && state.cached_current_match == self.current_match_start
            // Filter-mode inputs: wheel/scrollbar moves mutate
            // `filter_offset` without setting `is_dirty`, so the
            // offset (and the mode flag itself) must invalidate
            // the shape cache directly.
            && state.cached_filter_active == self.line_filter.is_some()
            && state.cached_filter_offset == state.filter_offset
            // A scroll is a different frame, not a stale one: it must
            // miss BOTH fast paths, the throttle included.
            && state.cached_display_offset == display_offset;

        // Fast path #1: nothing meaningful has changed since the
        // cached frame — same buffer (`!is_dirty`), same layout,
        // same horizontal offset, same search state. Reuse the
        // shapes verbatim. Now fires even when a search is active,
        // so an idle viewport with highlights doesn't pay the per-
        // frame `visible_regex_match_iter` + BTreeSet build.
        if !self.backend.is_dirty()
            && !state.scrollbar_dragging
            && !pointer_on_scrollbar
            && cache_key_matches
        {
            if let Some(ref shapes) = state.cached_shapes {
                painter.extend(shapes.clone());
                return;
            }
        }

        // Fast path #2: the buffer is dirty, but we rendered very
        // recently. Re-using the cached shapes for one more frame
        // caps effective render rate on streaming logs where 100+
        // pods set `is_dirty` on every batch flush. The actual
        // contents are at most ~33 ms behind — imperceptible — and
        // we save a full grid scan, lock acquisition, and shape
        // rebuild. Schedule a wake so we don't fall idle behind a
        // dirty bit that'll keep firing.
        // 50ms, matching the embedder's own terminal clock. Every one of these
        // wakes repaints the whole surface the terminal sits on — a pod table,
        // a resource tree — so the two clocks running at different rates just
        // meant the faster one paid for both. 20fps is well past what reading
        // streamed text needs.
        const RENDER_THROTTLE: std::time::Duration =
            std::time::Duration::from_millis(50);
        let recently_rendered = state
            .last_render_at
            .map(|t| t.elapsed() < RENDER_THROTTLE)
            .unwrap_or(false);
        if recently_rendered
            && cache_key_matches
            && !state.scrollbar_dragging
            && !pointer_on_scrollbar
        {
            if let Some(ref shapes) = state.cached_shapes {
                painter.extend(shapes.clone());
                layout.ctx.request_repaint_after(RENDER_THROTTLE);
                return;
            }
        }

        let term_arc = self.backend.term().clone();
        let mut terminal = term_arc.lock();
        self.backend.sync_with_term(&mut terminal);
        let content = self.backend.last_content();

        let layout_min = layout.rect.min;
        let layout_max = layout.rect.max;
        let cell_height = content.terminal_size.cell_height as f32;
        let cell_width = content.terminal_size.cell_width as f32;
        let global_bg =
            self.theme.get_color(Color::Named(NamedColor::Background));
        // Subtle selection tint (22% foreground over background) so
        // selecting empty cells doesn't paint solid inverted blocks.
        let global_fg =
            self.theme.get_color(Color::Named(NamedColor::Foreground));
        let selection_bg = egui::Color32::from_rgb(
            (global_bg.r() as f32 * 0.78 + global_fg.r() as f32 * 0.22) as u8,
            (global_bg.g() as f32 * 0.78 + global_fg.g() as f32 * 0.22) as u8,
            (global_bg.b() as f32 * 0.78 + global_fg.b() as f32 * 0.22) as u8,
        );
        let display_offset = content.display_offset;
        let cursor_point = content.cursor_point;

        // Grep/filter mode: build the ordered list of grid lines to
        // render (the "row map"). `None` leaves the entire render on
        // the existing identity path. The embedder-supplied list
        // covers the scrollback (refreshed asynchronously, so it may
        // be up to ~1 s stale); the bottom `screen_lines + 5` band is
        // rescanned fresh right here so a streaming tail shows new
        // matches the same frame they arrive. Stale lines that have
        // rotated out of the grid are dropped before indexing.
        let mut filter_rows: Option<Vec<alacritty_terminal::index::Line>> =
            None;
        let mut filtered_total: usize = 0;
        if let Some(ref app_lines) = self.line_filter {
            // Filter just turned on (previous frame rendered raw):
            // start at the bottom, following the stream, instead of
            // resuming a stale offset from an earlier grep session.
            if !state.cached_filter_active {
                state.filter_offset = 0;
            }
            let topmost = terminal.topmost_line().0;
            let bottommost = terminal.bottommost_line().0;
            let screen_lines = terminal.grid().screen_lines();
            // Fresh-scan band. Steady state: viewport + margin (the
            // embedder's async list covers everything above). But
            // right after the embedder replaces the buffer (sort
            // flip, container switch, stream restart) its list is
            // empty or too short to fill the screen until an async
            // scan lands — extend the self-scan so the filtered
            // view keeps showing real matches instead of blanking
            // for the gap. Capped by cell count so a huge or very
            // wide scrollback can't turn this into a per-frame
            // full-buffer scan.
            let band = {
                let base = screen_lines + 5;
                if app_lines.len() < screen_lines {
                    let total_rows =
                        (bottommost - topmost + 1).max(0) as usize;
                    let columns = terminal.grid().columns().max(1);
                    const MAX_SELF_SCAN_CELLS: usize = 500_000;
                    let cap = (MAX_SELF_SCAN_CELLS / columns).max(base);
                    total_rows.min(cap).max(base)
                } else {
                    base
                }
            };
            let (band_start, tail) = match self.search_regex.as_mut() {
                Some(regex) => (
                    (bottommost - band.saturating_sub(1) as i32)
                        .max(topmost),
                    crate::backend::tail_matching_lines(
                        &terminal, regex, band,
                    ),
                ),
                // No regex to rescan with — trust the caller's list
                // for the full range instead of dropping the band.
                None => (bottommost + 1, Vec::new()),
            };
            let merged = merge_filter_lines(
                app_lines, &tail, topmost, band_start,
            );

            // The user navigated (F3 / Enter / etc.) — bring the
            // target match's line into the filtered window. Must be
            // detected *before* the highlight code below updates
            // `last_caller_current`.
            if self.current_match_start != state.last_caller_current {
                if let Some(target) = self.current_match_start {
                    if let Some(idx) = filter_line_index(
                        &merged,
                        target.line.0,
                    ) {
                        state.filter_offset = nav_offset_for(
                            merged.len(),
                            idx,
                            screen_lines,
                            state.filter_offset,
                        );
                    }
                }
            }

            filtered_total = merged.len();
            state.filter_offset = state
                .filter_offset
                .min(filtered_total.saturating_sub(screen_lines));
            let (win_start, win_end) = filter_window(
                filtered_total,
                screen_lines,
                state.filter_offset,
            );
            state.filter_rows_grid = merged[win_start..win_end].to_vec();
            filter_rows = Some(
                state
                    .filter_rows_grid
                    .iter()
                    .map(|l| alacritty_terminal::index::Line(*l))
                    .collect(),
            );
        }
        // The offset this frame's shapes are actually built with.
        // The scrollbar below may mutate `state.filter_offset`
        // after the shapes exist; stamping the *rendered* value
        // into the cache key makes the next frame miss the cache
        // and repaint at the new position instead of freezing on
        // the pre-drag frame.
        let rendered_filter_offset = state.filter_offset;

        // Compute visible search matches once. Match ranges are
        // expanded into a BTreeSet of grid points so the per-cell
        // highlight check is O(log n) instead of O(matches × cells).
        // BTreeSet (not HashSet) because alacritty's `Point` only
        // implements `Ord`.
        //
        // The scan range itself is bounded to viewport ± 5 lines via
        // `visible_regex_match_iter`, so cost is O(viewport) rather
        // than O(viewport + 200) which used to dominate frame time
        // on wide-grid (no-wrap) windows during search typing.
        let term_columns = terminal.grid().columns();
        let mut highlight_cells: std::collections::BTreeSet<TerminalGridPoint> =
            std::collections::BTreeSet::new();
        let mut current_cells: std::collections::BTreeSet<TerminalGridPoint> =
            std::collections::BTreeSet::new();
        let mut current_match: Option<std::ops::RangeInclusive<TerminalGridPoint>> = None;

        // Pre-allocate Vec with a generous upper-bound capacity.
        // Per frame we push 1-2 shapes per visible cell, so the
        // final Vec is typically 4000-10000 entries. Without
        // preallocation Vec doubles ~12 times and copies all
        // elements on each grow — measurable when called every
        // frame on streaming logs.
        let mut shapes: Vec<Shape> = Vec::with_capacity(16384);
        shapes.push(Shape::Rect(RectShape::filled(
            Rect::from_min_max(layout_min, layout_max),
            CornerRadius::ZERO,
            global_bg,
        )));

        // Visible column band on the *grid* (not on the painter
        // viewport) — this is the slice of the grid the user
        // actually sees. With `horizontal_offset_cols` set, the
        // band is `[offset, offset + visible_cols_in_viewport]`.
        // Cells outside this band are skipped before any per-cell
        // work; cells inside are drawn at `x = layout_min.x +
        // (col - offset) * cell_width` so they appear at the
        // viewport's left edge.
        let visible_cols_in_viewport: i32 = if cell_width > 0.0 {
            ((layout_max.x - layout_min.x) / cell_width).ceil() as i32
        } else {
            term_columns as i32
        };
        let h_offset_cols = self.horizontal_offset_cols as i32;
        let visible_min_col = h_offset_cols;
        let visible_max_col = h_offset_cols + visible_cols_in_viewport;

        // Now that we know the visible column band, expand search
        // matches — but skip ones that fall entirely outside the
        // visible columns. In wrap-off mode the grid is much wider
        // than the viewport; matches in off-screen columns can't
        // be highlighted anyway, so building BTreeSet entries for
        // them is dead work. This is the difference that makes
        // search feel as smooth in wrap-off as in wrap-on.
        // The orange "current" highlight tracks a Point that
        // *follows* the user's selected match as streaming
        // content shifts the grid. `tracked_current` carries that
        // Point across frames (mutated below to whatever match
        // we paint orange), and we reset it only when the caller
        // hands us a different `current_match_start` than we last
        // saw — i.e. the user explicitly navigated.
        if self.current_match_start != state.last_caller_current {
            state.tracked_current = self.current_match_start;
            state.last_caller_current = self.current_match_start;
        }
        let target_point: Option<TerminalGridPoint> = state
            .tracked_current
            .or(self.current_match_start);

        // Two-pass match handling so the "current" (orange)
        // highlight is stable across streaming-induced grid
        // shifts. Pass 1: collect all visible matches and find
        // the one *closest* to `target_point`. Pass 2: expand
        // into the right BTreeSet (current vs. all-others).
        let mut visible_matches: Vec<
            std::ops::RangeInclusive<TerminalGridPoint>,
        > = Vec::new();
        let mut closest_idx: Option<usize> = None;
        let mut closest_score: u64 = u64::MAX;
        if let Some(ref mut regex) = self.search_regex {
            // Use the column-bounded variant: only scans grid cells
            // in the visible column band. With wide wrap-off grids
            // (2000+ cols) this cuts the regex-iter cost ~20× vs
            // the full-line scan that `visible_regex_match_iter`
            // does.
            let scan_min = visible_min_col.max(0) as usize;
            let scan_max =
                visible_max_col.max(visible_min_col) as usize;
            // In filter mode, scan exactly the remapped rows being
            // displayed — the viewport-range scan would highlight
            // lines that aren't rendered and miss ones that are.
            let bounded = if let Some(ref rows) = filter_rows {
                crate::backend::regex_matches_on_lines(
                    &terminal, regex, rows, scan_min, scan_max,
                )
            } else {
                crate::backend::visible_regex_match_iter_in_cols(
                    &terminal, regex, scan_min, scan_max, 5,
                )
            };
            for m in bounded {
                let m_start_col = m.start().column.0 as i32;
                let m_end_col = m.end().column.0 as i32;
                let touches_visible = m_end_col >= visible_min_col
                    && m_start_col <= visible_max_col
                    || m.start().line != m.end().line;
                if !touches_visible {
                    continue;
                }
                if let Some(target) = target_point.as_ref() {
                    // Manhattan distance in (line, col) space —
                    // line dominates so we prefer matches on the
                    // same row when scrolling horizontally.
                    let s = m.start();
                    let dline = (s.line.0 - target.line.0).unsigned_abs() as u64;
                    let dcol = (s.column.0 as i64 - target.column.0 as i64)
                        .unsigned_abs();
                    let score = dline * 1_000_000 + dcol;
                    if score < closest_score {
                        closest_score = score;
                        closest_idx = Some(visible_matches.len());
                    }
                }
                visible_matches.push(m);
            }
        }
        // Acceptable matches:
        //   - Exact Point match (score == 0), OR
        //   - Same column, within N lines of target (streaming
        //     logs append new lines which shifts existing matches'
        //     Line index but never their Column).
        //
        // Same-column requirement is what distinguishes a
        // streaming-shift "follow" from a horizontal-pan "skip":
        // pan changes which columns are visible (columns differ
        // → reject), streaming changes which lines exist at a
        // given Point (columns same → accept). This way the
        // orange follows its physical match through streaming
        // bursts (between 1s `search_all` refreshes) without ever
        // snapping to a *different* match.
        const STREAMING_LINE_TOLERANCE: u64 = 200;
        let acceptable = closest_idx.map_or(false, |_| {
            let dline = closest_score / 1_000_000;
            let dcol = closest_score % 1_000_000;
            dline == 0 && dcol == 0
                || (dcol == 0 && dline <= STREAMING_LINE_TOLERANCE)
        });
        if !acceptable {
            closest_idx = None;
        }
        for (i, m) in visible_matches.into_iter().enumerate() {
            let is_current = Some(i) == closest_idx;
            if is_current {
                current_match = Some(m.clone());
                // Re-anchor `tracked_current` to whatever match
                // we picked. Next frame this Point becomes the
                // target, so the orange stays attached to the
                // *same physical match* even as streaming content
                // shifts grid coordinates underneath it.
                state.tracked_current = Some(*m.start());
            }
            let target = if is_current {
                &mut current_cells
            } else {
                &mut highlight_cells
            };
            expand_match_into_cells(m, term_columns, target);
        }
        let has_any_highlights =
            !current_cells.is_empty() || !highlight_cells.is_empty();

        // Manual iteration over only visible rows × visible cols.
        // `display_iter` walks every cell of the grid (~26k yields
        // for a 526×50 grid even with column-band skips), and each
        // yield costs ~1 µs of iterator advance overhead. Direct
        // `grid[Line][Column]` access skips the iterator entirely
        // — for ~80 visible cols × ~50 rows, that's 4k iterations
        // instead of 26k. ~6× speedup on the per-frame cell loop,
        // which previous profiling showed at 25 ms.
        //
        // The whole loop runs inside ONE `painter.fonts_mut(...)`
        // call. Each `painter.fonts_mut` internally takes the egui
        // Context's *write lock* (`ctx.write(...)`); per-cell calls
        // were paying that lock-acquire cost ~6000 times per frame
        // and dominated `cells_us`. With the lock hoisted out of
        // the inner loop, only one lock cycle per frame.
        let grid = terminal.grid();
        let display_offset_i32 = display_offset as i32;
        let screen_lines_i32 = grid.screen_lines() as i32;
        let total_columns = grid.columns();
        let col_start_idx = visible_min_col.max(0) as usize;
        let col_end_idx = (visible_max_col + 1).max(0) as usize;
        let col_end_idx = col_end_idx.min(total_columns);
        let is_app_cursor_mode = content.terminal_mode.contains(TermMode::APP_CURSOR);
        let font_type = self.font.font_type();
        // Filter mode always hides the cursor — its grid position
        // has no meaningful screen row under the remap.
        let hide_cursor = self.hide_cursor || filter_rows.is_some();
        // Focused → solid block cursor; unfocused (e.g. the search box has focus)
        // → hollow outline, so it's visually clear keystrokes go elsewhere.
        let cursor_focused = layout.has_focus();
        let theme = &self.theme;
        let mouse_pos = state.current_mouse_position_on_grid;
        painter.fonts_mut(|fonts| {
        for line_idx in 0..screen_lines_i32 {
            // Identity path (`None`): screen row N shows grid line
            // `N - display_offset`, exactly as before. Filter mode:
            // screen row N shows the Nth line of the remapped
            // window; when the window has fewer lines than the
            // screen, the remaining rows stay background-only.
            let viewport_line = match filter_rows {
                None => alacritty_terminal::index::Line(
                    line_idx - display_offset_i32,
                ),
                Some(ref rows) => match rows.get(line_idx as usize) {
                    Some(l) => *l,
                    None => break,
                },
            };
            for col_idx in col_start_idx..col_end_idx {
                let column = alacritty_terminal::index::Column(col_idx);
                let point = alacritty_terminal::index::Point::new(
                    viewport_line, column,
                );
                let cell = &grid[viewport_line][column];

                let flags = cell.flags;
                let is_wide_char_spacer =
                    flags.contains(cell::Flags::WIDE_CHAR_SPACER);
                if is_wide_char_spacer {
                    continue;
                }

                let is_wide_char = flags.contains(cell::Flags::WIDE_CHAR);
                let is_inverse = flags.contains(cell::Flags::INVERSE);
                let is_dim =
                    flags.intersects(cell::Flags::DIM | cell::Flags::DIM_BOLD);
                let is_selected = content
                    .selectable_range
                    .is_some_and(|r| r.contains(point));
                let is_hovered_hyperling =
                    content.hovered_hyperlink.as_ref().is_some_and(|r| {
                        r.contains(&point) && r.contains(&mouse_pos)
                    });

                let highlight_kind = if has_any_highlights {
                    if current_cells.contains(&point) {
                        HighlightKind::Current
                    } else if highlight_cells.contains(&point) {
                        HighlightKind::Match
                    } else {
                        HighlightKind::None
                    }
                } else {
                    HighlightKind::None
                };

                let col = col_idx as i32;
                let x = layout_min.x
                    + (cell_width * (col - h_offset_cols) as f32);
                // Identity path: paint row equals screen row (the
                // expression is kept verbatim; it equals `line_idx`).
                // Filter mode: pack remapped lines consecutively.
                let line_num = if filter_rows.is_none() {
                    viewport_line.0 + display_offset as i32
                } else {
                    line_idx
                };
                let y = layout_min.y + (cell_height * line_num as f32);

                let mut fg = theme.get_color(cell.fg);
                let mut bg = theme.get_color(cell.bg);
            let cell_width = if is_wide_char {
                cell_width * 2.0
            } else {
                cell_width
            };

            if is_dim {
                fg = fg.linear_multiply(0.7);
            }

            // ANSI reverse video genuinely swaps fg/bg.
            if is_inverse {
                std::mem::swap(&mut fg, &mut bg);
            }
            // Selection paints a subtle tint behind the unchanged text,
            // instead of inverting — readable, and empty cells highlight
            // gently rather than becoming solid opposite-color blocks.
            if is_selected {
                bg = selection_bg;
            }

            match highlight_kind {
                HighlightKind::Current => {
                    bg = egui::Color32::from_rgb(255, 150, 50); // orange for current match
                    fg = egui::Color32::BLACK;
                }
                HighlightKind::Match => {
                    bg = egui::Color32::from_rgb(180, 160, 60); // yellow for other matches
                    fg = egui::Color32::BLACK;
                }
                HighlightKind::None => {}
            }

            if global_bg != bg {
                shapes.push(Shape::Rect(RectShape::filled(
                    Rect::from_min_size(
                        Pos2::new(x, y),
                        Vec2::new(cell_width + 1., cell_height + 1.),
                    ),
                    CornerRadius::ZERO,
                    bg,
                )));
            }

                if is_hovered_hyperling {
                    let underline_height = y + cell_height;
                    shapes.push(Shape::LineSegment {
                        points: [
                            Pos2::new(x, underline_height),
                            Pos2::new(x + cell_width, underline_height),
                        ],
                        stroke: Stroke::new(cell_height * 0.15, fg),
                    });
                }

                if cursor_point == point && !hide_cursor {
                    let cursor_color = theme.get_color(content.cursor.fg);
                    let cursor_rect = Rect::from_min_size(
                        Pos2::new(x, y),
                        Vec2::new(cell_width, cell_height),
                    );
                    let cursor_shape = if cursor_focused {
                        // Focused: solid block.
                        RectShape::filled(cursor_rect, CornerRadius::default(), cursor_color)
                    } else {
                        // Unfocused: hollow outline (terminal doesn't have focus).
                        RectShape::stroke(
                            cursor_rect,
                            CornerRadius::default(),
                            Stroke::new(1.0_f32, cursor_color),
                            egui::StrokeKind::Inside,
                        )
                    };
                    shapes.push(Shape::Rect(cursor_shape));
                }

                if cell.c != ' ' && cell.c != '\t' {
                    // Only invert the glyph under a SOLID (focused) block cursor;
                    // a hollow unfocused cursor leaves the char legible as-is.
                    if cursor_point == point
                        && is_app_cursor_mode
                        && !hide_cursor
                        && cursor_focused
                    {
                        std::mem::swap(&mut fg, &mut bg);
                    }

                    shapes.push(Shape::text(
                        fonts,
                        Pos2 {
                            x: x + (cell_width / 2.0),
                            y,
                        },
                        Align2::CENTER_TOP,
                        cell.c,
                        font_type.clone(),
                        fg,
                    ));
                }
            } // end col loop
        } // end row loop
        }); // end painter.fonts_mut

        // Draw border around current search match.
        // X positions follow the same `(col - h_offset_cols) * cell_width`
        // formula as cell drawing above, so the border tracks the
        // visible band when the user pans horizontally in wrap-off mode.
        if let Some(ref cm) = current_match {
            let cols = terminal.grid().columns();
            let start = *cm.start();
            let end = *cm.end();
            let border_color = egui::Color32::from_rgb(255, 180, 50);
            let stroke = Stroke::new(2.0_f32, border_color);

            // Screen paint row for a grid line. Identity path keeps
            // the original `line + display_offset` formula; filter
            // mode looks the line up in the remapped window (`None`
            // → the line isn't displayed, skip that border segment).
            let row_for_line =
                |l: alacritty_terminal::index::Line| -> Option<i32> {
                    match filter_rows {
                        None => Some(l.0 + display_offset as i32),
                        Some(ref rows) => rows
                            .binary_search(&l)
                            .ok()
                            .map(|i| i as i32),
                    }
                };

            if start.line == end.line {
                // Single-line match: one border rect
                let x1 = layout_min.x
                    + (cell_width * (start.column.0 as i32 - h_offset_cols) as f32);
                let x2 = layout_min.x
                    + (cell_width
                        * (end.column.0 as i32 + 1 - h_offset_cols) as f32);
                if let Some(line_num) = row_for_line(start.line) {
                    let y = layout_min.y + (cell_height * line_num as f32);
                    let rect = Rect::from_min_size(
                        Pos2::new(x1, y),
                        Vec2::new(x2 - x1, cell_height),
                    );
                    shapes.push(Shape::Rect(RectShape::new(rect, CornerRadius::same(2), egui::Color32::TRANSPARENT, stroke, egui::StrokeKind::Outside)));
                }
            } else {
                // Multi-line match: border per line
                let mut line = start.line;
                while line <= end.line {
                    let Some(line_num) = row_for_line(line) else {
                        line += 1;
                        continue;
                    };
                    let y = layout_min.y + (cell_height * line_num as f32);
                    let col_start = if line == start.line { start.column.0 } else { 0 };
                    let col_end = if line == end.line { end.column.0 + 1 } else { cols };
                    let x1 = layout_min.x
                        + (cell_width * (col_start as i32 - h_offset_cols) as f32);
                    let x2 = layout_min.x
                        + (cell_width * (col_end as i32 - h_offset_cols) as f32);
                    let rect = Rect::from_min_size(
                        Pos2::new(x1, y),
                        Vec2::new(x2 - x1, cell_height),
                    );
                    shapes.push(Shape::Rect(RectShape::new(rect, CornerRadius::same(2), egui::Color32::TRANSPARENT, stroke, egui::StrokeKind::Outside)));
                    line += 1;
                }
            }
        }

        // Scrollbar. Grep/filter mode gets its own branch below —
        // it counts *filtered* lines and moves `filter_offset`
        // instead of the raw grid's display offset. The raw branch
        // is the pre-filter code, untouched.
        let filter_scrollbar = filter_rows.is_some();
        let total_lines = terminal.grid().total_lines();
        let screen_lines = terminal.grid().screen_lines();
        let history_size = total_lines.saturating_sub(screen_lines);

        if !filter_scrollbar && history_size > 0 {
            let scrollbar_width = 8.0_f32;
            // Pin to the visible viewport's right edge — that's
            // where the user can actually see and click. The track's
            // y range is still clamped to the visible vertical band
            // so thumb positioning math stays consistent with the
            // viewport, not the off-screen grid.
            let track_rect = Rect::from_min_max(
                Pos2::new(visible_rect.max.x - scrollbar_width, visible_rect.min.y),
                Pos2::new(visible_rect.max.x, visible_rect.max.y),
            );
            let track_height = track_rect.height();
            let thumb_frac = screen_lines as f32 / total_lines as f32;
            let thumb_height = (thumb_frac * track_height).max(20.0);
            let scrollable_track = track_height - thumb_height;

            // Thumb position: display_offset=0 → thumb at bottom, display_offset=max → thumb at top
            let current_offset = terminal.grid().display_offset();
            let thumb_top = if history_size > 0 {
                let ratio = current_offset as f32 / history_size as f32;
                // ratio=0 → bottom, ratio=1 → top
                track_rect.min.y + (1.0 - ratio) * scrollable_track
            } else {
                track_rect.max.y - thumb_height
            };
            let thumb_rect = Rect::from_min_size(
                Pos2::new(track_rect.min.x, thumb_top),
                Vec2::new(scrollbar_width, thumb_height),
            );

            // Batch pointer state into a single input lock
            let (pointer_pos, primary_down, primary_pressed) =
                layout.ctx.input(|i| {
                    (
                        i.pointer.hover_pos(),
                        i.pointer.primary_down(),
                        i.pointer.primary_pressed(),
                    )
                });

            if let Some(pos) = pointer_pos {
                if primary_pressed && track_rect.contains(pos) {
                    if thumb_rect.contains(pos) {
                        // Grabbing the thumb: remember offset so it doesn't snap
                        state.scrollbar_dragging = true;
                        state.scrollbar_grab_offset = pos.y - thumb_top;
                    } else {
                        // Clicked on track above/below thumb: scroll by one page
                        let page = screen_lines.saturating_sub(1).max(1) as i32;
                        if pos.y < thumb_rect.min.y {
                            terminal.scroll_display(Scroll::Delta(page));
                        } else {
                            terminal.scroll_display(Scroll::Delta(-page));
                        }
                        self.backend.mark_dirty();
                    }
                }

                if state.scrollbar_dragging && primary_down {
                    // Compute desired thumb top from pointer position
                    let desired_thumb_top =
                        pos.y - state.scrollbar_grab_offset;
                    let ratio = if scrollable_track > 0.0 {
                        1.0 - ((desired_thumb_top - track_rect.min.y)
                            / scrollable_track)
                            .clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let target = (ratio * history_size as f32).round() as i32;

                    // Use absolute positioning: scroll to bottom then up by target
                    terminal.scroll_display(Scroll::Bottom);
                    if target > 0 {
                        terminal.scroll_display(Scroll::Delta(target));
                    }
                    self.backend.mark_dirty();
                }
            }
            if !primary_down {
                state.scrollbar_dragging = false;
            }

            // Scrollbar tint: derive from the palette's foreground so
            // the bar stays visible on any terminal background. Pure-
            // white alpha (the previous constant) reads fine on a
            // black terminal but disappears on light-theme palettes
            // where the terminal bg is near-white or light gray.
            let sb_fg = self.theme
                .get_color(Color::Named(NamedColor::Foreground));
            let track_color = egui::Color32::from_rgba_unmultiplied(
                sb_fg.r(), sb_fg.g(), sb_fg.b(), 24,
            );
            let thumb_color = egui::Color32::from_rgba_unmultiplied(
                sb_fg.r(), sb_fg.g(), sb_fg.b(), 110,
            );
            // Draw scrollbar track
            shapes.push(Shape::Rect(RectShape::filled(
                track_rect,
                CornerRadius::same(4),
                track_color,
            )));
            // Draw scrollbar thumb
            shapes.push(Shape::Rect(RectShape::filled(
                thumb_rect,
                CornerRadius::same(4),
                thumb_color,
            )));
        } else if filter_scrollbar
            && filtered_total > screen_lines
        {
            // Grep/filter scrollbar: same geometry as the raw
            // branch, but total = filtered lines and click/drag
            // move `filter_offset` — never the raw grid. Shapes
            // for this frame were already built with the offset
            // captured in `rendered_filter_offset`; mutations here
            // take effect next frame via the cache-key mismatch.
            let max_off = filtered_total - screen_lines;
            let scrollbar_width = 8.0_f32;
            let track_rect = Rect::from_min_max(
                Pos2::new(visible_rect.max.x - scrollbar_width, visible_rect.min.y),
                Pos2::new(visible_rect.max.x, visible_rect.max.y),
            );
            let track_height = track_rect.height();
            let thumb_frac = screen_lines as f32 / filtered_total as f32;
            let thumb_height = (thumb_frac * track_height).max(20.0);
            let scrollable_track = track_height - thumb_height;

            // filter_offset=0 → thumb at bottom, =max_off → at top
            let ratio = state.filter_offset as f32 / max_off as f32;
            let thumb_top =
                track_rect.min.y + (1.0 - ratio) * scrollable_track;
            let thumb_rect = Rect::from_min_size(
                Pos2::new(track_rect.min.x, thumb_top),
                Vec2::new(scrollbar_width, thumb_height),
            );

            let (pointer_pos, primary_down, primary_pressed) =
                layout.ctx.input(|i| {
                    (
                        i.pointer.hover_pos(),
                        i.pointer.primary_down(),
                        i.pointer.primary_pressed(),
                    )
                });

            if let Some(pos) = pointer_pos {
                if primary_pressed && track_rect.contains(pos) {
                    if thumb_rect.contains(pos) {
                        state.scrollbar_dragging = true;
                        state.scrollbar_grab_offset = pos.y - thumb_top;
                    } else {
                        let page = screen_lines.saturating_sub(1).max(1);
                        if pos.y < thumb_rect.min.y {
                            state.filter_offset = state
                                .filter_offset
                                .saturating_add(page)
                                .min(max_off);
                        } else {
                            state.filter_offset =
                                state.filter_offset.saturating_sub(page);
                        }
                    }
                }

                if state.scrollbar_dragging && primary_down {
                    let desired_thumb_top =
                        pos.y - state.scrollbar_grab_offset;
                    let ratio = if scrollable_track > 0.0 {
                        1.0 - ((desired_thumb_top - track_rect.min.y)
                            / scrollable_track)
                            .clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    state.filter_offset =
                        (ratio * max_off as f32).round() as usize;
                }
            }
            if !primary_down {
                state.scrollbar_dragging = false;
            }

            let sb_fg = self.theme
                .get_color(Color::Named(NamedColor::Foreground));
            let track_color = egui::Color32::from_rgba_unmultiplied(
                sb_fg.r(), sb_fg.g(), sb_fg.b(), 24,
            );
            let thumb_color = egui::Color32::from_rgba_unmultiplied(
                sb_fg.r(), sb_fg.g(), sb_fg.b(), 110,
            );
            shapes.push(Shape::Rect(RectShape::filled(
                track_rect,
                CornerRadius::same(4),
                track_color,
            )));
            shapes.push(Shape::Rect(RectShape::filled(
                thumb_rect,
                CornerRadius::same(4),
                thumb_color,
            )));
        } else {
            state.scrollbar_dragging = false;
        }

        drop(terminal);

        // Cache shapes for reuse when terminal is idle and for the
        // dirty-but-throttled fast path on streaming logs. Cache
        // keys are integer-rounded so sub-pixel drift in the
        // surrounding layout doesn't invalidate the cache.
        state.cached_shapes = Some(shapes.clone());
        state.cached_rect = Some(round_rect_int(layout.rect));
        state.cached_visible = Some(round_rect_int(*visible_rect));
        state.cached_h_offset = self.horizontal_offset_cols;
        state.cached_search_key = self.search_key;
        state.cached_current_match = self.current_match_start;
        state.cached_filter_active = self.line_filter.is_some();
        state.cached_filter_offset = rendered_filter_offset;
        state.cached_display_offset = display_offset;
        state.last_render_at = Some(std::time::Instant::now());
        state.had_highlights = has_any_highlights;
        painter.extend(shapes);
    }
}

/// Merge the embedder-supplied filtered-line list with the fresh
/// tail scan. `app` covers the scrollback but may be stale, so it
/// only contributes lines above the tail band (`< band_start`) and
/// within grid bounds (`>= topmost`); `tail` is authoritative for
/// the band itself. Both inputs are sorted ascending and the band
/// split keeps the result sorted with no duplicates.
fn merge_filter_lines(
    app: &[i32],
    tail: &[i32],
    topmost: i32,
    band_start: i32,
) -> Vec<i32> {
    app.iter()
        .copied()
        .filter(|l| *l >= topmost && *l < band_start)
        .chain(tail.iter().copied())
        .collect()
}

/// The `[start, end)` slice of a filtered-line list that is visible
/// with `offset` lines scrolled up from the bottom. Clamps the
/// offset so the window never runs past the top of the list.
fn filter_window(
    total: usize,
    screen: usize,
    offset: usize,
) -> (usize, usize) {
    let max_off = total.saturating_sub(screen);
    let offset = offset.min(max_off);
    let end = total - offset;
    let start = end.saturating_sub(screen);
    (start, end)
}

/// Index of `line` in a sorted filtered-line list, if present.
fn filter_line_index(lines: &[i32], line: i32) -> Option<usize> {
    lines.binary_search(&line).ok()
}

/// Scroll offset that brings list index `idx` into the filtered
/// window. Keeps the current offset when the index is already
/// visible; otherwise places the target roughly a third of the way
/// down the window (mirrors how F3 navigation re-centers the raw
/// grid).
fn nav_offset_for(
    total: usize,
    idx: usize,
    screen: usize,
    cur_offset: usize,
) -> usize {
    let max_off = total.saturating_sub(screen);
    let cur = cur_offset.min(max_off);
    let (start, end) = filter_window(total, screen, cur);
    if idx >= start && idx < end {
        return cur;
    }
    let desired_start = idx.saturating_sub(screen / 3);
    let desired_end = desired_start + screen;
    total.saturating_sub(desired_end).min(max_off)
}

/// Round a `Rect` to integer pixels. Used as a cache key so
/// sub-pixel drift from layout float math doesn't force a fresh
/// render every frame.
fn round_rect_int(r: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(r.min.x.round(), r.min.y.round()),
        egui::pos2(r.max.x.round(), r.max.y.round()),
    )
}

/// Expand an inclusive grid-point range into a set of every cell it
/// covers, line-by-line. Used to flatten search-match ranges into a
/// BTreeSet so the per-cell highlight check during render is O(log n).
fn expand_match_into_cells(
    range: std::ops::RangeInclusive<TerminalGridPoint>,
    term_columns: usize,
    out: &mut std::collections::BTreeSet<TerminalGridPoint>,
) {
    use alacritty_terminal::index::{Column, Line};
    let start = *range.start();
    let end = *range.end();
    let last_col = term_columns.saturating_sub(1);
    if start.line == end.line {
        for c in start.column.0..=end.column.0 {
            out.insert(TerminalGridPoint::new(start.line, Column(c)));
        }
        return;
    }
    // First line: from start.column to end of line.
    for c in start.column.0..=last_col {
        out.insert(TerminalGridPoint::new(start.line, Column(c)));
    }
    // Middle lines: every column.
    let mut line = start.line.0 + 1;
    while line < end.line.0 {
        for c in 0..=last_col {
            out.insert(TerminalGridPoint::new(Line(line), Column(c)));
        }
        line += 1;
    }
    // Last line: from 0 to end.column.
    for c in 0..=end.column.0 {
        out.insert(TerminalGridPoint::new(end.line, Column(c)));
    }
}

fn process_keyboard_event(
    event: egui::Event,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    modifiers: Modifiers,
) -> InputAction {
    match event {
        egui::Event::Text(text) => {
            process_text_event(&text, modifiers, backend, bindings_layout)
        },
        egui::Event::Paste(text) => InputAction::BackendCall(
            #[cfg(not(any(target_os = "ios", target_os = "macos")))]
            if modifiers.contains(Modifiers::COMMAND | Modifiers::SHIFT) {
                BackendCommand::Write(text.as_bytes().to_vec())
            } else if modifiers.alt {
                // Ctrl+Alt+V → ESC + ^V (Meta-on-control).
                BackendCommand::Write(vec![0x1b, 0x16])
            } else {
                // Hotfix - Send ^V when there's not selection on view.
                BackendCommand::Write([0x16].to_vec())
            },
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            {
                BackendCommand::Write(text.as_bytes().to_vec())
            },
        ),
        egui::Event::Copy => {
            #[cfg(not(any(target_os = "ios", target_os = "macos")))]
            if modifiers.contains(Modifiers::COMMAND | Modifiers::SHIFT) {
                let content = backend.selectable_content();
                InputAction::WriteToClipboard(content)
            } else if modifiers.alt {
                // Ctrl+Alt+C → ESC + ^C (Meta-on-control).
                InputAction::BackendCall(BackendCommand::Write(vec![0x1b, 0x03]))
            } else {
                // Hotfix - Send ^C when there's not selection on view.
                InputAction::BackendCall(BackendCommand::Write([0x3].to_vec()))
            }
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            {
                let content = backend.selectable_content();
                InputAction::WriteToClipboard(content)
            }
        },
        egui::Event::Key {
            key,
            pressed,
            modifiers,
            ..
        } => process_keyboard_key(
            backend,
            bindings_layout,
            key,
            modifiers,
            pressed,
        ),
        _ => InputAction::Ignore,
    }
}

fn process_text_event(
    text: &str,
    modifiers: Modifiers,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
) -> InputAction {
    // xterm-style "Meta sends ESC": when Alt is held alongside a
    // text-producing key (e.g. Alt+b for backward-word in readline),
    // prepend ESC so the application sees a Meta-prefixed sequence
    // rather than the bare letter. Skipped on macOS where Option is
    // typically used to insert special characters (ç, π, …) and
    // applications use Cmd for shortcuts.
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let alt_prefix = modifiers.alt && !modifiers.ctrl && !modifiers.command;
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let alt_prefix = false;

    let write_bytes = |bytes: Vec<u8>| -> InputAction {
        let payload = if alt_prefix {
            let mut out = Vec::with_capacity(bytes.len() + 1);
            out.push(0x1b);
            out.extend_from_slice(&bytes);
            out
        } else {
            bytes
        };
        InputAction::BackendCall(BackendCommand::Write(payload))
    };

    if let Some(key) = Key::from_name(text) {
        if bindings_layout.get_action(
            InputKind::KeyCode(key),
            modifiers,
            backend.last_content().terminal_mode,
        ) == BindingAction::Ignore
        {
            write_bytes(text.as_bytes().to_vec())
        } else {
            InputAction::Ignore
        }
    } else {
        write_bytes(text.as_bytes().to_vec())
    }
}

fn process_keyboard_key(
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    key: Key,
    modifiers: Modifiers,
    pressed: bool,
) -> InputAction {
    if !pressed {
        return InputAction::Ignore;
    }

    let terminal_mode = backend.last_content().terminal_mode;
    let binding_action = bindings_layout.get_action(
        InputKind::KeyCode(key),
        modifiers,
        terminal_mode,
    );

    // Ctrl+Alt+<key> = ESC + Ctrl+<key> (xterm Meta-on-control).
    // No explicit binding exists for every Ctrl+Alt combination,
    // so when the direct lookup misses, retry without Alt and
    // prepend ESC if a Ctrl-binding exists. As a final fallback for
    // printable keys with no Ctrl-binding (digits, punctuation),
    // emit ESC + the literal character — matching what Alt-alone
    // would produce (e.g. Ctrl+Alt+1 → "\x1b1", same as Alt+1).
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    if matches!(binding_action, BindingAction::Ignore)
        && modifiers.alt
        && modifiers.ctrl
    {
        let alt_stripped = Modifiers { alt: false, ..modifiers };
        let inner = bindings_layout.get_action(
            InputKind::KeyCode(key),
            alt_stripped,
            terminal_mode,
        );
        match inner {
            BindingAction::Char(c) => {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                let mut out = Vec::with_capacity(s.len() + 1);
                out.push(0x1b);
                out.extend_from_slice(s.as_bytes());
                return InputAction::BackendCall(BackendCommand::Write(out));
            }
            BindingAction::Esc(seq) => {
                let mut out = Vec::with_capacity(seq.len() + 1);
                out.push(0x1b);
                out.extend_from_slice(seq.as_bytes());
                return InputAction::BackendCall(BackendCommand::Write(out));
            }
            BindingAction::Ignore => {
                // Final fallback: ESC + literal char for keys
                // whose symbol_or_name() is a single printable
                // ASCII character (digits, ".", ",", "/", …).
                // Multi-char names ("Tab", "Enter", …) are
                // skipped to avoid sending garbage.
                let sym = key.symbol_or_name();
                let mut chars = sym.chars();
                if let (Some(c), None) = (chars.next(), chars.next()) {
                    if c.is_ascii_graphic() {
                        let lower = c.to_ascii_lowercase();
                        return InputAction::BackendCall(BackendCommand::Write(
                            vec![0x1b, lower as u8],
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    match binding_action {
        BindingAction::Char(c) => {
            let mut buf = [0, 0, 0, 0];
            let str = c.encode_utf8(&mut buf);
            InputAction::BackendCall(BackendCommand::Write(
                str.as_bytes().to_vec(),
            ))
        },
        BindingAction::Esc(seq) => InputAction::BackendCall(
            BackendCommand::Write(seq.as_bytes().to_vec()),
        ),
        _ => InputAction::Ignore,
    }
}

/// Map a pointer position to the raw grid point it touches under
/// the grep/filter remap, plus the cell side (for selection). Uses
/// the row list rendered by the last filter frame; returns `None`
/// when nothing is displayed (no matches yet) or geometry is
/// degenerate.
fn filter_selection_point(
    state: &TerminalViewState,
    layout: &Response,
    term_size: &crate::backend::TerminalSize,
    h_offset_cols: usize,
    position: Pos2,
) -> Option<(TerminalGridPoint, alacritty_terminal::index::Side)> {
    let (line, col, right_side) = filter_selection_target(
        &state.filter_rows_grid,
        position.x - layout.rect.min.x,
        position.y - layout.rect.min.y,
        term_size.cell_width as f32,
        term_size.cell_height as f32,
        term_size.columns(),
        h_offset_cols,
    )?;
    let side = if right_side {
        alacritty_terminal::index::Side::Right
    } else {
        alacritty_terminal::index::Side::Left
    };
    Some((
        TerminalGridPoint::new(
            alacritty_terminal::index::Line(line),
            alacritty_terminal::index::Column(col),
        ),
        side,
    ))
}

/// Pure pixel→(grid line, column, right-side) math for
/// `filter_selection_point`. `rows` is the remapped window (grid
/// lines top to bottom), `x`/`y` are relative to the widget origin,
/// and `h_offset_cols` is the wrap-off horizontal pan (screen
/// column 0 shows grid column `h_offset_cols`).
fn filter_selection_target(
    rows: &[i32],
    x: f32,
    y: f32,
    cell_width: f32,
    cell_height: f32,
    columns: usize,
    h_offset_cols: usize,
) -> Option<(i32, usize, bool)> {
    if rows.is_empty() || cell_width <= 0.0 || cell_height <= 0.0 || columns == 0 {
        return None;
    }
    let x = x.max(0.0);
    let y = y.max(0.0);
    let row = ((y / cell_height) as usize).min(rows.len() - 1);
    let col = ((x / cell_width) as usize + h_offset_cols).min(columns - 1);
    let right_side = x % cell_width > cell_width / 2.0;
    Some((rows[row], col, right_side))
}

/// Rows one wheel notch travels when the view itself scrolls.
///
/// X11 reports a notch as a single line, so an unscaled notch moved the
/// view one row — next to a text editor or a browser, both of which
/// cover several lines per notch, that is not smoothness, it is just
/// slow. Three is the long-standing terminal convention (xterm, and
/// every libvte terminal) and roughly matches what an editor does.
/// Under mouse tracking the notch is relayed to the program instead and
/// is worth one row; see the wheel arm in `process_input`.
const ROWS_PER_NOTCH: f32 = 3.0;

/// Bank a wheel event's distance in pixels, to be spent by the frame
/// loop. Shared by the raw grid and the grep/filter view, which move
/// different offsets but scroll at the same cadence. `rows_per_notch`
/// is what a `Line`-unit notch is worth.
fn bank_wheel_pixels(
    state: &mut TerminalViewState,
    cell_height: f32,
    unit: MouseWheelUnit,
    delta: Vec2,
    rows_per_notch: f32,
) {
    match unit {
        // A notch is a count of lines; convert to pixels so the frame
        // loop can pay it out gradually.
        MouseWheelUnit::Line => {
            state.scroll_pixels -= delta.y * rows_per_notch * cell_height
        },
        // A trackpad already reports the distance the finger travelled;
        // scaling that would overshoot what the user asked for.
        MouseWheelUnit::Point => state.scroll_pixels -= delta.y,
        MouseWheelUnit::Page => {},
    }
}

/// How many rows to spend this frame, taken out of the bank.
///
/// Not the whole bank, and not a single row either: a flick of ten
/// notches would crawl for a second at one row a frame, and a single
/// notch spent whole is the jump we are trying to avoid. A third of
/// what is left, at least one row, drains fast when there is a lot and
/// eases out at the end — the shape a browser's scroll animation has.
fn drain_banked_scroll(
    state: &mut TerminalViewState,
    cell_height: f32,
) -> i32 {
    if cell_height <= 0.0 {
        state.scroll_pixels = 0.0;
        return 0;
    }
    let banked = (state.scroll_pixels / cell_height).trunc();
    if banked == 0.0 {
        return 0;
    }
    let step = (banked.abs() / 3.0).ceil().max(1.0) * banked.signum();
    state.scroll_pixels -= step * cell_height;
    -step as i32
}

fn process_button_click(
    state: &mut TerminalViewState,
    layout: &Response,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    button: PointerButton,
    position: Pos2,
    modifiers: &Modifiers,
    pressed: bool,
) -> InputAction {
    match button {
        PointerButton::Primary => process_left_button(
            state,
            layout,
            backend,
            bindings_layout,
            position,
            modifiers,
            pressed,
        ),
        _ => InputAction::Ignore,
    }
}

fn process_left_button(
    state: &mut TerminalViewState,
    layout: &Response,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    position: Pos2,
    modifiers: &Modifiers,
    pressed: bool,
) -> InputAction {
    let terminal_mode = backend.last_content().terminal_mode;
    if terminal_mode.intersects(TermMode::MOUSE_MODE) {
        InputAction::BackendCall(BackendCommand::MouseReport(
            MouseButton::LeftButton,
            *modifiers,
            state.current_mouse_position_on_grid,
            pressed,
        ))
    } else if pressed {
        process_left_button_pressed(state, layout, position)
    } else {
        process_left_button_released(
            state,
            layout,
            backend,
            bindings_layout,
            position,
            modifiers,
        )
    }
}

fn process_left_button_pressed(
    state: &mut TerminalViewState,
    layout: &Response,
    position: Pos2,
) -> InputAction {
    state.is_dragged = true;
    InputAction::BackendCall(build_start_select_command(layout, position))
}

fn process_left_button_released(
    state: &mut TerminalViewState,
    layout: &Response,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    position: Pos2,
    modifiers: &Modifiers,
) -> InputAction {
    state.is_dragged = false;
    if layout.double_clicked() || layout.triple_clicked() {
        InputAction::BackendCall(build_start_select_command(layout, position))
    } else {
        let terminal_content = backend.last_content();
        let binding_action = bindings_layout.get_action(
            InputKind::Mouse(PointerButton::Primary),
            *modifiers,
            terminal_content.terminal_mode,
        );

        if binding_action == BindingAction::LinkOpen {
            InputAction::BackendCall(BackendCommand::ProcessLink(
                LinkAction::Open,
                state.current_mouse_position_on_grid,
            ))
        } else {
            InputAction::Ignore
        }
    }
}

fn build_start_select_command(
    layout: &Response,
    cursor_position: Pos2,
) -> BackendCommand {
    let selection_type = if layout.double_clicked() {
        SelectionType::Semantic
    } else if layout.triple_clicked() {
        SelectionType::Lines
    } else {
        SelectionType::Simple
    };

    BackendCommand::SelectStart(
        selection_type,
        cursor_position.x - layout.rect.min.x,
        cursor_position.y - layout.rect.min.y,
    )
}

fn process_mouse_move(
    state: &mut TerminalViewState,
    layout: &Response,
    backend: &TerminalBackend,
    position: Pos2,
    modifiers: &Modifiers,
) -> Vec<InputAction> {
    let terminal_content = backend.last_content();
    let cursor_x = position.x - layout.rect.min.x;
    let cursor_y = position.y - layout.rect.min.y;
    state.current_mouse_position_on_grid = TerminalBackend::selection_point(
        cursor_x,
        cursor_y,
        &terminal_content.terminal_size,
        terminal_content.display_offset,
    );

    let mut actions = vec![];
    // Handle command or selection update based on terminal mode and modifiers
    if state.is_dragged {
        let terminal_mode = terminal_content.terminal_mode;
        let cmd = if terminal_mode.contains(TermMode::MOUSE_MOTION)
            && modifiers.is_none()
        {
            InputAction::BackendCall(BackendCommand::MouseReport(
                MouseButton::LeftMove,
                *modifiers,
                state.current_mouse_position_on_grid,
                true,
            ))
        } else {
            // Auto-scroll when dragging above or below the terminal area
            let cell_height = terminal_content.terminal_size.cell_height as f32;
            if cursor_y < 0.0 {
                let lines = ((-cursor_y) / cell_height).ceil().max(1.0) as i32;
                actions.push(InputAction::BackendCall(BackendCommand::Scroll(lines)));
            } else if cursor_y > layout.rect.height() {
                let overflow = cursor_y - layout.rect.height();
                let lines = (overflow / cell_height).ceil().max(1.0) as i32;
                actions.push(InputAction::BackendCall(BackendCommand::Scroll(-lines)));
            }
            InputAction::BackendCall(BackendCommand::SelectUpdate(
                cursor_x, cursor_y,
            ))
        };

        actions.push(cmd);
    }

    // Handle link hover if applicable
    if modifiers.command_only() {
        actions.push(InputAction::BackendCall(BackendCommand::ProcessLink(
            LinkAction::Hover,
            state.current_mouse_position_on_grid,
        )));
    }

    actions
}

#[cfg(test)]
mod filter_tests {
    use super::{
        filter_line_index, filter_selection_target, filter_window,
        merge_filter_lines, nav_offset_for,
    };

    #[test]
    fn merge_keeps_app_above_band_and_tail_inside() {
        // app has stale entries inside the band (5, 9) that the
        // fresh tail (band_start = 5) supersedes.
        let app = vec![-10, -3, 0, 5, 9];
        let tail = vec![6, 8];
        assert_eq!(
            merge_filter_lines(&app, &tail, -100, 5),
            vec![-10, -3, 0, 6, 8]
        );
    }

    #[test]
    fn merge_drops_lines_rotated_out_of_the_grid() {
        let app = vec![-500, -200, -50, 0];
        assert_eq!(
            merge_filter_lines(&app, &[], -100, 10),
            vec![-50, 0]
        );
    }

    #[test]
    fn merge_empty_inputs() {
        assert_eq!(merge_filter_lines(&[], &[], -100, 5), Vec::<i32>::new());
        assert_eq!(merge_filter_lines(&[], &[1, 2], -100, 0), vec![1, 2]);
    }

    #[test]
    fn window_at_bottom() {
        assert_eq!(filter_window(100, 40, 0), (60, 100));
    }

    #[test]
    fn window_scrolled_up_and_clamped() {
        assert_eq!(filter_window(100, 40, 30), (30, 70));
        // offset past the top clamps to the topmost window
        assert_eq!(filter_window(100, 40, 500), (0, 40));
    }

    #[test]
    fn window_shorter_than_screen() {
        assert_eq!(filter_window(10, 40, 0), (0, 10));
        assert_eq!(filter_window(10, 40, 7), (0, 10));
        assert_eq!(filter_window(0, 40, 0), (0, 0));
    }

    #[test]
    fn line_index_lookup() {
        let lines = vec![-5, 0, 3, 42];
        assert_eq!(filter_line_index(&lines, 3), Some(2));
        assert_eq!(filter_line_index(&lines, 4), None);
        assert_eq!(filter_line_index(&[], 4), None);
    }

    #[test]
    fn nav_keeps_offset_when_target_visible() {
        // window at offset 10 of total 100, screen 40 → [50, 90)
        assert_eq!(nav_offset_for(100, 60, 40, 10), 10);
    }

    #[test]
    fn nav_scrolls_to_target_above_window() {
        // target index 5 with screen 40 → desired_start 0 (5 - 13
        // saturates towards the top), offset near max
        let off = nav_offset_for(100, 5, 40, 0);
        let (start, end) = filter_window(100, 40, off);
        assert!((start..end).contains(&5));
    }

    #[test]
    fn nav_scrolls_to_target_below_window() {
        // window at offset 50 → [10, 50); target 95 is below
        let off = nav_offset_for(100, 95, 40, 50);
        let (start, end) = filter_window(100, 40, off);
        assert!((start..end).contains(&95));
    }

    #[test]
    fn nav_short_list_stays_at_zero() {
        assert_eq!(nav_offset_for(10, 3, 40, 0), 0);
        assert_eq!(nav_offset_for(10, 3, 40, 99), 0);
    }

    #[test]
    fn selection_target_maps_screen_row_to_grid_line() {
        let rows = vec![-40, -12, 3, 7];
        // y in row 2 (cell_height 10), x in col 4 (cell_width 8)
        let (line, col, right) =
            filter_selection_target(&rows, 33.0, 25.0, 8.0, 10.0, 80, 0)
                .unwrap();
        assert_eq!(line, 3);
        assert_eq!(col, 4);
        assert!(!right); // 33 % 8 = 1 → left half
    }

    #[test]
    fn selection_target_clamps_below_last_row_and_right_edge() {
        let rows = vec![0, 5];
        let (line, col, _) =
            filter_selection_target(&rows, 9999.0, 9999.0, 8.0, 10.0, 80, 0)
                .unwrap();
        assert_eq!(line, 5);
        assert_eq!(col, 79);
    }

    #[test]
    fn selection_target_applies_horizontal_offset() {
        let rows = vec![2];
        let (_, col, _) =
            filter_selection_target(&rows, 16.0, 0.0, 8.0, 10.0, 500, 100)
                .unwrap();
        assert_eq!(col, 102);
    }

    #[test]
    fn selection_target_right_side_past_half_cell() {
        let rows = vec![0];
        let (_, col, right) =
            filter_selection_target(&rows, 7.0, 0.0, 8.0, 10.0, 80, 0)
                .unwrap();
        assert_eq!(col, 0);
        assert!(right); // 7 > 4 → right half
    }

    #[test]
    fn selection_target_rejects_empty_or_degenerate() {
        assert!(filter_selection_target(&[], 5.0, 5.0, 8.0, 10.0, 80, 0)
            .is_none());
        assert!(filter_selection_target(&[1], 5.0, 5.0, 0.0, 10.0, 80, 0)
            .is_none());
        assert!(filter_selection_target(&[1], 5.0, 5.0, 8.0, 10.0, 0, 0)
            .is_none());
    }
}

#[cfg(test)]
mod wheel_tests {
    use super::{bank_wheel_pixels, TerminalViewState, ROWS_PER_NOTCH};
    use egui::{MouseWheelUnit, Vec2};

    #[test]
    fn bank_scales_a_line_notch_by_one_row() {
        let mut state = TerminalViewState::default();
        let notch_up = Vec2::new(0.0, 1.0);
        bank_wheel_pixels(
            &mut state,
            10.0,
            MouseWheelUnit::Line,
            notch_up,
            1.0,
        );
        assert_eq!(state.scroll_pixels, -10.0);
    }

    #[test]
    fn bank_scales_a_line_notch_by_rows_per_notch() {
        let mut state = TerminalViewState::default();
        let notch_up = Vec2::new(0.0, 1.0);
        bank_wheel_pixels(
            &mut state,
            10.0,
            MouseWheelUnit::Line,
            notch_up,
            ROWS_PER_NOTCH,
        );
        assert_eq!(state.scroll_pixels, -30.0);
    }
}
