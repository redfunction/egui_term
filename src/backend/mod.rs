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
use alacritty_terminal::vte::ansi::Processor;
use egui::Modifiers;
use settings::BackendSettings;
use std::borrow::Cow;
use std::cmp::min;
use std::io::Result;
use std::ops::{Index, RangeInclusive};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{mpsc, Arc, Mutex};

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
                if let Ok(event) = event_receiver.recv() {
                    pty_event_proxy_sender
                        .send((id, event.clone()))
                        .unwrap_or_else(|_| {
                            panic!("pty_event_subscription_{}: sending PtyEvent is failed", id)
                        });
                    app_context.clone().request_repaint();
                    match event {
                        Event::Exit => break,
                        Event::PtyWrite(pty) => pty_notifier.notify(pty.into_bytes()),
                        _ => {}
                    }
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
                if let Ok(event) = event_receiver.recv() {
                    pty_event_proxy_sender
                        .send((id, event.clone()))
                        .unwrap_or_else(|_| {
                            panic!("pty_event_subscription_{}: sending PtyEvent is failed", id)
                        });
                    app_context.clone().request_repaint();
                    match event {
                        Event::Exit => break,
                        Event::PtyWrite(pty) => pty_notifier.notify(pty.into_bytes()),
                        _ => {}
                    }
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

        let writer = DirectWriter {
            inner: Arc::new(DirectWriterInner {
                term: term.clone(),
                processor: Mutex::new(processor),
                app_context: app_context.clone(),
                dirty: dirty.clone(),
            }),
        };

        // Route PtyWrite events to input_tx
        let input_tx_for_events = input_tx.clone();
        let _pty_event_subscription = std::thread::Builder::new()
            .name(format!("pty_event_subscription_{}", id))
            .spawn(move || loop {
                if let Ok(event) = event_receiver.recv() {
                    pty_event_proxy_sender
                        .send((id, event.clone()))
                        .unwrap_or_else(|_| {
                            panic!("pty_event_subscription_{}: sending PtyEvent is failed", id)
                        });
                    app_context.clone().request_repaint();
                    match event {
                        Event::Exit => break,
                        Event::PtyWrite(data) => {
                            let _ = input_tx_for_events.send(data.into_bytes());
                        }
                        _ => {}
                    }
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
                self.write(input);
                term.scroll_display(Scroll::Bottom);
            },
            BackendCommand::Scroll(delta) => {
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

    pub fn selectable_content(&self) -> String {
        let content = self.last_content();
        let mut result = String::new();
        if let Some(range) = content.selectable_range {
            let terminal = self.term.lock();
            for indexed in terminal.grid().display_iter() {
                if range.contains(indexed.point) {
                    result.push(indexed.c);
                }
            }
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

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
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

/// Copied from alacritty/src/display/hint.rs:
/// Iterate over all visible regex matches.
fn visible_regex_match_iter<'a>(
    term: &'a Term<EventProxy>,
    regex: &'a mut RegexSearch,
) -> impl Iterator<Item = Match> + 'a {
    let viewport_start = Line(-(term.grid().display_offset() as i32));
    let viewport_end = viewport_start + term.bottommost_line();
    let mut start =
        term.line_search_left(Point::new(viewport_start, Column(0)));
    let mut end = term.line_search_right(Point::new(viewport_end, Column(0)));
    start.line = start.line.max(viewport_start - 100);
    end.line = end.line.min(viewport_end + 100);

    RegexIter::new(start, end, Direction::Right, term, regex)
        .skip_while(move |rm| rm.end().line < viewport_start)
        .take_while(move |rm| rm.start().line <= viewport_end)
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
}

impl DirectWriter {
    /// Feed raw bytes into the terminal emulator.
    pub fn write(&self, data: &[u8]) {
        let mut term = self.inner.term.lock();
        let mut processor = self.inner.processor.lock().unwrap();
        processor.advance(&mut *term, data);
        self.inner.dirty.store(true, Ordering::Release);
        drop(processor);
        drop(term);
        self.inner.app_context.request_repaint();
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
