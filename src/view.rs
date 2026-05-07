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
            layout.request_focus();
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
                    input_actions.push(process_keyboard_event(
                        event,
                        self.backend,
                        &self.bindings_layout,
                        modifiers,
                    ))
                },
                // Mouse wheel: require pointer over widget
                egui::Event::MouseWheel { unit, delta, .. } => {
                    if !has_pointer {
                        continue;
                    }
                    input_actions.push(process_mouse_wheel(
                        state,
                        self.font.font_type().size,
                        unit,
                        delta,
                    ))
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
                },
                // Mouse move: require pointer over widget
                egui::Event::PointerMoved(pos) => {
                    if !has_pointer && !state.is_dragged {
                        continue;
                    }
                    if state.scrollbar_dragging || pos.x >= scrollbar_x {
                        continue;
                    }
                    input_actions = process_mouse_move(
                        state,
                        layout,
                        self.backend,
                        pos,
                        &modifiers,
                    )
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
        let key_layout = round_rect_int(layout.rect);
        let key_visible = round_rect_int(*visible_rect);
        let cache_key_matches = state.cached_rect == Some(key_layout)
            && state.cached_visible == Some(key_visible)
            && state.cached_h_offset == self.horizontal_offset_cols
            && state.cached_search_key == self.search_key
            && state.cached_current_match == self.current_match_start;

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
        const RENDER_THROTTLE: std::time::Duration =
            std::time::Duration::from_millis(33);
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
        let display_offset = content.display_offset;
        let cursor_point = content.cursor_point;

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
            let bounded =
                crate::backend::visible_regex_match_iter_in_cols(
                    &terminal, regex, scan_min, scan_max, 5,
                );
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
        let hide_cursor = self.hide_cursor;
        let theme = &self.theme;
        let mouse_pos = state.current_mouse_position_on_grid;
        painter.fonts_mut(|fonts| {
        for line_idx in 0..screen_lines_i32 {
            let viewport_line =
                alacritty_terminal::index::Line(line_idx - display_offset_i32);
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
                let line_num = viewport_line.0 + display_offset as i32;
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

            if is_inverse || is_selected {
                std::mem::swap(&mut fg, &mut bg);
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
                    shapes.push(Shape::Rect(RectShape::filled(
                        Rect::from_min_size(
                            Pos2::new(x, y),
                            Vec2::new(cell_width, cell_height),
                        ),
                        CornerRadius::default(),
                        cursor_color,
                    )));
                }

                if cell.c != ' ' && cell.c != '\t' {
                    if cursor_point == point
                        && is_app_cursor_mode
                        && !hide_cursor
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
            let stroke = Stroke::new(2.0, border_color);

            if start.line == end.line {
                // Single-line match: one border rect
                let x1 = layout_min.x
                    + (cell_width * (start.column.0 as i32 - h_offset_cols) as f32);
                let x2 = layout_min.x
                    + (cell_width
                        * (end.column.0 as i32 + 1 - h_offset_cols) as f32);
                let line_num = start.line.0 + display_offset as i32;
                let y = layout_min.y + (cell_height * line_num as f32);
                let rect = Rect::from_min_size(
                    Pos2::new(x1, y),
                    Vec2::new(x2 - x1, cell_height),
                );
                shapes.push(Shape::Rect(RectShape::new(rect, CornerRadius::same(2), egui::Color32::TRANSPARENT, stroke, egui::StrokeKind::Outside)));
            } else {
                // Multi-line match: border per line
                let mut line = start.line;
                while line <= end.line {
                    let line_num = line.0 + display_offset as i32;
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

        // Scrollbar
        let total_lines = terminal.grid().total_lines();
        let screen_lines = terminal.grid().screen_lines();
        let history_size = total_lines.saturating_sub(screen_lines);

        if history_size > 0 {
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
        state.last_render_at = Some(std::time::Instant::now());
        state.had_highlights = has_any_highlights;
        painter.extend(shapes);
    }
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

fn process_mouse_wheel(
    state: &mut TerminalViewState,
    font_size: f32,
    unit: MouseWheelUnit,
    delta: Vec2,
) -> InputAction {
    match unit {
        MouseWheelUnit::Line => {
            let lines = delta.y.signum() * delta.y.abs().ceil();
            InputAction::BackendCall(BackendCommand::Scroll(lines as i32))
        },
        MouseWheelUnit::Point => {
            state.scroll_pixels -= delta.y;
            let lines = (state.scroll_pixels / font_size).trunc();
            state.scroll_pixels %= font_size;
            if lines != 0.0 {
                InputAction::BackendCall(BackendCommand::Scroll(-lines as i32))
            } else {
                InputAction::Ignore
            }
        },
        MouseWheelUnit::Page => InputAction::Ignore,
    }
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
