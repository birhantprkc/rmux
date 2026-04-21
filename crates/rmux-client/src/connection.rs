//! Blocking Unix-socket transport for detached RPC traffic.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rmux_proto::{
    decode_frame, encode_frame, AttachSessionResponse, ControlMode, ControlModeResponse,
    FrameDecoder, Request, Response, RmuxError, DEFAULT_MAX_FRAME_LENGTH,
};
use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::net::sockopt::socket_error;
use rustix::net::{
    connect as socket_connect, socket_with, AddressFamily, SocketAddrUnix, SocketFlags, SocketType,
};
use rustix::process::getuid;

use crate::ClientError;

/// Read buffer size for blocking socket reads.
const READ_BUFFER_SIZE: usize = 8192;
/// Default timeout for detached connects, request writes, and ordinary response
/// reads.
const SOCKET_IO_TIMEOUT: Duration = Duration::from_secs(5);

const DEFAULT_SOCKET_LABEL: &str = "default";
const FALLBACK_SOCKET_ROOT: &str = "/tmp";
const SOCKET_DIR_PREFIX: &str = "rmux";
const RMUX_ENV: &str = "RMUX";
const RMUX_TMPDIR_ENV: &str = "RMUX_TMPDIR";

/// Computes the default RMUX client socket path.
///
/// The path uses an rmux-specific per-user directory so an rmux client never
/// speaks the rmux wire protocol to a real tmux server.
pub fn default_socket_path() -> Result<PathBuf, ClientError> {
    socket_path_for_label(DEFAULT_SOCKET_LABEL)
}

/// Computes an rmux socket path for a top-level `-L` socket name.
pub fn socket_path_for_label(label: impl AsRef<OsStr>) -> Result<PathBuf, ClientError> {
    socket_path_from_parts(
        std::env::var_os(RMUX_TMPDIR_ENV).as_deref(),
        getuid().as_raw(),
        label.as_ref(),
    )
    .map_err(ClientError::Io)
}

/// Resolves the top-level socket path from `-L`, `-S`, `$RMUX`, or defaults.
///
/// `-S` wins over `-L`; both command-line forms win over `$RMUX`.
pub fn resolve_socket_path(
    socket_name: Option<&OsStr>,
    socket_path: Option<&Path>,
) -> Result<PathBuf, ClientError> {
    if let Some(socket_path) = socket_path {
        return Ok(socket_path.to_path_buf());
    }
    if let Some(socket_name) = socket_name {
        return socket_path_for_label(socket_name);
    }
    if let Some(socket_path) = socket_path_from_rmux_env(std::env::var_os(RMUX_ENV).as_deref()) {
        return Ok(socket_path);
    }

    default_socket_path()
}

/// Result of attempting to connect to the RMUX server.
#[derive(Debug)]
pub enum ConnectResult {
    /// Successfully connected to the server.
    Connected(Connection),
    /// The server is absent (socket does not exist or connection refused).
    Absent,
}

/// Attempts to connect to the RMUX server, distinguishing absent servers from
/// real connection errors.
///
/// Returns [`ConnectResult::Absent`] when the socket does not exist or the
/// connection is refused, which lets callers like `kill-session` succeed with
/// exit code `0` for an absent server. Returns an error only for unexpected
/// transport failures.
pub fn connect_or_absent(socket_path: &Path) -> Result<ConnectResult, ClientError> {
    connect_or_absent_with_timeout_using(
        socket_path,
        SOCKET_IO_TIMEOUT,
        connect_stream_with_timeout,
    )
}

/// Connects to the RMUX server, returning an error if the server is absent.
pub fn connect(socket_path: &Path) -> Result<Connection, ClientError> {
    connect_with_timeout_using(socket_path, SOCKET_IO_TIMEOUT, connect_stream_with_timeout)
}

/// A blocking connection to the RMUX server that exchanges typed frames.
#[derive(Debug)]
pub struct Connection {
    stream: UnixStream,
    decoder: FrameDecoder,
}

/// The explicit result of requesting an attach-stream upgrade.
#[derive(Debug)]
pub enum AttachTransition {
    /// The server accepted the attach request and switched protocols.
    Upgraded(AttachSessionUpgrade),
    /// The server responded without switching protocols.
    Rejected(Response),
}

/// The explicit result of requesting a control-mode upgrade.
#[derive(Debug)]
pub enum ControlTransition {
    /// The server accepted the control-mode request and switched protocols.
    Upgraded(ControlModeUpgrade),
    /// The server responded without switching protocols.
    Rejected(Response),
}

/// A detached connection that has transitioned into attach-stream mode.
#[derive(Debug)]
pub struct AttachSessionUpgrade {
    response: AttachSessionResponse,
    stream: UnixStream,
}

/// A detached connection that has transitioned into control-mode streaming.
#[derive(Debug)]
pub struct ControlModeUpgrade {
    pub(crate) response: ControlModeResponse,
    pub(crate) stream: UnixStream,
}

impl AttachSessionUpgrade {
    /// Returns the upgrade response sent by the server.
    #[must_use]
    pub const fn response(&self) -> &AttachSessionResponse {
        &self.response
    }

    /// Consumes the upgrade and returns the raw attach-stream socket.
    #[must_use]
    pub fn into_stream(self) -> UnixStream {
        self.stream
    }
}

impl ControlModeUpgrade {
    /// Returns the upgrade response sent by the server.
    #[must_use]
    pub const fn response(&self) -> &ControlModeResponse {
        &self.response
    }

    /// Returns the negotiated control-mode flavor.
    #[must_use]
    pub const fn mode(&self) -> ControlMode {
        self.response.mode
    }

    /// Consumes the upgrade and returns the raw control-mode socket.
    #[must_use]
    pub fn into_stream(self) -> UnixStream {
        self.stream
    }
}

impl Connection {
    pub(crate) fn new(stream: UnixStream) -> Result<Self, ClientError> {
        stream
            .set_read_timeout(Some(SOCKET_IO_TIMEOUT))
            .map_err(ClientError::Io)?;
        stream
            .set_write_timeout(Some(SOCKET_IO_TIMEOUT))
            .map_err(ClientError::Io)?;

        Ok(Self {
            stream,
            decoder: FrameDecoder::new(),
        })
    }

    /// Sends a request and reads the server's response.
    ///
    /// Server-side `Response::Error` payloads are returned as-is in the `Ok`
    /// variant so callers can pattern-match on them. Only transport and framing
    /// failures produce `Err`.
    pub fn roundtrip(&mut self, request: &Request) -> Result<Response, ClientError> {
        self.write_request(request)?;
        self.read_response()
    }

    /// Sends a request without a detached response read timeout.
    ///
    /// This is reserved for scripting requests whose server-side completion can
    /// legitimately block beyond the normal five-second detached RPC bound.
    pub(crate) fn roundtrip_without_read_timeout(
        &mut self,
        request: &Request,
    ) -> Result<Response, ClientError> {
        let previous_timeout = self.stream.read_timeout().map_err(ClientError::Io)?;
        self.stream
            .set_read_timeout(None)
            .map_err(ClientError::Io)?;
        let result = self.roundtrip(request);
        self.stream
            .set_read_timeout(previous_timeout)
            .map_err(ClientError::Io)?;
        result
    }

    pub(crate) fn write_request(&mut self, request: &Request) -> Result<(), ClientError> {
        let frame = encode_frame(request).map_err(ClientError::Protocol)?;
        self.stream.write_all(&frame).map_err(ClientError::Io)
    }

    fn read_response(&mut self) -> Result<Response, ClientError> {
        let mut buffer = [0u8; READ_BUFFER_SIZE];

        loop {
            match self.decoder.next_frame::<Response>() {
                Ok(Some(response)) => return Ok(response),
                Ok(None) => {}
                Err(error) => return Err(ClientError::Protocol(error)),
            }

            let bytes_read = match self.stream.read(&mut buffer) {
                Ok(bytes_read) => bytes_read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(ClientError::Io(error)),
            };

            if bytes_read == 0 {
                return Err(ClientError::UnexpectedEof);
            }

            self.decoder.push_bytes(&buffer[..bytes_read]);
        }
    }

    pub(crate) fn stream_mut(&mut self) -> &mut UnixStream {
        &mut self.stream
    }

    pub(crate) fn into_attach_upgrade(
        self,
        response: AttachSessionResponse,
    ) -> Result<AttachSessionUpgrade, ClientError> {
        self.stream
            .set_read_timeout(None)
            .map_err(ClientError::Io)?;
        self.stream
            .set_write_timeout(None)
            .map_err(ClientError::Io)?;

        Ok(AttachSessionUpgrade {
            response,
            stream: self.stream,
        })
    }

    pub(crate) fn into_control_upgrade(
        self,
        response: ControlModeResponse,
    ) -> Result<ControlModeUpgrade, ClientError> {
        self.stream
            .set_read_timeout(None)
            .map_err(ClientError::Io)?;
        self.stream
            .set_write_timeout(None)
            .map_err(ClientError::Io)?;

        Ok(ControlModeUpgrade {
            response,
            stream: self.stream,
        })
    }
}

pub(crate) fn read_response_frame_exact(stream: &mut UnixStream) -> Result<Response, ClientError> {
    let mut header = [0_u8; 4];
    read_exact_or_eof(stream, &mut header)?;

    let length = u32::from_le_bytes(header) as usize;
    if length == 0 {
        return Err(ClientError::Protocol(RmuxError::EmptyFrame));
    }
    if length > DEFAULT_MAX_FRAME_LENGTH {
        return Err(ClientError::Protocol(RmuxError::FrameTooLarge {
            length,
            maximum: DEFAULT_MAX_FRAME_LENGTH,
        }));
    }

    let mut payload = vec![0_u8; length];
    read_exact_or_eof(stream, &mut payload)?;

    let mut frame = Vec::with_capacity(4 + length);
    frame.extend_from_slice(&header);
    frame.extend_from_slice(&payload);
    decode_frame(&frame).map_err(ClientError::Protocol)
}

fn read_exact_or_eof(stream: &mut UnixStream, buffer: &mut [u8]) -> Result<(), ClientError> {
    match stream.read_exact(buffer) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            Err(ClientError::UnexpectedEof)
        }
        Err(error) => Err(ClientError::Io(error)),
    }
}

fn socket_path_from_parts(
    rmux_tmpdir: Option<&OsStr>,
    user_id: u32,
    label: &OsStr,
) -> io::Result<PathBuf> {
    let root = socket_root_from_parts(rmux_tmpdir)?;
    let base = root.join(format!("{SOCKET_DIR_PREFIX}-{user_id}"));
    let mut path = base.into_os_string().into_vec();
    path.push(b'/');
    path.extend_from_slice(label.as_bytes());

    Ok(PathBuf::from(OsString::from_vec(path)))
}

fn socket_root_from_parts(rmux_tmpdir: Option<&OsStr>) -> io::Result<PathBuf> {
    let rmux_tmpdir = rmux_tmpdir
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let candidates = rmux_tmpdir
        .into_iter()
        .chain(std::iter::once(PathBuf::from(FALLBACK_SOCKET_ROOT)));

    for candidate in candidates {
        if let Ok(resolved) = fs::canonicalize(&candidate) {
            return Ok(resolved);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no suitable rmux socket directory",
    ))
}

fn socket_path_from_rmux_env(rmux: Option<&OsStr>) -> Option<PathBuf> {
    let rmux = rmux?;
    let bytes = rmux.as_bytes();
    if bytes.is_empty() || bytes.first() == Some(&b',') {
        return None;
    }

    let end = bytes
        .iter()
        .position(|byte| *byte == b',')
        .unwrap_or(bytes.len());
    let path = PathBuf::from(OsString::from_vec(bytes[..end].to_vec()));
    socket_path_is_rmux_owned(&path).then_some(path)
}

fn socket_path_is_rmux_owned(path: &Path) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            name.starts_with(SOCKET_DIR_PREFIX) && name[SOCKET_DIR_PREFIX.len()..].starts_with('-')
        })
}

fn connect_or_absent_with_timeout_using<F>(
    socket_path: &Path,
    timeout: Duration,
    connect_stream: F,
) -> Result<ConnectResult, ClientError>
where
    F: FnOnce(&Path, Duration) -> io::Result<UnixStream>,
{
    match connect_stream(socket_path, timeout) {
        Ok(stream) => Ok(ConnectResult::Connected(Connection::new(stream)?)),
        Err(error) if is_absent_error(&error) => Ok(ConnectResult::Absent),
        Err(error) => Err(ClientError::Io(error)),
    }
}

fn connect_with_timeout_using<F>(
    socket_path: &Path,
    timeout: Duration,
    connect_stream: F,
) -> Result<Connection, ClientError>
where
    F: FnOnce(&Path, Duration) -> io::Result<UnixStream>,
{
    let stream = connect_stream(socket_path, timeout).map_err(ClientError::Io)?;
    Connection::new(stream)
}

fn connect_stream_with_timeout(socket_path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let address = SocketAddrUnix::new(socket_path)?;
    let socket = socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )?;

    match socket_connect(&socket, &address) {
        Ok(()) => {}
        Err(rustix::io::Errno::INPROGRESS | rustix::io::Errno::WOULDBLOCK) => {
            wait_for_socket_connect(&socket, socket_path, timeout)?
        }
        Err(error) => return Err(error.into()),
    }

    let stream = UnixStream::from(socket);
    stream.set_nonblocking(false)?;
    Ok(stream)
}

fn wait_for_socket_connect<Fd>(socket: &Fd, socket_path: &Path, timeout: Duration) -> io::Result<()>
where
    Fd: std::os::fd::AsFd,
{
    wait_for_connect_completion(
        socket_path,
        timeout,
        |remaining| {
            let poll_timeout = Timespec {
                tv_sec: remaining.as_secs() as i64,
                tv_nsec: remaining.subsec_nanos().into(),
            };
            let mut fds = [PollFd::new(
                socket,
                PollFlags::OUT | PollFlags::ERR | PollFlags::HUP,
            )];

            match poll(&mut fds, Some(&poll_timeout)) {
                Ok(0) => Ok(ConnectProgress::Pending),
                Ok(_) => Ok(ConnectProgress::Ready),
                Err(rustix::io::Errno::INTR) => Ok(ConnectProgress::Pending),
                Err(error) => Err(error.into()),
            }
        },
        || match socket_error(socket)? {
            Ok(()) => Ok(ConnectProgress::Ready),
            Err(rustix::io::Errno::INPROGRESS | rustix::io::Errno::WOULDBLOCK) => {
                Ok(ConnectProgress::Pending)
            }
            Err(error) => Err(error.into()),
        },
    )
}

fn connect_timeout_error(socket_path: &Path, timeout: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "timed out after {}s connecting to '{}'",
            timeout.as_secs_f32(),
            socket_path.display()
        ),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectProgress {
    Pending,
    Ready,
}

fn wait_for_connect_completion<P, S>(
    socket_path: &Path,
    timeout: Duration,
    mut wait_for_ready: P,
    mut check_status: S,
) -> io::Result<()>
where
    P: FnMut(Duration) -> io::Result<ConnectProgress>,
    S: FnMut() -> io::Result<ConnectProgress>,
{
    let deadline = Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(connect_timeout_error(socket_path, timeout));
        }

        if wait_for_ready(remaining)? == ConnectProgress::Pending {
            continue;
        }

        if check_status()? == ConnectProgress::Ready {
            return Ok(());
        }
    }
}

/// Returns `true` for I/O errors that indicate the server is not running.
fn is_absent_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

#[cfg(test)]
mod tests {
    include!("connection/tests.rs");
}
