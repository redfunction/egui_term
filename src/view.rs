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
    cached_shapes: Option<Vec<Shape>>,
    cached_rect: Option<Rect>,
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

        self.focus(&layout)
            .resize(&layout)
            .process_input(&layout, &mut state)
            .show(&mut state, &layout, &painter);

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
        }
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
        self.backend.process_command(BackendCommand::Resize(
            Size::from(layout.rect.size()),
            self.font.font_measure(&layout.ctx),
        ));

        self
    }

    fn process_input(
        self,
        layout: &Response,
        state: &mut TerminalViewState,
    ) -> Self {
        let has_focus = layout.has_focus();
        let has_pointer = layout.contains_pointer();

        if !has_focus && !has_pointer {
            return self;
        }

        // Scrollbar occupies the rightmost 8px of the layout
        let scrollbar_x = layout.rect.max.x - 8.0;

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
        painter: &Painter,
    ) {
        let has_search = self.search_regex.is_some();

        // Check if pointer is interacting with the scrollbar area
        let scrollbar_x = layout.rect.max.x - 8.0;
        let pointer_on_scrollbar = layout.ctx.input(|i| {
            if let Some(pos) = i.pointer.hover_pos() {
                (i.pointer.primary_pressed() || i.pointer.primary_down())
                    && pos.x >= scrollbar_x
                    && layout.rect.contains(pos)
            } else {
                false
            }
        });

        // Fast path: if terminal is not dirty, no scrollbar interaction,
        // no search (and none last frame), and we have a cached frame for the same rect, reuse it.
        if !self.backend.is_dirty()
            && !state.scrollbar_dragging
            && !pointer_on_scrollbar
            && !has_search
            && !state.had_highlights
            && state.cached_rect == Some(layout.rect)
        {
            if let Some(ref shapes) = state.cached_shapes {
                painter.extend(shapes.clone());
                return;
            }
        }

        // Single lock for both metadata sync and rendering
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

        // Compute visible search matches once (cheap — only visible viewport).
        // Store as sorted Vec of match ranges for binary-search lookup.
        let mut highlight_matches: Vec<std::ops::RangeInclusive<TerminalGridPoint>> = Vec::new();
        let mut current_match: Option<std::ops::RangeInclusive<TerminalGridPoint>> = None;
        if let Some(ref mut regex) = self.search_regex {
            for m in crate::backend::visible_regex_match_iter(&terminal, regex) {
                if self.current_match_start.as_ref() == Some(m.start()) {
                    current_match = Some(m);
                } else {
                    highlight_matches.push(m);
                }
            }
        }
        let has_any_highlights = current_match.is_some() || !highlight_matches.is_empty();

        let mut shapes = vec![Shape::Rect(RectShape::filled(
            Rect::from_min_max(layout_min, layout_max),
            CornerRadius::ZERO,
            global_bg,
        ))];

        for indexed in terminal.grid().display_iter() {
            let flags = indexed.cell.flags;
            let is_wide_char_spacer =
                flags.contains(cell::Flags::WIDE_CHAR_SPACER);
            if is_wide_char_spacer {
                continue;
            }

            let is_app_cursor_mode =
                content.terminal_mode.contains(TermMode::APP_CURSOR);
            let is_wide_char = flags.contains(cell::Flags::WIDE_CHAR);
            let is_inverse = flags.contains(cell::Flags::INVERSE);
            let is_dim =
                flags.intersects(cell::Flags::DIM | cell::Flags::DIM_BOLD);
            let is_selected = content
                .selectable_range
                .is_some_and(|r| r.contains(indexed.point));
            let is_hovered_hyperling =
                content.hovered_hyperlink.as_ref().is_some_and(|r| {
                    r.contains(&indexed.point)
                        && r.contains(&state.current_mouse_position_on_grid)
                });

            // Check search highlights
            let highlight_kind = if has_any_highlights {
                if current_match.as_ref().is_some_and(|r| r.contains(&indexed.point)) {
                    HighlightKind::Current
                } else if highlight_matches.iter().any(|r| r.contains(&indexed.point)) {
                    HighlightKind::Match
                } else {
                    HighlightKind::None
                }
            } else {
                HighlightKind::None
            };

            let x = layout_min.x + (cell_width * indexed.point.column.0 as f32);
            let line_num =
                indexed.point.line.0 + display_offset as i32;
            let y = layout_min.y + (cell_height * line_num as f32);

            let mut fg = self.theme.get_color(indexed.fg);
            let mut bg = self.theme.get_color(indexed.bg);
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

            if cursor_point == indexed.point && !self.hide_cursor {
                let cursor_color = self.theme.get_color(content.cursor.fg);
                shapes.push(Shape::Rect(RectShape::filled(
                    Rect::from_min_size(
                        Pos2::new(x, y),
                        Vec2::new(cell_width, cell_height),
                    ),
                    CornerRadius::default(),
                    cursor_color,
                )));
            }

            if indexed.c != ' ' && indexed.c != '\t' {
                if cursor_point == indexed.point
                    && is_app_cursor_mode
                    && !self.hide_cursor
                {
                    std::mem::swap(&mut fg, &mut bg);
                }

                shapes.push(painter.fonts_mut(|c| {
                    Shape::text(
                        c,
                        Pos2 {
                            x: x + (cell_width / 2.0),
                            y,
                        },
                        Align2::CENTER_TOP,
                        indexed.c,
                        self.font.font_type(),
                        fg,
                    )
                }));
            }
        }

        // Draw border around current search match
        if let Some(ref cm) = current_match {
            let cols = terminal.grid().columns();
            let start = *cm.start();
            let end = *cm.end();
            let border_color = egui::Color32::from_rgb(255, 180, 50);
            let stroke = Stroke::new(2.0, border_color);

            if start.line == end.line {
                // Single-line match: one border rect
                let x1 = layout_min.x + (cell_width * start.column.0 as f32);
                let x2 = layout_min.x + (cell_width * (end.column.0 as f32 + 1.0));
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
                    let x1 = layout_min.x + (cell_width * col_start as f32);
                    let x2 = layout_min.x + (cell_width * col_end as f32);
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
            let track_rect = Rect::from_min_max(
                Pos2::new(layout_max.x - scrollbar_width, layout_min.y),
                layout_max,
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

            // Draw scrollbar track
            shapes.push(Shape::Rect(RectShape::filled(
                track_rect,
                CornerRadius::same(4),
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 15),
            )));
            // Draw scrollbar thumb
            shapes.push(Shape::Rect(RectShape::filled(
                thumb_rect,
                CornerRadius::same(4),
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 80),
            )));
        } else {
            state.scrollbar_dragging = false;
        }

        drop(terminal);

        // Cache shapes for reuse when terminal is idle
        state.cached_shapes = Some(shapes.clone());
        state.cached_rect = Some(layout.rect);
        state.had_highlights = has_any_highlights;
        painter.extend(shapes);
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
    if let Some(key) = Key::from_name(text) {
        if bindings_layout.get_action(
            InputKind::KeyCode(key),
            modifiers,
            backend.last_content().terminal_mode,
        ) == BindingAction::Ignore
        {
            InputAction::BackendCall(BackendCommand::Write(
                text.as_bytes().to_vec(),
            ))
        } else {
            InputAction::Ignore
        }
    } else {
        InputAction::BackendCall(BackendCommand::Write(
            text.as_bytes().to_vec(),
        ))
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
