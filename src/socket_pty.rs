use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{ChildEvent, EventedPty, EventedReadWrite};
use polling::{Event, PollMode, Poller};
use std::io;
use std::net::TcpStream;
use std::sync::Arc;

/// Token value matching alacritty_terminal's internal PTY_READ_WRITE_TOKEN on unix.
const PTY_READ_WRITE_TOKEN: usize = 0;

/// A fake PTY backed by a TCP socket pair over localhost.
///
/// One end is given to alacritty's `EventLoop` (this struct),
/// the other end is bridged to kube-rs async streams via tokio tasks.
pub struct SocketPty {
    stream: TcpStream,
    resize_tx: std::sync::mpsc::Sender<WindowSize>,
}

impl SocketPty {
    pub fn new(stream: TcpStream, resize_tx: std::sync::mpsc::Sender<WindowSize>) -> Self {
        stream.set_nonblocking(true).expect("set nonblocking");
        Self { stream, resize_tx }
    }
}

/// Create a connected pair of TCP streams over localhost.
pub fn tcp_stream_pair() -> io::Result<(TcpStream, TcpStream)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let stream_a = TcpStream::connect(addr)?;
    let (stream_b, _) = listener.accept()?;
    stream_a.set_nodelay(true)?;
    stream_b.set_nodelay(true)?;
    Ok((stream_a, stream_b))
}

impl EventedReadWrite for SocketPty {
    type Reader = TcpStream;
    type Writer = TcpStream;

    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        mut interest: Event,
        poll_opts: PollMode,
    ) -> io::Result<()> {
        interest.key = PTY_READ_WRITE_TOKEN;
        unsafe { poll.add_with_mode(&self.stream, interest, poll_opts) }
    }

    fn reregister(
        &mut self,
        poll: &Arc<Poller>,
        mut interest: Event,
        poll_opts: PollMode,
    ) -> io::Result<()> {
        interest.key = PTY_READ_WRITE_TOKEN;
        poll.modify_with_mode(&self.stream, interest, poll_opts)
    }

    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        poll.delete(&self.stream)
    }

    fn reader(&mut self) -> &mut Self::Reader {
        &mut self.stream
    }

    fn writer(&mut self) -> &mut Self::Writer {
        &mut self.stream
    }
}

impl EventedPty for SocketPty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        None
    }
}

impl OnResize for SocketPty {
    fn on_resize(&mut self, size: WindowSize) {
        let _ = self.resize_tx.send(size);
    }
}
