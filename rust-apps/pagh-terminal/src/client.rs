//! PDP/1 client: connect to `pagh-desktop`, perform HELLO/WELCOME, create a
//! window, upload RGBA frames and decode server input events.
//!
//! The compositor owns decorations, focus, placement and resize.  This client
//! reacts to `configure`/`focus`/`close`.  When the server advertises
//! `welcome.flags` bit 2, frames go through a `ShmSurface` (memfd mapped
//! directly by the renderer, only damage metadata on the socket); otherwise the
//! v1 full-frame upload path is used.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use pagh_ui::damage::Damage;
use pagh_ui::protocol::FrameFormat as UiFrameFormat;
use pagh_ui::shm as ui_shm;

use crate::protocol::{
    self, buttons, close_reason, header_flags, mods, pack_rgb565, pack_xrgb8888, welcome_flags,
    Configure, FrameFormat, InputEvent, Message, MessageReader, ProtocolError, HEADER_LEN, MAGIC,
    MAX_PAYLOAD, MAX_SURFACE_DIM, VERSION,
};
use crate::render::Canvas;

/// Default control socket used by the sibling `pagh-desktop` server.
pub const DEFAULT_SOCKET: &str = "/tmp/pagh-desktop.sock";
/// Framing + commit prefix overhead (diagnostics).
pub const COMMIT_OVERHEAD: usize = HEADER_LEN + 24;
/// Optional PDP/1 trace file (one line per message, no pixel bytes).
pub const TRACE_ENV: &str = "PAGH_TERMINAL_PDP_TRACE";

/// Welcome information returned by the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub version: u32,
    pub client_id: u64,
    pub screen_width: u32,
    pub screen_height: u32,
    pub flags: u32,
}

/// Server-to-client events relevant to a terminal surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Configure(Configure),
    Input(InputEvent),
    Focus {
        window_id: u32,
        focused: bool,
    },
    Close {
        window_id: u32,
        reason: u32,
    },
    Error {
        code: u32,
        message: String,
    },
    Ping(u32),
    Pong(u32),
    /// Server accepted the shared surface and assigned a generation.
    ShmReady {
        window_id: u32,
        generation: u32,
    },
    /// Server released a generation; fall back to full-frame upload.
    ShmRelease {
        window_id: u32,
        generation: u32,
    },
}

/// Client shared surface: mmap canvas + generation for shm commits.
pub struct ShmFrame {
    pub canvas: Canvas,
    pub window_id: u32,
    pub generation: u32,
}

impl ShmFrame {
    pub fn size(&self) -> (u32, u32) {
        (self.canvas.width as u32, self.canvas.height as u32)
    }
}

/// Connected PDP/1 client.
#[derive(Debug)]
pub struct SurfaceClient {
    stream: UnixStream,
    reader: MessageReader,
    client_id: u64,
    window_id: u32,
    screen_width: u32,
    screen_height: u32,
    width: u32,
    height: u32,
    flags: u32,
    format: FrameFormat,
    peer_closed: bool,
    /// Welcome advertises ShmSurface and the environment supports it.
    shm_capable: bool,
    /// Current accepted generation (0 = none).
    generation: u32,
    /// Events received while synchronously waiting for SHM_READY.
    pending: VecDeque<Event>,
    /// Reused shm-commit buffer (no allocation in the frame loop).
    tx: Vec<u8>,
    /// Optional compact PDP/1 trace (e2e/debug).
    trace: Option<BufWriter<File>>,
}

impl SurfaceClient {
    pub fn connect(path: impl AsRef<Path>) -> io::Result<SurfaceClient> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_millis(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(SurfaceClient {
            stream,
            reader: MessageReader::new(),
            client_id: 0,
            window_id: 0,
            screen_width: 0,
            screen_height: 0,
            width: 0,
            height: 0,
            flags: 0,
            format: FrameFormat::Xrgb8888,
            peer_closed: false,
            shm_capable: false,
            generation: 0,
            pending: VecDeque::new(),
            tx: Vec::with_capacity(64 * 1024),
            trace: std::env::var_os(TRACE_ENV)
                .filter(|value| !value.is_empty())
                .and_then(|path| File::create(std::path::Path::new(&path)).ok())
                .map(BufWriter::new),
        })
    }

    pub fn client_id(&self) -> u64 {
        self.client_id
    }

    pub fn window_id(&self) -> u32 {
        self.window_id
    }

    pub fn screen_size(&self) -> (u32, u32) {
        (self.screen_width, self.screen_height)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn format(&self) -> FrameFormat {
        self.format
    }

    pub fn flags(&self) -> u32 {
        self.flags
    }

    pub fn peer_closed(&self) -> bool {
        self.peer_closed
    }

    /// Send `hello` and wait for `welcome`.
    pub fn handshake(&mut self, app: &str, timeout: Duration) -> io::Result<Welcome> {
        self.send(&Message::Hello {
            version: VERSION,
            pid: std::process::id(),
            app: app.to_string(),
        })?;
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for welcome",
                ));
            }
            let Some(message) = self.next_message(remaining)? else {
                continue;
            };
            match message {
                Message::Welcome {
                    version,
                    client_id,
                    width,
                    height,
                    flags,
                } => {
                    if version != VERSION {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("unsupported PDP/1 version {version}"),
                        ));
                    }
                    self.client_id = client_id;
                    self.screen_width = width;
                    self.screen_height = height;
                    self.flags = flags;
                    self.format = FrameFormat::from_welcome_flags(flags);
                    self.shm_capable = flags & welcome_flags::SHM_SURFACE != 0
                        && self.format == FrameFormat::Xrgb8888
                        && ui_shm::shm_supported();
                    return Ok(Welcome {
                        version,
                        client_id,
                        screen_width: width,
                        screen_height: height,
                        flags,
                    });
                }
                Message::Error { code, message } => {
                    return Err(io::Error::other(format!("server error {code}: {message}")));
                }
                _ => {} // tolerate unrelated messages until welcome
            }
        }
    }

    /// Ask the compositor to create a window.  `window_id` is client-chosen,
    /// nonzero and unique among the client's windows.  The server answers with
    /// `focus` and `configure` events.
    pub fn create_window(
        &mut self,
        window_id: u32,
        width: u32,
        height: u32,
        title: &str,
    ) -> io::Result<()> {
        if window_id == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "window_id must be nonzero",
            ));
        }
        if width == 0 || height == 0 || width > MAX_SURFACE_DIM || height > MAX_SURFACE_DIM {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid window size",
            ));
        }
        self.window_id = window_id;
        self.width = width;
        self.height = height;
        self.send(&Message::Create {
            window_id,
            width,
            height,
            format: self.format.to_wire(),
            title: title.to_string(),
        })
    }

    /// Upload a full canvas using the negotiated frame format.
    pub fn commit(&mut self, canvas: &Canvas) -> io::Result<()> {
        self.commit_rgba(canvas.width as u32, canvas.height as u32, &canvas.pixels)
    }

    /// Upload straight RGBA pixels.
    pub fn commit_rgba(&mut self, width: u32, height: u32, rgba: &[u8]) -> io::Result<()> {
        if width == 0 || height == 0 || width > MAX_SURFACE_DIM || height > MAX_SURFACE_DIM {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid frame size",
            ));
        }
        let (pixels, stride) = match self.format {
            FrameFormat::Xrgb8888 => (
                pack_xrgb8888(rgba, width as usize, height as usize).map_err(io::Error::from)?,
                width * 4,
            ),
            FrameFormat::Rgb565 => (
                pack_rgb565(rgba, width as usize, height as usize).map_err(io::Error::from)?,
                width * 2,
            ),
        };
        self.width = width;
        self.height = height;
        self.send(&Message::Commit {
            window_id: self.window_id,
            width,
            height,
            format: self.format.to_wire(),
            stride,
            pixels,
        })
    }

    /// Optional damage hint (v1 still sends full frames).
    pub fn damage_hint(&mut self, rects: &[(i32, i32, u16, u16)]) -> io::Result<()> {
        self.send(&Message::Damage {
            window_id: self.window_id,
            rects: rects.to_vec(),
        })
    }

    /// Ask the server to close the window.
    pub fn close(&mut self) -> io::Result<()> {
        self.send(&Message::Close {
            window_id: self.window_id,
            reason: close_reason::CLIENT,
        })
    }

    pub fn ping(&mut self, nonce: u32) -> io::Result<()> {
        self.send(&Message::Ping { nonce })
    }

    // ───────────────────────── ShmSurface (PDP/1 B5) ────────────────────────

    /// Server + environment support shared-memory frames.
    pub fn shm_supported(&self) -> bool {
        self.shm_capable
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Create a memfd surface, send `SHM_OFFER` with its fd and wait for
    /// `SHM_READY`.  `Ok(None)` means "use the full-frame path".
    pub fn enable_shm(&mut self, width: u32, height: u32) -> io::Result<Option<ShmFrame>> {
        if !self.shm_capable || width == 0 || height == 0 {
            return Ok(None);
        }
        let Some((mut frame, generation)) = self.offer_shm(width, height)? else {
            return Ok(None);
        };
        frame.generation = generation;
        self.generation = generation;
        Ok(Some(frame))
    }

    /// Recreate memfd/mapping for a new size and release the old generation.
    /// `Ok(false)` means the caller must fall back to full-frame upload.
    pub fn resize_shm(
        &mut self,
        frame: &mut ShmFrame,
        width: u32,
        height: u32,
    ) -> io::Result<bool> {
        if !self.shm_capable || width == 0 || height == 0 {
            return Ok(false);
        }
        let Some((mut new_frame, generation)) = self.offer_shm(width, height)? else {
            return Ok(false);
        };
        new_frame.generation = generation;
        let old = frame.generation;
        let _ = self.send(&Message::ShmRelease {
            window_id: frame.window_id,
            generation: old,
        });
        self.generation = generation;
        *frame = new_frame;
        Ok(true)
    }

    /// Send only damage metadata for the shared surface (no pixel payload).
    pub fn commit_shm(&mut self, frame: &ShmFrame, damage: &Damage) -> io::Result<()> {
        if !self.shm_capable {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "ShmSurface is not enabled",
            ));
        }
        if damage.is_empty() {
            return Ok(());
        }
        let rects = damage.rects();
        // PDP/1 B5: rect = i32 x, i32 y, u32 w, u32 h (16 байт).
        let payload = 12 + rects.len() * 16;
        self.tx.clear();
        self.tx.reserve(HEADER_LEN + payload);
        self.tx.extend_from_slice(&MAGIC);
        self.tx
            .extend_from_slice(&protocol::types::COMMIT.to_le_bytes());
        self.tx.extend_from_slice(&header_flags::SHM.to_le_bytes());
        self.tx.extend_from_slice(&(payload as u32).to_le_bytes());
        self.tx.extend_from_slice(&frame.window_id.to_le_bytes());
        self.tx.extend_from_slice(&frame.generation.to_le_bytes());
        self.tx
            .extend_from_slice(&(rects.len() as u32).to_le_bytes());
        for rect in rects {
            self.tx.extend_from_slice(&rect.x.to_le_bytes());
            self.tx.extend_from_slice(&rect.y.to_le_bytes());
            self.tx.extend_from_slice(&rect.w.max(0).to_le_bytes());
            self.tx.extend_from_slice(&rect.h.max(0).to_le_bytes());
        }
        self.stream.write_all(&self.tx)?;
        self.stream.flush()?;
        if self.trace.is_some() {
            let detail = format!(
                "window={} gen={} damage={}",
                frame.window_id,
                frame.generation,
                rects.len()
            );
            self.trace('>', "commit_shm", header_flags::SHM, payload, &detail);
        }
        Ok(())
    }

    /// Release the current generation and stop using shared memory.
    pub fn release_shm(&mut self, frame: &ShmFrame) -> io::Result<()> {
        self.shm_capable = false;
        self.generation = 0;
        self.send(&Message::ShmRelease {
            window_id: frame.window_id,
            generation: frame.generation,
        })
    }

    fn offer_shm(&mut self, width: u32, height: u32) -> io::Result<Option<(ShmFrame, u32)>> {
        let name = format!("pagh-terminal-{}-{}", std::process::id(), self.window_id);
        let surface =
            match ui_shm::ShmSurface::create(&name, width, height, UiFrameFormat::Xrgb8888) {
                Ok(surface) => surface,
                Err(_) => {
                    self.shm_capable = false;
                    return Ok(None);
                }
            };
        let mapping = match surface.map() {
            Ok(mapping) => mapping,
            Err(_) => {
                self.shm_capable = false;
                return Ok(None);
            }
        };
        let Some(canvas) = Canvas::from_mapped(
            mapping,
            width as usize,
            height as usize,
            surface.stride() as usize,
        ) else {
            self.shm_capable = false;
            return Ok(None);
        };
        let offer = Message::ShmOffer {
            window_id: self.window_id,
            width,
            height,
            format: self.format.to_wire(),
            stride: surface.stride(),
            flags: 0,
        };
        let encoded = offer.encode();
        if ui_shm::send_fd_payload(&self.stream, &encoded, surface.fd()).is_err() {
            self.shm_capable = false;
            return Ok(None);
        }
        if self.trace.is_some() {
            let detail = format!(
                "window={} {}x{} stride={} fd",
                self.window_id,
                width,
                height,
                surface.stride()
            );
            self.trace('>', "shm_offer", 0, encoded.len() - HEADER_LEN, &detail);
        }
        let Some(generation) = self.wait_for_shm_ready()? else {
            return Ok(None);
        };
        Ok(Some((
            ShmFrame {
                canvas,
                window_id: self.window_id,
                generation,
            },
            generation,
        )))
    }

    fn wait_for_shm_ready(&mut self) -> io::Result<Option<u32>> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let Some(message) = self.next_message(remaining)? else {
                return Ok(None);
            };
            match message {
                Message::ShmReady {
                    window_id,
                    generation,
                } if window_id == self.window_id => {
                    self.trace('<', "shm_ready", 0, 8, "");
                    return Ok(Some(generation));
                }
                Message::Error { .. } => return Ok(None),
                other => {
                    if let Some(event) = self.handle_message(other)? {
                        self.pending.push_back(event);
                    }
                }
            }
        }
    }

    fn trace(&mut self, direction: char, kind: &str, flags: u16, length: usize, detail: &str) {
        let Some(trace) = self.trace.as_mut() else {
            return;
        };
        if detail.is_empty() {
            let _ = writeln!(trace, "{direction} {kind} flags={flags:#x} len={length}");
        } else {
            let _ = writeln!(
                trace,
                "{direction} {kind} flags={flags:#x} len={length} {detail}"
            );
        }
        let _ = trace.flush();
    }

    fn send(&mut self, message: &Message) -> io::Result<()> {
        let encoded = message.encode();
        if encoded.len() > HEADER_LEN + MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PDP/1 message too large",
            ));
        }
        self.stream.write_all(&encoded)?;
        self.stream.flush()?;
        if self.trace.is_some() {
            let detail = match message {
                Message::Commit {
                    window_id,
                    width,
                    height,
                    pixels,
                    ..
                } => format!(
                    "window={window_id} {width}x{height} pixels={}",
                    pixels.len()
                ),
                Message::ShmOffer {
                    window_id,
                    width,
                    height,
                    stride,
                    ..
                } => format!("window={window_id} {width}x{height} stride={stride} fd"),
                Message::ShmRelease {
                    window_id,
                    generation,
                } => format!("window={window_id} gen={generation}"),
                _ => String::new(),
            };
            self.trace(
                '>',
                message.name(),
                message.header_flags(),
                encoded.len() - HEADER_LEN,
                &detail,
            );
        }
        Ok(())
    }

    /// Poll for events, waiting at most `timeout` for the first byte.
    pub fn poll_events(&mut self, timeout: Duration) -> io::Result<Vec<Event>> {
        let mut events = Vec::new();
        while let Some(event) = self.pending.pop_front() {
            events.push(event);
        }
        if !events.is_empty() {
            return Ok(events);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(message) = self.reader.next().map_err(io::Error::from)? {
                if let Some(event) = self.handle_message(message)? {
                    events.push(event);
                }
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            self.stream
                .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))?;
            let mut buf = [0u8; 64 * 1024];
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    self.peer_closed = true;
                    events.push(Event::Close {
                        window_id: self.window_id,
                        reason: close_reason::DISCONNECT,
                    });
                    return Ok(events);
                }
                Ok(n) => self.reader.push(&buf[..n]),
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut =>
                {
                    if events.is_empty() && !remaining.is_zero() {
                        continue;
                    }
                    return Ok(events);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn handle_message(&mut self, message: Message) -> io::Result<Option<Event>> {
        match message {
            Message::Welcome {
                client_id,
                width,
                height,
                flags,
                ..
            } => {
                self.client_id = client_id;
                self.screen_width = width;
                self.screen_height = height;
                self.flags = flags;
                self.format = FrameFormat::from_welcome_flags(flags);
                Ok(None)
            }
            Message::Configure {
                window_id,
                x,
                y,
                width,
                height,
                flags,
            } => {
                if window_id == self.window_id {
                    self.width = width;
                    self.height = height;
                }
                Ok(Some(Event::Configure(Configure {
                    window_id,
                    x,
                    y,
                    width,
                    height,
                    flags,
                })))
            }
            Message::Input { window_id, event } => {
                if window_id != self.window_id && self.window_id != 0 {
                    return Ok(None);
                }
                Ok(Some(Event::Input(event)))
            }
            Message::Focus { window_id, focused } => Ok(Some(Event::Focus { window_id, focused })),
            Message::Close { window_id, reason } => Ok(Some(Event::Close { window_id, reason })),
            Message::Error { code, message } => Ok(Some(Event::Error { code, message })),
            Message::Ping { nonce } => {
                self.send(&Message::Pong { nonce })?;
                Ok(Some(Event::Ping(nonce)))
            }
            Message::Pong { nonce } => Ok(Some(Event::Pong(nonce))),
            Message::ShmReady {
                window_id,
                generation,
            } => {
                if window_id == self.window_id {
                    self.generation = generation;
                }
                Ok(Some(Event::ShmReady {
                    window_id,
                    generation,
                }))
            }
            Message::ShmRelease {
                window_id,
                generation,
            } => {
                if window_id == self.window_id && generation == self.generation {
                    self.generation = 0;
                }
                Ok(Some(Event::ShmRelease {
                    window_id,
                    generation,
                }))
            }
            _ => Ok(None),
        }
    }

    /// Read one message with a deadline, preserving partial reads.
    fn next_message(&mut self, timeout: Duration) -> io::Result<Option<Message>> {
        if let Some(message) = self.reader.next().map_err(io::Error::from)? {
            return Ok(Some(message));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            self.stream
                .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))?;
            let mut buf = [0u8; 64 * 1024];
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    self.peer_closed = true;
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "server closed the connection",
                    ));
                }
                Ok(n) => {
                    self.reader.push(&buf[..n]);
                    if let Some(message) = self.reader.next().map_err(io::Error::from)? {
                        return Ok(Some(message));
                    }
                }
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut =>
                {
                    return Ok(None)
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

/// Convert a server error code into a short diagnostic.
pub fn error_name(code: u32) -> &'static str {
    match code {
        protocol::error_codes::BAD_MESSAGE => "bad message",
        protocol::error_codes::NOT_HELLO => "not hello",
        protocol::error_codes::DUPLICATE_WINDOW => "duplicate window",
        protocol::error_codes::UNKNOWN_WINDOW => "unknown window",
        protocol::error_codes::NOT_OWNER => "not owner",
        protocol::error_codes::INVALID_SURFACE => "invalid surface",
        protocol::error_codes::LIMIT => "window limit",
        protocol::error_codes::UNSUPPORTED => "unsupported",
        protocol::error_codes::INTERNAL => "internal",
        protocol::error_codes::STALE_GENERATION => "stale shm generation",
        _ => "unknown",
    }
}

/// Convert a close reason into a short diagnostic.
pub fn close_name(reason: u32) -> &'static str {
    match reason {
        close_reason::CLIENT => "client",
        close_reason::SERVER => "server",
        close_reason::DISCONNECT => "disconnect",
        close_reason::PROTOCOL_ERROR => "protocol error",
        close_reason::DESTROYED => "destroyed",
        _ => "unknown",
    }
}

/// Convenience: map modifier constants to xterm bits.
pub fn xterm_mouse_mods(modifiers: u32) -> u16 {
    let mut value = 0;
    if modifiers & mods::SHIFT != 0 {
        value |= 4;
    }
    if modifiers & mods::ALT != 0 {
        value |= 8;
    }
    if modifiers & mods::CTRL != 0 {
        value |= 16;
    }
    value
}

/// Convenience: convert a PDP/1 mouse button id to a held-buttons mask.
pub fn button_mask(button: u32) -> u32 {
    match button {
        buttons::LEFT => buttons::MASK_LEFT,
        buttons::MIDDLE => buttons::MASK_MIDDLE,
        buttons::RIGHT => buttons::MASK_RIGHT,
        _ => 0,
    }
}

/// Convert a protocol error to `io::Error` (kept for callers that only use the
/// client API).
pub fn protocol_error_to_io(error: ProtocolError) -> io::Error {
    io::Error::from(error)
}

/// Check that the server's advertised flags support at least one frame format.
pub fn server_supports_a_frame(flags: u32) -> bool {
    flags & (welcome_flags::FORMAT_XRGB8888 | welcome_flags::FORMAT_RGB565) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_socket_is_the_server_default() {
        assert_eq!(DEFAULT_SOCKET, "/tmp/pagh-desktop.sock");
    }

    #[test]
    fn commit_overhead_is_header_plus_commit_prefix() {
        assert_eq!(COMMIT_OVERHEAD, 36);
    }

    #[test]
    fn names_are_stable() {
        assert_eq!(
            error_name(protocol::error_codes::INVALID_SURFACE),
            "invalid surface"
        );
        assert_eq!(close_name(close_reason::SERVER), "server");
        assert_eq!(xterm_mouse_mods(mods::SHIFT | mods::CTRL), 20);
        assert_eq!(button_mask(buttons::RIGHT), buttons::MASK_RIGHT);
        assert!(server_supports_a_frame(welcome_flags::FORMAT_RGB565));
        assert!(!server_supports_a_frame(welcome_flags::SHM_SURFACE));
    }
}
