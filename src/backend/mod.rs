pub mod settings;

use crate::socket_pty::SocketPty;
use crate::types::Size;
use alacritty_terminal::event::{
    Event, EventListener, Notify, OnResize, WindowSize,
};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{
    Selection, SelectionRange, SelectionType as AlacrittySelectionType,
};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::{
    self, cell::Cell, test::TermSize, viewport_to_point, Term, TermMode,
};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Processor};
use egui::Modifiers;
use settings::BackendSettings;
use std::borrow::Cow;
use std::cmp::min;
use std::io::Result;
use std::ops::{Index, RangeInclusive};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

/// Minimum interval between repaint requests (milliseconds).
/// This controls how often the *parent* window is asked to repaint.
/// When terminals live in deferred viewports, the parent repaint is
/// only needed for re-registering callbacks, not for actual rendering.
/// Higher value = less CPU. Terminal viewports self-refresh independently.
const REPAINT_THROTTLE_MS: u64 = 1000;

/// Global repaint throttle shared by every terminal backend.
static GLOBAL_LAST_REPAINT_MS: AtomicU64 = AtomicU64::new(0);

/// Monotonic millisecond timestamp for repaint throttling.
fn now_ms() -> u64 {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    epoch.elapsed().as_millis() as u64
}

/// Request a repaint only if enough time has passed since the last global request.
/// All terminals share one throttle so N terminals don't cause N× repaints.
fn throttled_repaint(ctx: &egui::Context) {
    let now = now_ms();
    let prev = GLOBAL_LAST_REPAINT_MS.load(Ordering::Relaxed);
    if now.saturating_sub(prev) >= REPAINT_THROTTLE_MS {
        GLOBAL_LAST_REPAINT_MS.store(now, Ordering::Relaxed);
        ctx.request_repaint_after(std::time::Duration::from_millis(REPAINT_THROTTLE_MS));
    }
}

pub type TerminalMode = TermMode;
pub type PtyEvent = Event;
pub type SelectionType = AlacrittySelectionType;

#[derive(Debug, Clone)]
pub enum BackendCommand {
    Write(Vec<u8>),
    Scroll(i32),
    Resize(Size, Size),
    SelectStart(SelectionType, f32, f32),
    SelectUpdate(f32, f32),
    /// Selection anchored at an explicit grid point. Used by the
    /// grep/filter render mode, where the pixel→grid mapping goes
    /// through the remapped row list and can't be derived from
    /// (x, y) + display_offset like `SelectStart` does.
    SelectStartAtPoint(SelectionType, Point, Side),
    /// Grid-point counterpart of `SelectUpdate` (see above).
    SelectUpdateAtPoint(Point, Side),
    ProcessLink(LinkAction, Point),
    MouseReport(MouseButton, Modifiers, Point, bool),
}

#[derive(Debug, Clone)]
pub enum MouseMode {
    Sgr,
    Normal(bool),
}

impl From<TermMode> for MouseMode {
    fn from(term_mode: TermMode) -> Self {
        if term_mode.contains(TermMode::SGR_MOUSE) {
            MouseMode::Sgr
        } else if term_mode.contains(TermMode::UTF8_MOUSE) {
            MouseMode::Normal(true)
        } else {
            MouseMode::Normal(false)
        }
    }
}

#[derive(Debug, Clone)]
pub enum MouseButton {
    LeftButton = 0,
    MiddleButton = 1,
    RightButton = 2,
    LeftMove = 32,
    MiddleMove = 33,
    RightMove = 34,
    NoneMove = 35,
    ScrollUp = 64,
    ScrollDown = 65,
    Other = 99,
}

#[derive(Debug, Clone)]
pub enum LinkAction {
    Clear,
    Hover,
    Open,
}

#[derive(Clone, Copy, Debug)]
pub struct TerminalSize {
    pub cell_width: u16,
    pub cell_height: u16,
    num_cols: u16,
    num_lines: u16,
    layout_size: Size,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            cell_width: 1,
            cell_height: 1,
            num_cols: 80,
            num_lines: 50,
            layout_size: Size::default(),
        }
    }
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.num_lines as usize
    }

    fn columns(&self) -> usize {
        self.num_cols as usize
    }

    fn last_column(&self) -> Column {
        Column(self.num_cols as usize - 1)
    }

    fn bottommost_line(&self) -> Line {
        Line(self.num_lines as i32 - 1)
    }
}

impl From<TerminalSize> for WindowSize {
    fn from(size: TerminalSize) -> Self {
        Self {
            num_lines: size.num_lines,
            num_cols: size.num_cols,
            cell_width: size.cell_width,
            cell_height: size.cell_height,
        }
    }
}

enum Sink {
    EventLoop { notifier: Notifier },
    Channel {
        input_tx: mpsc::Sender<Vec<u8>>,
        resize_tx: mpsc::Sender<WindowSize>,
    },
}

impl Sink {
    fn notify<I: Into<Cow<'static, [u8]>>>(&self, input: I) {
        match self {
            Sink::EventLoop { notifier } => notifier.notify(input),
            Sink::Channel { input_tx, .. } => {
                let _ = input_tx.send(input.into().into_owned());
            }
        }
    }

    fn on_resize(&mut self, size: WindowSize) {
        match self {
            Sink::EventLoop { notifier } => notifier.on_resize(size),
            Sink::Channel { resize_tx, .. } => {
                let _ = resize_tx.send(size);
            }
        }
    }

    fn shutdown(&self) {
        match self {
            Sink::EventLoop { notifier } => {
                let _ = notifier.0.send(Msg::Shutdown);
            }
            Sink::Channel { .. } => {}
        }
    }
}

pub struct TerminalBackend {
    id: u64,
    pty_id: u32,
    url_regex: RegexSearch,
    term: Arc<FairMutex<Term<EventProxy>>>,
    size: TerminalSize,
    sink: Sink,
    last_content: RenderableContent,
    dirty: Arc<AtomicBool>,
    /// Monotonic counter incremented every time content is written or
    /// scrolled. Unlike `dirty` (which the renderer clears each
    /// frame), this keeps growing — callers can snapshot it and
    /// compare later to know whether anything changed in between
    /// without polling per-frame. Used by the periodic search
    /// refresh: skip the scan if the buffer hasn't moved since the
    /// last search ran.
    content_revision: Arc<AtomicU64>,
    /// Shared flag for sticky-scroll (log mode): keep view stable as new content arrives.
    sticky_scroll: Option<Arc<AtomicBool>>,
}

impl TerminalBackend {
    pub fn new(
        id: u64,
        app_context: egui::Context,
        pty_event_proxy_sender: Sender<(u64, PtyEvent)>,
        settings: BackendSettings,
    ) -> Result<Self> {
        let pty_config = tty::Options {
            shell: Some(tty::Shell::new(settings.shell, settings.args)),
            working_directory: settings.working_directory,
            // Merge host-provided env (e.g. KUBECONFIG) into the child's
            // inherited environment — custom values win on both platforms.
            env: settings.env,
            // Escape Windows args per C-runtime rules so multi-word args
            // (our `--settings {json}` / prompts) survive CreateProcessW.
            // The field only exists on Windows.
            #[cfg(target_os = "windows")]
            escape_args: true,
            ..tty::Options::default()
        };
        let config = term::Config::default();
        let terminal_size = TerminalSize::default();
        let pty = tty::new(&pty_config, terminal_size.into(), id)?;
        #[cfg(not(windows))]
        let pty_id = pty.child().id();
        #[cfg(windows)]
        let pty_id = pty
            .child_watcher()
            .pid()
            .ok_or(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Failed to get child process ID",
            ))?
            .into();
        let dirty = Arc::new(AtomicBool::new(true));
        let (event_sender, event_receiver) = mpsc::channel();
        let event_proxy = EventProxy { sender: event_sender, dirty: dirty.clone() };
        let mut term = Term::new(config, &terminal_size, event_proxy.clone());
        let initial_content = RenderableContent {
            display_offset: term.grid().display_offset(),
            cursor_point: term.grid().cursor.point,
            selectable_range: None,
            terminal_mode: *term.mode(),
            terminal_size,
            cursor: term.grid_mut().cursor_cell().clone(),
            hovered_hyperlink: None,
        };
        let term = Arc::new(FairMutex::new(term));
        let pty_event_loop =
            EventLoop::new(term.clone(), event_proxy, pty, false, false)?;
        let notifier = Notifier(pty_event_loop.channel());
        let pty_notifier = Notifier(pty_event_loop.channel());
        let url_regex = RegexSearch::new(r#"(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file://|git://|ssh:|ftp://)[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>"\s{-}\^⟨⟩`]+"#).unwrap();
        let _pty_event_loop_thread = pty_event_loop.spawn();
        let _pty_event_subscription = std::thread::Builder::new()
            .name(format!("pty_event_subscription_{}", id))
            .spawn(move || loop {
                match event_receiver.recv() {
                    Ok(event) => {
                        let _ = pty_event_proxy_sender.send((id, event.clone()));
                        throttled_repaint(&app_context);
                        match event {
                            Event::Exit => break,
                            Event::PtyWrite(pty) => pty_notifier.notify(pty.into_bytes()),
                            _ => {}
                        }
                    }
                    Err(_) => break,
                }
            })?;

        Ok(Self {
            id,
            pty_id,
            url_regex,
            term: term.clone(),
            size: terminal_size,
            sink: Sink::EventLoop { notifier },
            last_content: initial_content,
            dirty,
            content_revision: Arc::new(AtomicU64::new(0)),
            sticky_scroll: None,
        })
    }

    /// Create a backend backed by a Unix socket pair instead of a real PTY.
    ///
    /// Returns `(TerminalBackend, StreamHandle)` where `StreamHandle` contains
    /// the other end of the socket and a resize receiver that the caller bridges
    /// to an external data source (e.g. kube-rs exec/logs).
    pub fn new_streaming(
        id: u64,
        app_context: egui::Context,
        pty_event_proxy_sender: Sender<(u64, PtyEvent)>,
    ) -> Result<(Self, StreamHandle)> {
        let (stream_a, stream_b) = crate::socket_pty::tcp_stream_pair()?;
        let (resize_tx, resize_rx) = std::sync::mpsc::channel();
        let socket_pty = SocketPty::new(stream_a, resize_tx);

        let dirty = Arc::new(AtomicBool::new(true));
        let config = term::Config::default();
        let terminal_size = TerminalSize::default();
        let (event_sender, event_receiver) = mpsc::channel();
        let event_proxy = EventProxy { sender: event_sender, dirty: dirty.clone() };
        let mut term = Term::new(config, &terminal_size, event_proxy.clone());
        let initial_content = RenderableContent {
            display_offset: term.grid().display_offset(),
            cursor_point: term.grid().cursor.point,
            selectable_range: None,
            terminal_mode: *term.mode(),
            terminal_size,
            cursor: term.grid_mut().cursor_cell().clone(),
            hovered_hyperlink: None,
        };
        let term = Arc::new(FairMutex::new(term));
        let pty_event_loop =
            EventLoop::new(term.clone(), event_proxy, socket_pty, false, false)?;
        let notifier = Notifier(pty_event_loop.channel());
        let pty_notifier = Notifier(pty_event_loop.channel());
        let url_regex = RegexSearch::new(r#"(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file://|git://|ssh:|ftp://)[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>"\s{-}\^⟨⟩`]+"#).unwrap();
        let _pty_event_loop_thread = pty_event_loop.spawn();
        let _pty_event_subscription = std::thread::Builder::new()
            .name(format!("pty_event_subscription_{}", id))
            .spawn(move || loop {
                match event_receiver.recv() {
                    Ok(event) => {
                        let _ = pty_event_proxy_sender.send((id, event.clone()));
                        throttled_repaint(&app_context);
                        match event {
                            Event::Exit => break,
                            Event::PtyWrite(pty) => pty_notifier.notify(pty.into_bytes()),
                            _ => {}
                        }
                    }
                    Err(_) => break,
                }
            })?;

        let handle = StreamHandle {
            stream: stream_b,
            resize_rx,
        };

        Ok((
            Self {
                id,
                pty_id: 0,
                url_regex,
                term: term.clone(),
                size: terminal_size,
                sink: Sink::EventLoop { notifier },
                last_content: initial_content,
                dirty,
                content_revision: Arc::new(AtomicU64::new(0)),
                sticky_scroll: None,
                },
            handle,
        ))
    }

    /// Create a backend that writes directly to the terminal grid,
    /// with no socket, no EventLoop, and no Notifier.
    ///
    /// Returns `(TerminalBackend, DirectHandle)` where `DirectHandle` contains
    /// a `DirectWriter` for feeding data into the grid, plus receivers for
    /// user input and resize events.
    pub fn new_direct(
        id: u64,
        app_context: egui::Context,
        pty_event_proxy_sender: Sender<(u64, PtyEvent)>,
    ) -> Result<(Self, DirectHandle)> {
        let dirty = Arc::new(AtomicBool::new(true));
        let config = term::Config::default();
        let terminal_size = TerminalSize::default();
        let (event_sender, event_receiver) = mpsc::channel();
        let event_proxy = EventProxy { sender: event_sender, dirty: dirty.clone() };
        let mut term = Term::new(config, &terminal_size, event_proxy.clone());
        let initial_content = RenderableContent {
            display_offset: term.grid().display_offset(),
            cursor_point: term.grid().cursor.point,
            selectable_range: None,
            terminal_mode: *term.mode(),
            terminal_size,
            cursor: term.grid_mut().cursor_cell().clone(),
            hovered_hyperlink: None,
        };
        let term = Arc::new(FairMutex::new(term));
        let url_regex = RegexSearch::new(r#"(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file://|git://|ssh:|ftp://)[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>"\s{-}\^⟨⟩`]+"#).unwrap();

        let (input_tx, input_rx) = mpsc::channel();
        let (resize_tx, resize_rx) = mpsc::channel();

        let processor = Processor::new();

        let sticky_scroll = Arc::new(AtomicBool::new(false));
        let content_revision = Arc::new(AtomicU64::new(0));
        let writer = DirectWriter {
            inner: Arc::new(DirectWriterInner {
                term: term.clone(),
                processor: Mutex::new(processor),
                app_context: app_context.clone(),
                dirty: dirty.clone(),
                content_revision: content_revision.clone(),
                sticky_scroll: sticky_scroll.clone(),
            }),
        };

        // Route PtyWrite events to input_tx
        let input_tx_for_events = input_tx.clone();
        let _pty_event_subscription = std::thread::Builder::new()
            .name(format!("pty_event_subscription_{}", id))
            .spawn(move || loop {
                match event_receiver.recv() {
                    Ok(event) => {
                        let _ = pty_event_proxy_sender.send((id, event.clone()));
                        throttled_repaint(&app_context);
                        match event {
                            Event::Exit => break,
                            Event::PtyWrite(data) => {
                                let _ = input_tx_for_events.send(data.into_bytes());
                            }
                            _ => {}
                        }
                    }
                    Err(_) => break, // Channel disconnected, stop thread
                }
            })?;

        let handle = DirectHandle {
            writer: writer.clone(),
            input_rx,
            resize_rx,
        };

        Ok((
            Self {
                id,
                pty_id: 0,
                url_regex,
                term: term.clone(),
                size: terminal_size,
                sink: Sink::Channel { input_tx, resize_tx },
                last_content: initial_content,
                dirty,
                content_revision,
                sticky_scroll: Some(sticky_scroll),
            },
            handle,
        ))
    }

    pub fn process_command(&mut self, cmd: BackendCommand) {
        let term = self.term.clone();
        let mut term = term.lock();
        match cmd {
            BackendCommand::Write(input) => {
                self.dirty.store(true, Ordering::Release);
                self.content_revision.fetch_add(1, Ordering::Relaxed);
                self.write(input);
                term.scroll_display(Scroll::Bottom);
            },
            BackendCommand::Scroll(delta) => {
                // `dirty` triggers a re-render so the new viewport
                // band gets painted, but the underlying grid bytes
                // didn't change — match Points are still where they
                // were. Don't bump `content_revision`, otherwise
                // mouse-wheel scrolling kicks `search_all` once a
                // second for no reason.
                self.dirty.store(true, Ordering::Release);
                self.scroll(&mut term, delta);
            },
            BackendCommand::Resize(layout_size, font_size) => {
                // resize() sets dirty only when grid dimensions change
                self.resize(&mut term, layout_size, font_size);
            },
            BackendCommand::SelectStart(selection_type, x, y) => {
                self.dirty.store(true, Ordering::Release);
                self.start_selection(&mut term, selection_type, x, y);
            },
            BackendCommand::SelectUpdate(x, y) => {
                self.dirty.store(true, Ordering::Release);
                self.update_selection(&mut term, x, y);
            },
            BackendCommand::SelectStartAtPoint(selection_type, point, side) => {
                self.dirty.store(true, Ordering::Release);
                term.selection =
                    Some(Selection::new(selection_type, point, side));
            },
            BackendCommand::SelectUpdateAtPoint(point, side) => {
                self.dirty.store(true, Ordering::Release);
                if let Some(ref mut selection) = term.selection {
                    selection.update(point, side);
                }
            },
            BackendCommand::ProcessLink(link_action, point) => {
                self.dirty.store(true, Ordering::Release);
                self.process_link_action(&term, link_action, point);
            },
            BackendCommand::MouseReport(button, modifiers, point, pressed) => {
                self.dirty.store(true, Ordering::Release);
                self.process_mouse_report(button, modifiers, point, pressed);
            },
        };
    }

    pub fn selection_point(
        x: f32,
        y: f32,
        terminal_size: &TerminalSize,
        display_offset: usize,
    ) -> Point {
        let col = (x as usize) / (terminal_size.cell_width as usize);
        let col = min(Column(col), Column(terminal_size.num_cols as usize - 1));

        let line = (y as usize) / (terminal_size.cell_height as usize);
        let line = min(line, terminal_size.num_lines as usize - 1);

        viewport_to_point(display_offset, Point::new(line, col))
    }

    /// Extract all text from the terminal buffer (history + visible screen).
    pub fn full_text(&self) -> String {
        let terminal = self.term.lock();
        let total_lines = terminal.grid().total_lines();
        let screen_lines = terminal.grid().screen_lines();
        let history = total_lines.saturating_sub(screen_lines);
        let columns = terminal.grid().columns();

        let mut result = String::new();
        let start_line = -(history as i32);
        let end_line = screen_lines as i32 - 1;

        for line_idx in start_line..=end_line {
            let line = Line(line_idx);
            let mut row_text = String::new();
            for col in 0..columns {
                let point = Point::new(line, Column(col));
                let cell = &terminal.grid()[point];
                row_text.push(cell.c);
            }
            // Check if this row wraps to the next (no newline between them)
            let last_cell = &terminal.grid()[Point::new(line, Column(columns - 1))];
            let is_wrapped = last_cell.flags.contains(term::cell::Flags::WRAPLINE);

            let trimmed = row_text.trim_end();
            result.push_str(trimmed);
            if !is_wrapped {
                result.push('\n');
            }
        }
        // Remove trailing empty lines
        while result.ends_with("\n\n") {
            result.pop();
        }
        result
    }

    /// The whole buffer as logical lines (wrapped rows joined,
    /// wide-char spacers skipped), each carrying minimal ANSI SGR
    /// sequences that reconstruct foreground color and bold/dim.
    /// Trailing empty lines are dropped. Feeding these back through
    /// `feed_display` reproduces the buffer content with its log
    /// coloring intact — used for local buffer transforms such as
    /// reversing line order for a newest-first log view without
    /// refetching from the network.
    pub fn text_ansi_lines(&self) -> Vec<String> {
        let terminal = self.term.lock();
        let grid = terminal.grid();
        let total_lines = grid.total_lines();
        let screen_lines = grid.screen_lines();
        let history = total_lines.saturating_sub(screen_lines);
        let columns = grid.columns();

        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        // Byte length of `current` up to (and including) the last
        // non-blank cell — used to trim trailing padding without
        // slicing through escape sequences.
        let mut keep_len = 0usize;
        // (fg, bold, dim) last emitted; None = terminal default.
        let mut sgr_state: Option<(AnsiColor, bool, bool)> = None;

        let start_line = -(history as i32);
        let end_line = screen_lines as i32 - 1;
        for line_idx in start_line..=end_line {
            let line = Line(line_idx);
            for col in 0..columns {
                let cell = &grid[Point::new(line, Column(col))];
                if cell.flags.contains(term::cell::Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                // DIM_BOLD is the composite (DIM | BOLD) bits, so
                // plain `contains` on the single flags is correct
                // for both.
                let bold = cell.flags.contains(term::cell::Flags::BOLD);
                let dim = cell.flags.contains(term::cell::Flags::DIM);
                let want = sgr_for(cell.fg, bold, dim);
                if want != sgr_state {
                    // Reset, then re-apply the new attributes.
                    current.push_str("\x1b[0m");
                    if let Some((fg, b, d)) = want {
                        current.push_str(&sgr_codes(fg, b, d));
                    }
                    sgr_state = want;
                }
                current.push(cell.c);
                if cell.c != ' ' {
                    keep_len = current.len();
                }
            }
            let wraps = grid[Point::new(line, Column(columns - 1))]
                .flags
                .contains(term::cell::Flags::WRAPLINE);
            if !wraps {
                current.truncate(keep_len);
                if sgr_state.is_some() {
                    current.push_str("\x1b[0m");
                }
                lines.push(std::mem::take(&mut current));
                keep_len = 0;
                sgr_state = None;
            }
        }
        if !current.is_empty() {
            current.truncate(keep_len);
            lines.push(current);
        }
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines
    }

    /// Feed bytes straight into the terminal grid — the same display
    /// path a `DirectWriter` uses — for embedders that only hold the
    /// backend. Used for local buffer transforms (clear + rewrite);
    /// a fresh escape parser is used per call, so feed complete,
    /// self-contained chunks.
    pub fn feed_display(&mut self, data: &[u8]) {
        let mut term = self.term.lock();
        let mut processor: Processor = Processor::new();
        processor.advance(&mut *term, data);
        drop(term);
        self.dirty.store(true, Ordering::Release);
        self.content_revision.fetch_add(1, Ordering::Relaxed);
    }

    pub fn selectable_content(&self) -> String {
        let content = self.last_content();
        let mut result = String::new();
        if let Some(range) = content.selectable_range {
            let terminal = self.term.lock();
            let columns = terminal.grid().columns();
            let start = range.start;
            let end = range.end;

            // Iterate line by line from start.line to end.line
            let mut line = start.line;
            while line <= end.line {
                let col_start = if line == start.line { start.column.0 } else { 0 };
                let col_end = if line == end.line { end.column.0 + 1 } else { columns };

                let mut row_text = String::new();
                for col in col_start..col_end {
                    let point = Point::new(line, Column(col));
                    let cell = &terminal.grid()[point];
                    row_text.push(cell.c);
                }

                let trimmed = row_text.trim_end();
                result.push_str(trimmed);

                // Add newline unless this row wraps to the next, or it's the last line
                if line < end.line {
                    let last_cell = &terminal.grid()[Point::new(line, Column(columns - 1))];
                    let is_wrapped = last_cell.flags.contains(term::cell::Flags::WRAPLINE);
                    if !is_wrapped {
                        result.push('\n');
                    }
                }

                line += 1i32;
            }
        }
        result
    }

    /// Like `selectable_content`, but only lines present in
    /// `keep_lines` (sorted raw `Line.0` values) contribute to the
    /// result. Grep/filter mode uses this so a selection spanning
    /// hidden rows copies exactly what the user sees. Each kept row
    /// becomes its own output line — wrap-joining is deliberately
    /// skipped, since the wrapped continuation may be hidden.
    pub fn selectable_content_filtered(&self, keep_lines: &[i32]) -> String {
        let content = self.last_content();
        let mut result = String::new();
        if let Some(range) = content.selectable_range {
            let terminal = self.term.lock();
            let columns = terminal.grid().columns();
            let start = range.start;
            let end = range.end;
            let mut first = true;
            let mut line = start.line;
            while line <= end.line {
                if keep_lines.binary_search(&line.0).is_err() {
                    line += 1i32;
                    continue;
                }
                let col_start = if line == start.line { start.column.0 } else { 0 };
                let col_end = if line == end.line { end.column.0 + 1 } else { columns };

                let mut row_text = String::new();
                for col in col_start..col_end {
                    let point = Point::new(line, Column(col));
                    let cell = &terminal.grid()[point];
                    row_text.push(cell.c);
                }
                if !first {
                    result.push('\n');
                }
                first = false;
                result.push_str(row_text.trim_end());
                line += 1i32;
            }
        }
        result
    }

    /// Text of the given grid lines (sorted raw `Line.0` values),
    /// out-of-range lines skipped. Grep/filter mode's "copy all" /
    /// "save filtered log".
    ///
    /// Grep semantics: each kept row is expanded to its full
    /// LOGICAL line — wrapped segments are walked back to the line
    /// start and forward to its end (WRAPLINE flags) and joined
    /// without breaks, and a logical line is emitted once even when
    /// several of its visual rows are in `keep_lines`. Saving a
    /// filtered log therefore yields complete unbroken lines like
    /// `grep` would, not viewport-width fragments.
    pub fn lines_text(&self, keep_lines: &[i32]) -> String {
        let terminal = self.term.lock();
        let topmost = terminal.topmost_line().0;
        let bottommost = terminal.bottommost_line().0;
        let columns = terminal.grid().columns();
        let row_wraps = |row: i32| -> bool {
            terminal.grid()[Point::new(Line(row), Column(columns - 1))]
                .flags
                .contains(term::cell::Flags::WRAPLINE)
        };
        let mut result = String::new();
        let mut first = true;
        // Last visual row consumed by an emitted logical line — kept
        // rows at or before it belong to that same line.
        let mut emitted_through: Option<i32> = None;
        for &l in keep_lines {
            if l < topmost || l > bottommost {
                continue;
            }
            if emitted_through.is_some_and(|e| l <= e) {
                continue;
            }
            let mut start = l;
            while start > topmost && row_wraps(start - 1) {
                start -= 1;
            }
            let mut end = l;
            while end < bottommost && row_wraps(end) {
                end += 1;
            }
            let mut line_text = String::new();
            for row in start..=end {
                for col in 0..columns {
                    let point = Point::new(Line(row), Column(col));
                    line_text.push(terminal.grid()[point].c);
                }
            }
            if !first {
                result.push('\n');
            }
            first = false;
            result.push_str(line_text.trim_end());
            emitted_through = Some(end);
        }
        result
    }

    /// Update metadata from a terminal that is already locked.
    /// Returns true if there was new content to sync.
    pub fn sync_with_term(&mut self, terminal: &mut Term<EventProxy>) -> bool {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return false;
        }
        self.last_content.selectable_range = match &terminal.selection {
            Some(s) => s.to_range(terminal),
            None => None,
        };
        self.last_content.display_offset = terminal.grid().display_offset();
        self.last_content.cursor_point = terminal.grid().cursor.point;
        self.last_content.cursor = terminal.grid_mut().cursor_cell().clone();
        self.last_content.terminal_mode = *terminal.mode();
        self.last_content.terminal_size = self.size;
        true
    }

    /// Lock the terminal for direct grid access (used for rendering).
    pub fn term(&self) -> &Arc<FairMutex<Term<EventProxy>>> {
        &self.term
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    /// Snapshot of the content-revision counter. Bumped on any
    /// write/scroll. Callers compare two snapshots to know whether
    /// content changed in between, *without* polling per-frame the
    /// way `is_dirty` requires (since `is_dirty` is cleared by the
    /// renderer each frame).
    pub fn content_revision(&self) -> u64 {
        self.content_revision.load(Ordering::Relaxed)
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    /// Enable sticky-scroll mode (log mode): when the user scrolls up,
    /// new content won't auto-scroll the view down.
    pub fn set_sticky_scroll(&self, enabled: bool) {
        if let Some(ref ss) = self.sticky_scroll {
            ss.store(enabled, Ordering::Relaxed);
        }
    }

    /// Scroll the terminal to the very top of the scrollback history.
    pub fn scroll_to_top(&self) {
        let mut terminal = self.term.lock();
        terminal.grid_mut().scroll_display(Scroll::Top);
        self.dirty.store(true, Ordering::Release);
    }

    /// Check if the terminal is scrolled to the bottom.
    pub fn is_at_bottom(&self) -> bool {
        let terminal = self.term.lock();
        terminal.grid().display_offset() == 0
    }

    pub fn last_content(&self) -> &RenderableContent {
        &self.last_content
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn pty_id(&self) -> u32 {
        self.pty_id
    }

    /// Search the entire scrollback + screen for a regex pattern.
    /// Returns all matches as `RangeInclusive<Point>`.
    pub fn search_all(&self, regex: &mut RegexSearch) -> Vec<Match> {
        let terminal = self.term.lock();
        search_all_in_term(&terminal, regex)
    }

    /// Clone the underlying term `Arc<FairMutex<Term<_>>>` so a
    /// caller can run `search_all_in_term_arc` from a worker
    /// thread without holding any of `egui_term`'s state. Used by
    /// Kubezilla's async search path which moves the full-buffer
    /// regex scan off the UI thread.
    pub fn term_arc(
        &self,
    ) -> std::sync::Arc<alacritty_terminal::sync::FairMutex<alacritty_terminal::Term<EventProxy>>>
    {
        self.term.clone()
    }

    /// Search only the visible viewport for a regex pattern.
    pub fn search_visible(&self, regex: &mut RegexSearch) -> Vec<Match> {
        let terminal = self.term.lock();
        visible_regex_match_iter(&terminal, regex).collect()
    }

    /// Convenience snapshot of grid dimensions for callers that
    /// don't want to depend on alacritty's `Dimensions` trait
    /// directly. Returns `(display_offset, screen_lines, columns,
    /// total_lines, bottommost_line_index)` — everything the
    /// async navigation paths need to compute viewport bounds.
    pub fn grid_bounds(&self) -> GridBounds {
        let terminal = self.term.lock();
        let grid = terminal.grid();
        GridBounds {
            display_offset: grid.display_offset() as i32,
            screen_lines: grid.screen_lines() as i32,
            columns: grid.columns(),
            total_lines: grid.total_lines(),
            bottommost_line: terminal.bottommost_line().0,
        }
    }

    /// Find the next regex match in the live grid starting *after*
    /// `from`. Used by F3 / Enter navigation so we don't rely on
    /// stale match `Point`s cached from an earlier `search_all`.
    /// Returns `None` if no match exists between `from` and the
    /// bottom of history.
    ///
    /// `from` is clamped into the current grid bounds before the
    /// regex iterator runs — alacritty's `Grid::index` panics on
    /// out-of-range Points, and stale anchors from streaming
    /// content can easily produce them.
    pub fn next_match_after(
        &self,
        regex: &mut RegexSearch,
        from: Point,
    ) -> Option<Match> {
        let terminal = self.term.lock();
        let bottom = terminal.bottommost_line();
        let total_lines = terminal.grid().total_lines();
        let screen_lines = terminal.grid().screen_lines();
        let history = total_lines.saturating_sub(screen_lines) as i32;
        let last_col = terminal.grid().columns().saturating_sub(1);
        let from = Point::new(
            Line(from.line.0.clamp(-history, bottom.0)),
            Column(from.column.0.min(last_col)),
        );
        let end = Point::new(bottom, Column(last_col));
        terminal.regex_search_right(regex, from, end)
    }

    /// Mirror of `next_match_after` for backward navigation.
    pub fn prev_match_before(
        &self,
        regex: &mut RegexSearch,
        from: Point,
    ) -> Option<Match> {
        let terminal = self.term.lock();
        let total_lines = terminal.grid().total_lines();
        let screen_lines = terminal.grid().screen_lines();
        let history = total_lines.saturating_sub(screen_lines) as i32;
        let bottom = terminal.bottommost_line().0;
        let last_col = terminal.grid().columns().saturating_sub(1);
        let from = Point::new(
            Line(from.line.0.clamp(-history, bottom)),
            Column(from.column.0.min(last_col)),
        );
        let top = Line(-history);
        let start = Point::new(top, Column(0));
        terminal.regex_search_left(regex, from, start)
    }

    /// Scroll the terminal so the given point is visible.
    pub fn scroll_to_point(&self, point: Point) {
        let mut terminal = self.term.lock();
        let screen_lines = terminal.grid().screen_lines();
        let display_offset = terminal.grid().display_offset();
        let viewport_start = Line(-(display_offset as i32));
        let viewport_end = viewport_start + Line(screen_lines as i32 - 1);
        if point.line < viewport_start {
            // Need to scroll up (increase display_offset)
            let delta = viewport_start.0 - point.line.0;
            terminal.grid_mut().scroll_display(Scroll::Delta(delta));
            self.dirty.store(true, Ordering::Release);
        } else if point.line > viewport_end {
            // Need to scroll down (decrease display_offset)
            let delta = point.line.0 - viewport_end.0;
            terminal.grid_mut().scroll_display(Scroll::Delta(-delta));
            self.dirty.store(true, Ordering::Release);
        }
    }

    fn process_link_action(
        &mut self,
        terminal: &Term<EventProxy>,
        link_action: LinkAction,
        point: Point,
    ) {
        match link_action {
            LinkAction::Hover => {
                self.last_content.hovered_hyperlink = self.regex_match_at(
                    terminal,
                    point,
                    &mut self.url_regex.clone(),
                );
            },
            LinkAction::Clear => {
                self.last_content.hovered_hyperlink = None;
            },
            LinkAction::Open => {
                self.open_link(terminal);
            },
        };
    }

    fn open_link(&self, terminal: &Term<EventProxy>) {
        if let Some(range) = &self.last_content.hovered_hyperlink {
            let start = range.start();
            let end = range.end();

            let mut url = String::from(terminal.grid().index(*start).c);
            for indexed in terminal.grid().iter_from(*start) {
                url.push(indexed.c);
                if indexed.point == *end {
                    break;
                }
            }

            open::that(url).unwrap_or_else(|_| {
                panic!("link opening is failed");
            })
        }
    }

    fn process_mouse_report(
        &self,
        button: MouseButton,
        modifiers: Modifiers,
        point: Point,
        pressed: bool,
    ) {
        let mut mods = 0;
        if modifiers.contains(Modifiers::SHIFT) {
            mods += 4;
        }
        if modifiers.contains(Modifiers::ALT) {
            mods += 8;
        }
        if modifiers.contains(Modifiers::COMMAND) {
            mods += 16;
        }

        match MouseMode::from(self.last_content().terminal_mode) {
            MouseMode::Sgr => {
                self.sgr_mouse_report(point, button as u8 + mods, pressed)
            },
            MouseMode::Normal(is_utf8) => {
                if pressed {
                    self.normal_mouse_report(
                        point,
                        button as u8 + mods,
                        is_utf8,
                    )
                } else {
                    self.normal_mouse_report(point, 3 + mods, is_utf8)
                }
            },
        }
    }

    fn sgr_mouse_report(&self, point: Point, button: u8, pressed: bool) {
        let c = if pressed { 'M' } else { 'm' };

        let msg = format!(
            "\x1b[<{};{};{}{}",
            button,
            point.column + 1,
            point.line + 1,
            c
        );

        self.sink.notify(msg.as_bytes().to_vec());
    }

    fn normal_mouse_report(&self, point: Point, button: u8, is_utf8: bool) {
        let Point { line, column } = point;
        let max_point = if is_utf8 { 2015 } else { 223 };

        if line >= max_point || column >= max_point {
            return;
        }

        let mut msg = vec![b'\x1b', b'[', b'M', 32 + button];

        let mouse_pos_encode = |pos: usize| -> Vec<u8> {
            let pos = 32 + 1 + pos;
            let first = 0xC0 + pos / 64;
            let second = 0x80 + (pos & 63);
            vec![first as u8, second as u8]
        };

        if is_utf8 && column >= Column(95) {
            msg.append(&mut mouse_pos_encode(column.0));
        } else {
            msg.push(32 + 1 + column.0 as u8);
        }

        if is_utf8 && line >= 95 {
            msg.append(&mut mouse_pos_encode(line.0 as usize));
        } else {
            msg.push(32 + 1 + line.0 as u8);
        }

        self.sink.notify(msg);
    }

    fn start_selection(
        &mut self,
        terminal: &mut Term<EventProxy>,
        selection_type: SelectionType,
        x: f32,
        y: f32,
    ) {
        let location = Self::selection_point(
            x,
            y,
            &self.size,
            terminal.grid().display_offset(),
        );
        terminal.selection = Some(Selection::new(
            selection_type,
            location,
            self.selection_side(x),
        ));
    }

    fn update_selection(
        &mut self,
        terminal: &mut Term<EventProxy>,
        x: f32,
        y: f32,
    ) {
        let display_offset = terminal.grid().display_offset();
        if let Some(ref mut selection) = terminal.selection {
            let location =
                Self::selection_point(x, y, &self.size, display_offset);
            selection.update(location, self.selection_side(x));
        }
    }

    fn selection_side(&self, x: f32) -> Side {
        let cell_x = x as usize % self.size.cell_width as usize;
        let half_cell_width = (self.size.cell_width as f32 / 2.0) as usize;

        if cell_x > half_cell_width {
            Side::Right
        } else {
            Side::Left
        }
    }

    fn resize(
        &mut self,
        terminal: &mut Term<EventProxy>,
        layout_size: Size,
        font_size: Size,
    ) {
        if layout_size == self.size.layout_size
            && font_size.width as u16 == self.size.cell_width
            && font_size.height as u16 == self.size.cell_height
        {
            return;
        }

        let new_cell_w = font_size.width as u16;
        let new_cell_h = font_size.height as u16;
        let new_lines = (layout_size.height / font_size.height.floor()) as u16;
        let new_cols = (layout_size.width / font_size.width.floor()) as u16;

        if new_lines == 0 || new_cols == 0 {
            return;
        }

        let grid_changed = new_lines != self.size.num_lines || new_cols != self.size.num_cols;

        // Always update pixel dimensions for rendering
        self.size.layout_size = layout_size;
        self.size.cell_width = new_cell_w;
        self.size.cell_height = new_cell_h;

        // Only do expensive grid reflow when character dimensions change
        if grid_changed {
            self.size.num_lines = new_lines;
            self.size.num_cols = new_cols;
            self.dirty.store(true, Ordering::Release);
            self.sink.on_resize(self.size.into());
            terminal.resize(TermSize::new(
                new_cols as usize,
                new_lines as usize,
            ));
        }
    }

    fn write<I: Into<Cow<'static, [u8]>>>(&self, input: I) {
        self.sink.notify(input);
    }

    fn scroll(&mut self, terminal: &mut Term<EventProxy>, delta_value: i32) {
        if delta_value != 0 {
            let scroll = Scroll::Delta(delta_value);
            if terminal
                .mode()
                .contains(TermMode::ALTERNATE_SCROLL | TermMode::ALT_SCREEN)
            {
                let line_cmd = if delta_value > 0 { b'A' } else { b'B' };
                let mut content = vec![];

                for _ in 0..delta_value.abs() {
                    content.push(0x1b);
                    content.push(b'O');
                    content.push(line_cmd);
                }

                self.sink.notify(content);
            } else {
                terminal.grid_mut().scroll_display(scroll);
            }
        }
    }

    /// Based on alacritty/src/display/hint.rs > regex_match_at
    /// Retrieve the match, if the specified point is inside the content matching the regex.
    fn regex_match_at(
        &self,
        terminal: &Term<EventProxy>,
        point: Point,
        regex: &mut RegexSearch,
    ) -> Option<Match> {
        let x = visible_regex_match_iter(terminal, regex)
            .find(|rm| rm.contains(&point));
        x
    }
}

/// Chunked-lock variant of [`search_all_in_term`]: scans the buffer
/// in `chunk_lines` slices, re-acquiring the term lock per slice so
/// a multi-second scan over a huge or very wide buffer doesn't
/// starve the renderer (which needs the same `FairMutex` every
/// frame). `should_abort` is polled between slices — a superseded
/// scan stops burning CPU early. Returns `None` when aborted.
///
/// Slices use a small line overlap and matches are kept by their
/// start line, so a match spanning a boundary is neither lost nor
/// duplicated. A streaming writer may shift the buffer between
/// slices; the embedder's periodic refresh bounds that staleness
/// exactly as it does for the single-lock scan.
pub fn search_all_in_term_chunked(
    term: &FairMutex<Term<EventProxy>>,
    regex: &mut RegexSearch,
    chunk_lines: usize,
    mut should_abort: impl FnMut() -> bool,
) -> Option<Vec<Match>> {
    const OVERLAP: i32 = 4;
    let chunk_lines = chunk_lines.max(64) as i32;
    let (mut current, last) = {
        let t = term.lock();
        let total = t.grid().total_lines();
        let screen = t.grid().screen_lines();
        let history = total.saturating_sub(screen);
        (-(history as i32), t.bottommost_line().0)
    };
    let mut out = Vec::new();
    while current <= last {
        if should_abort() {
            return None;
        }
        let chunk_end = (current + chunk_lines - 1).min(last);
        {
            let t = term.lock();
            let topmost = t.topmost_line().0;
            let bottom = t.bottommost_line().0;
            let lo = current.max(topmost);
            let hi = (chunk_end + OVERLAP).min(bottom);
            if lo <= hi {
                let start =
                    t.line_search_left(Point::new(Line(lo), Column(0)));
                let end =
                    t.line_search_right(Point::new(Line(hi), Column(0)));
                for m in
                    RegexIter::new(start, end, Direction::Right, &t, regex)
                {
                    let sl = m.start().line.0;
                    if sl >= current && sl <= chunk_end {
                        out.push(m);
                    }
                }
            }
        }
        current = chunk_end + 1;
    }
    Some(out)
}

/// Attribute triple `text_ansi_lines` must emit for a cell, or
/// `None` when the cell renders with pure defaults (default
/// foreground, no bold/dim).
fn sgr_for(
    fg: AnsiColor,
    bold: bool,
    dim: bool,
) -> Option<(AnsiColor, bool, bool)> {
    let default_fg = matches!(fg, AnsiColor::Named(NamedColor::Foreground));
    if default_fg && !bold && !dim {
        None
    } else {
        Some((fg, bold, dim))
    }
}

/// Minimal SGR escape applying the given attributes (assumes a
/// preceding reset).
fn sgr_codes(fg: AnsiColor, bold: bool, dim: bool) -> String {
    let mut params: Vec<String> = Vec::new();
    if bold {
        params.push("1".into());
    }
    if dim {
        params.push("2".into());
    }
    match fg {
        AnsiColor::Named(n) => {
            if let Some(code) = named_fg_code(n) {
                params.push(code.to_string());
            }
        }
        AnsiColor::Spec(rgb) => {
            params.push(format!("38;2;{};{};{}", rgb.r, rgb.g, rgb.b))
        }
        AnsiColor::Indexed(i) => params.push(format!("38;5;{}", i)),
    }
    if params.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", params.join(";"))
    }
}

fn named_fg_code(n: NamedColor) -> Option<u8> {
    use NamedColor::*;
    Some(match n {
        Black | DimBlack => 30,
        Red | DimRed => 31,
        Green | DimGreen => 32,
        Yellow | DimYellow => 33,
        Blue | DimBlue => 34,
        Magenta | DimMagenta => 35,
        Cyan | DimCyan => 36,
        White | DimWhite => 37,
        BrightBlack => 90,
        BrightRed => 91,
        BrightGreen => 92,
        BrightYellow => 93,
        BrightBlue => 94,
        BrightMagenta => 95,
        BrightCyan => 96,
        BrightWhite => 97,
        _ => return None,
    })
}

/// Copied from alacritty/src/display/hint.rs:
/// Iterate over all visible regex matches.
/// Run a full-buffer search against an already-locked `Term`.
/// Lets external callers (e.g. Kubezilla's async search task) run
/// the same `RegexIter::collect()` that `TerminalBackend::search_all`
/// runs internally, but with their own lock acquisition timing —
/// useful for `tokio::task::spawn_blocking` so the UI thread isn't
/// stalled by a slow regex scan over a wide-grid streaming buffer.
pub fn search_all_in_term(
    term: &Term<EventProxy>,
    regex: &mut RegexSearch,
) -> Vec<Match> {
    let total_lines = term.grid().total_lines();
    let screen_lines = term.grid().screen_lines();
    let history = total_lines.saturating_sub(screen_lines);
    let start_line = Line(-(history as i32));
    let end_line = term.bottommost_line();
    let start = term.line_search_left(Point::new(start_line, Column(0)));
    let end = term.line_search_right(Point::new(end_line, Column(0)));
    RegexIter::new(start, end, Direction::Right, term, regex).collect()
}

/// Column-bounded version of `visible_regex_match_iter`. Only
/// scans grid cells in `[min_col, max_col]` on each line, so the
/// cost scales with *visible* width rather than full grid_cols.
/// On a wrap-off log window with grid_cols=2000 and visible_cols
/// ≈100, this cuts per-frame regex cost roughly 20×.
///
/// Returns matches that may extend past `max_col` (the regex
/// engine still sees the full line content from `min_col` onward
/// once it starts a match), but starts looking only at columns in
/// the visible band — matches whose start is outside the band are
/// never produced.
pub fn visible_regex_match_iter_in_cols(
    term: &Term<EventProxy>,
    regex: &mut RegexSearch,
    min_col: usize,
    max_col: usize,
    line_padding: i32,
) -> Vec<Match> {
    let viewport_start = Line(-(term.grid().display_offset() as i32));
    let viewport_end = viewport_start + term.bottommost_line();
    let topmost = term.topmost_line();
    let bottommost = term.bottommost_line();
    let line_lo = (viewport_start - line_padding).max(topmost);
    let line_hi = (viewport_end + line_padding).min(bottommost);
    let last_col = term.grid().columns().saturating_sub(1);
    let mut matches = Vec::new();
    let mut line = line_lo;
    while line <= line_hi {
        let row_start = Point::new(line, Column(min_col));
        let row_end = Point::new(line, Column(max_col.min(last_col)));
        let mut current = row_start;
        loop {
            match term.regex_search_right(regex, current, row_end) {
                Some(m) => {
                    let next_col = m.end().column.0.saturating_add(1);
                    matches.push(m);
                    if next_col > max_col || next_col > last_col {
                        break;
                    }
                    current = Point::new(line, Column(next_col));
                }
                None => break,
            }
        }
        line += alacritty_terminal::index::Line(1);
    }
    matches
}

/// Run the column-bounded per-line regex scan over an explicit set
/// of grid lines instead of a contiguous viewport range. Used by
/// grep/filter mode to highlight matches on exactly the rows being
/// displayed after the line remap.
///
/// Lines outside the grid's current `[topmost, bottommost]` range
/// are skipped — the caller's line list may be stale relative to a
/// streaming buffer whose scrollback has rotated, and indexing the
/// grid out of range panics.
pub fn regex_matches_on_lines(
    term: &Term<EventProxy>,
    regex: &mut RegexSearch,
    lines: &[Line],
    min_col: usize,
    max_col: usize,
) -> Vec<Match> {
    let topmost = term.topmost_line();
    let bottommost = term.bottommost_line();
    let last_col = term.grid().columns().saturating_sub(1);
    let mut matches = Vec::new();
    for &line in lines {
        if line < topmost || line > bottommost {
            continue;
        }
        let row_start = Point::new(line, Column(min_col));
        let row_end = Point::new(line, Column(max_col.min(last_col)));
        let mut current = row_start;
        loop {
            match term.regex_search_right(regex, current, row_end) {
                Some(m) => {
                    let next_col = m.end().column.0.saturating_add(1);
                    matches.push(m);
                    if next_col > max_col || next_col > last_col {
                        break;
                    }
                    current = Point::new(line, Column(next_col));
                }
                None => break,
            }
        }
    }
    matches
}

/// Grid lines within the last `band` rows of the buffer that contain
/// at least one regex match, sorted ascending. Full-width scan —
/// grep membership must not depend on the horizontal pan window.
/// Early-outs per line on the first match, so cost is bounded by
/// `band` regex probes on non-matching lines.
///
/// Used by grep/filter mode to keep the tail of a streaming buffer
/// fresh between the embedder's full-scrollback rescans.
pub fn tail_matching_lines(
    term: &Term<EventProxy>,
    regex: &mut RegexSearch,
    band: usize,
) -> Vec<i32> {
    let topmost = term.topmost_line();
    let bottommost = term.bottommost_line();
    let band_start = (bottommost - (band.saturating_sub(1) as i32)).max(topmost);
    let last_col = term.grid().columns().saturating_sub(1);
    let mut lines = Vec::new();
    let mut line = band_start;
    while line <= bottommost {
        let row_start = Point::new(line, Column(0));
        let row_end = Point::new(line, Column(last_col));
        if term.regex_search_right(regex, row_start, row_end).is_some() {
            lines.push(line.0);
        }
        line += Line(1);
    }
    lines
}

pub fn visible_regex_match_iter<'a>(
    term: &'a Term<EventProxy>,
    regex: &'a mut RegexSearch,
) -> impl Iterator<Item = Match> + 'a {
    // Padding around the visible viewport so matches that span the
    // boundary are still found. Was 100 lines historically; that's
    // dominant per-frame cost when the grid is wide (long-line /
    // wrap-off log windows). 5 lines covers any realistic search
    // pattern length and shrinks the regex engine's input by an
    // order of magnitude on wide grids — the search-typing path
    // becomes O(viewport) instead of O(viewport × 200).
    const SCAN_PADDING: i32 = 5;
    let viewport_start = Line(-(term.grid().display_offset() as i32));
    let viewport_end = viewport_start + term.bottommost_line();
    let mut start =
        term.line_search_left(Point::new(viewport_start, Column(0)));
    let mut end = term.line_search_right(Point::new(viewport_end, Column(0)));
    start.line = start.line.max(viewport_start - SCAN_PADDING);
    end.line = end.line.min(viewport_end + SCAN_PADDING);

    RegexIter::new(start, end, Direction::Right, term, regex)
        .skip_while(move |rm| rm.end().line < viewport_start)
        .take_while(move |rm| rm.start().line <= viewport_end)
}

/// Public snapshot of grid geometry for navigation helpers.
#[derive(Clone, Copy, Debug)]
pub struct GridBounds {
    pub display_offset: i32,
    pub screen_lines: i32,
    pub columns: usize,
    pub total_lines: usize,
    pub bottommost_line: i32,
}

pub struct RenderableContent {
    pub display_offset: usize,
    pub cursor_point: Point,
    pub hovered_hyperlink: Option<RangeInclusive<Point>>,
    pub selectable_range: Option<SelectionRange>,
    pub cursor: Cell,
    pub terminal_mode: TermMode,
    pub terminal_size: TerminalSize,
}

impl Default for RenderableContent {
    fn default() -> Self {
        Self {
            display_offset: 0,
            cursor_point: Point::default(),
            hovered_hyperlink: None,
            selectable_range: None,
            cursor: Cell::default(),
            terminal_mode: TermMode::empty(),
            terminal_size: TerminalSize::default(),
        }
    }
}

/// Handle returned from `TerminalBackend::new_streaming()`.
///
/// The caller uses `stream` to pipe data to/from the terminal, and
/// `resize_rx` to receive resize events forwarded by alacritty.
pub struct StreamHandle {
    pub stream: std::net::TcpStream,
    pub resize_rx: std::sync::mpsc::Receiver<alacritty_terminal::event::WindowSize>,
}

/// A writer that feeds bytes directly into the terminal grid,
/// bypassing the EventLoop and socket proxy.
pub struct DirectWriter {
    inner: Arc<DirectWriterInner>,
}

struct DirectWriterInner {
    term: Arc<FairMutex<Term<EventProxy>>>,
    processor: Mutex<Processor>,
    app_context: egui::Context,
    dirty: Arc<AtomicBool>,
    /// Mirror of TerminalBackend.content_revision so the writer can
    /// bump it on every byte fed in.
    content_revision: Arc<AtomicU64>,
    /// When true, keep view position stable as new content arrives (log mode).
    sticky_scroll: Arc<AtomicBool>,
}

impl DirectWriter {
    /// Feed raw bytes into the terminal emulator.
    pub fn write(&self, data: &[u8]) {
        let mut term = self.inner.term.lock();
        let old_offset = term.grid().display_offset();
        let old_history = term.grid().total_lines().saturating_sub(term.grid().screen_lines());
        let mut processor = self.inner.processor.lock().unwrap();
        processor.advance(&mut *term, data);
        drop(processor);

        // In sticky_scroll mode: if the user was scrolled up, adjust
        // display_offset to keep the same content visible.
        if self.inner.sticky_scroll.load(Ordering::Relaxed) && old_offset > 0 {
            let new_history = term.grid().total_lines().saturating_sub(term.grid().screen_lines());
            let added = new_history.saturating_sub(old_history);
            if added > 0 {
                let new_offset = (old_offset + added).min(new_history);
                term.scroll_display(Scroll::Bottom);
                if new_offset > 0 {
                    term.scroll_display(Scroll::Delta(new_offset as i32));
                }
            }
        }

        self.inner.dirty.store(true, Ordering::Release);
        self.inner.content_revision.fetch_add(1, Ordering::Relaxed);
        drop(term);
        throttled_repaint(&self.inner.app_context);
    }
}

impl Clone for DirectWriter {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Handle returned from `TerminalBackend::new_direct()`.
///
/// The caller uses `writer` to feed data directly into the terminal grid,
/// `input_rx` to receive user input, and `resize_rx` to receive resize events.
pub struct DirectHandle {
    pub writer: DirectWriter,
    pub input_rx: mpsc::Receiver<Vec<u8>>,
    pub resize_rx: mpsc::Receiver<WindowSize>,
}

impl Drop for TerminalBackend {
    fn drop(&mut self) {
        self.sink.shutdown();
    }
}

#[derive(Clone)]
pub struct EventProxy {
    sender: mpsc::Sender<Event>,
    dirty: Arc<AtomicBool>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        self.dirty.store(true, Ordering::Release);
        let _ = self.sender.send(event.clone());
    }
}

#[cfg(test)]
mod grep_text_tests {
    use super::*;

    /// Direct backend (80×50 default grid) fed synthetic log lines;
    /// the 200-char ERROR line wraps across three visual rows.
    fn backend_with_log() -> TerminalBackend {
        let (tx, _rx) = std::sync::mpsc::channel();
        let ctx = egui::Context::default();
        let (backend, handle) =
            TerminalBackend::new_direct(7, ctx, tx).unwrap();
        let long = format!("ERROR alpha {} OMEGA", "x".repeat(180));
        let data = format!(
            "info one\r\n{}\r\ninfo two\r\nERROR beta short\r\n",
            long
        );
        handle.writer.write(data.as_bytes());
        backend
    }

    fn error_lines(backend: &TerminalBackend) -> Vec<i32> {
        let mut regex = RegexSearch::new("ERROR").unwrap();
        let term = backend.term_arc();
        let guard = term.lock();
        let mut lines: Vec<i32> = search_all_in_term(&guard, &mut regex)
            .iter()
            .flat_map(|m| m.start().line.0..=m.end().line.0)
            .collect();
        lines.sort_unstable();
        lines.dedup();
        lines
    }

    #[test]
    fn lines_text_joins_wrapped_logical_lines() {
        let backend = backend_with_log();
        let lines = error_lines(&backend);
        assert!(!lines.is_empty());
        let text = backend.lines_text(&lines);
        let rows: Vec<&str> = text.lines().collect();
        // Exactly the two ERROR lines, each complete and unbroken.
        assert_eq!(rows.len(), 2, "got: {rows:?}");
        assert!(rows[0].starts_with("ERROR alpha"));
        assert!(
            rows[0].ends_with("OMEGA"),
            "wrapped line not joined to its end: {:?}",
            rows[0]
        );
        assert!(rows[0].len() > 190, "wrapped tail missing");
        assert_eq!(rows[1], "ERROR beta short");
        assert!(!text.contains("info"));
    }

    #[test]
    fn lines_text_emits_wrapped_line_once_for_multiple_rows() {
        let backend = backend_with_log();
        let lines = error_lines(&backend);
        // Add every continuation row of the wrapped line explicitly —
        // the logical line must still be emitted only once.
        let mut padded = lines.clone();
        for l in &lines {
            padded.push(l + 1);
        }
        padded.sort_unstable();
        padded.dedup();
        let text = backend.lines_text(&padded);
        assert_eq!(
            text.matches("ERROR alpha").count(),
            1,
            "wrapped line duplicated: {text}"
        );
    }

    #[test]
    fn lines_text_skips_out_of_range_lines() {
        let backend = backend_with_log();
        assert_eq!(backend.lines_text(&[-9999, 9999]), "");
    }
}

#[cfg(test)]
mod flip_tests {
    use super::*;

    fn direct_backend() -> (TerminalBackend, DirectHandle) {
        let (tx, _rx) = std::sync::mpsc::channel();
        let ctx = egui::Context::default();
        TerminalBackend::new_direct(9, ctx, tx).unwrap()
    }

    #[test]
    fn text_ansi_lines_preserves_colors_and_order() {
        let (backend, handle) = direct_backend();
        handle.writer.write(
            b"plain one\r\n\x1b[31mRED line\x1b[0m tail\r\n\x1b[1;32mbold green\x1b[0m\r\n",
        );
        let lines = backend.text_ansi_lines();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0], "plain one");
        assert!(lines[1].contains("\x1b[31m"), "{:?}", lines[1]);
        assert!(lines[1].contains("RED line"));
        assert!(lines[1].ends_with("\x1b[0m") || lines[1].ends_with("tail"));
        assert!(
            lines[2].contains("\x1b[1;32m") || lines[2].contains("\x1b[32;1m"),
            "{:?}",
            lines[2]
        );
    }

    #[test]
    fn text_ansi_lines_joins_wrapped_rows() {
        let (backend, handle) = direct_backend();
        let long = format!("HEAD {} TAIL", "y".repeat(150)); // wraps at 80 cols
        handle.writer.write(format!("{}\r\nnext\r\n", long).as_bytes());
        let lines = backend.text_ansi_lines();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("HEAD"));
        assert!(lines[0].ends_with("TAIL"), "wrap not joined: {:?}", lines[0]);
        assert_eq!(lines[1], "next");
    }

    #[test]
    fn feed_display_clear_and_reverse_round_trip() {
        let (mut backend, handle) = direct_backend();
        handle
            .writer
            .write(b"first\r\n\x1b[31msecond red\x1b[0m\r\nthird\r\n");
        let mut lines = backend.text_ansi_lines();
        lines.reverse();
        let data = format!("\x1b[2J\x1b[3J\x1b[H{}", lines.join("\r\n"));
        backend.feed_display(data.as_bytes());

        let text = backend.full_text();
        let rows: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(rows, vec!["third", "second red", "first"], "{text:?}");
        // Colors survived the round trip: re-extract and find red SGR
        // on the middle (formerly second) line.
        let again = backend.text_ansi_lines();
        assert!(
            again.iter().any(|l| l.contains("\x1b[31m")),
            "{again:?}"
        );
    }
}

#[cfg(test)]
mod chunked_scan_tests {
    use super::*;

    fn backend_with_lines(n: usize) -> TerminalBackend {
        let (tx, _rx) = std::sync::mpsc::channel();
        let ctx = egui::Context::default();
        let (backend, handle) =
            TerminalBackend::new_direct(11, ctx, tx).unwrap();
        let mut data = String::new();
        for i in 0..n {
            if i % 7 == 0 {
                data.push_str(&format!("ERROR num {:04}\r\n", i));
            } else {
                data.push_str(&format!("fine  num {:04}\r\n", i));
            }
        }
        handle.writer.write(data.as_bytes());
        backend
    }

    #[test]
    fn chunked_matches_equal_single_lock_scan() {
        let backend = backend_with_lines(500);
        let term = backend.term_arc();

        let mut r1 = RegexSearch::new("ERROR").unwrap();
        let full = {
            let guard = term.lock();
            search_all_in_term(&guard, &mut r1)
        };

        let mut r2 = RegexSearch::new("ERROR").unwrap();
        let chunked =
            search_all_in_term_chunked(&term, &mut r2, 64, || false)
                .expect("not aborted");

        let key = |m: &Match| (m.start().line.0, m.start().column.0);
        let a: Vec<_> = full.iter().map(key).collect();
        let b: Vec<_> = chunked.iter().map(key).collect();
        assert!(!a.is_empty());
        assert_eq!(a, b);
    }

    #[test]
    fn chunked_scan_aborts() {
        let backend = backend_with_lines(500);
        let term = backend.term_arc();
        let mut regex = RegexSearch::new("ERROR").unwrap();
        let mut polls = 0;
        let result =
            search_all_in_term_chunked(&term, &mut regex, 64, || {
                polls += 1;
                polls > 2
            });
        assert!(result.is_none());
    }
}
