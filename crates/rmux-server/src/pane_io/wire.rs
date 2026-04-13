use std::future::pending;
use std::io;
use std::os::fd::OwnedFd;

use rmux_proto::{encode_attach_message, AttachFrameDecoder, AttachMessage};
use rmux_pty::PtyMaster;
use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
use rustix::net::RecvFlags;
use tokio::io::{unix::AsyncFd, Interest};
use tokio::net::UnixStream;
use tokio::sync::broadcast;
use tracing::warn;

use crate::outer_terminal::OuterTerminal;

use super::types::{AttachTarget, OpenAttachTarget, PaneOutputReceiver};

pub(super) fn open_attach_target(target: AttachTarget) -> io::Result<OpenAttachTarget> {
    let AttachTarget {
        session_name,
        pane_master,
        pane_output,
        render_frame,
        outer_terminal,
        cursor_style,
        persistent_overlay_state_id,
    } = target;
    let pane_writer = open_pane_writer(pane_master)?;

    Ok(OpenAttachTarget {
        session_name,
        pane_writer,
        pane_output: Some(pane_output.subscribe()),
        render_frame,
        outer_terminal,
        cursor_style,
        persistent_overlay_state_id,
    })
}

pub(super) fn open_pane_writer(pane_master: PtyMaster) -> io::Result<AsyncFd<OwnedFd>> {
    let pane_writer = pane_master.into_owned_fd();
    make_nonblocking(&pane_writer)?;

    AsyncFd::new(pane_writer)
}

pub(super) async fn emit_render_frame(
    stream: &UnixStream,
    outer_terminal: &OuterTerminal,
    render_frame: &[u8],
) -> io::Result<()> {
    let frame = outer_terminal.wrap_render_frame(render_frame);
    emit_attach_bytes(stream, &frame).await
}

pub(super) async fn read_socket_bytes(
    stream: &UnixStream,
    decoder: &mut AttachFrameDecoder,
    buffer: &mut [u8],
) -> io::Result<bool> {
    loop {
        stream.readable().await?;
        match stream.try_read(buffer) {
            Ok(0) => return Ok(false),
            Ok(bytes_read) => {
                decoder.push_bytes(&buffer[..bytes_read]);
                return Ok(true);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
    }
}

pub(super) enum TrySocketRead {
    Read,
    Closed,
    WouldBlock,
}

pub(super) fn try_read_socket_bytes(
    stream: &UnixStream,
    decoder: &mut AttachFrameDecoder,
    buffer: &mut [u8],
) -> io::Result<TrySocketRead> {
    match stream.try_read(buffer) {
        Ok(0) => Ok(TrySocketRead::Closed),
        Ok(bytes_read) => {
            decoder.push_bytes(&buffer[..bytes_read]);
            Ok(TrySocketRead::Read)
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(TrySocketRead::WouldBlock),
        Err(error) => Err(error),
    }
}

pub(super) async fn emit_attach_message(
    stream: &UnixStream,
    message: &AttachMessage,
) -> io::Result<()> {
    let frame = encode_attach_message(message).map_err(io::Error::other)?;
    emit_attach_bytes(stream, &frame).await
}

pub(super) async fn emit_attach_frame(
    stream: &UnixStream,
    message: &AttachMessage,
) -> io::Result<()> {
    let frame = encode_attach_message(message).map_err(io::Error::other)?;
    write_all_to_stream(stream, &frame).await
}

pub(super) async fn recv_pane_output(
    pane_output: &mut PaneOutputReceiver,
) -> io::Result<Option<Vec<u8>>> {
    loop {
        match pane_output.recv().await {
            Ok(bytes) => return Ok(Some(bytes)),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    skipped,
                    "attach pane output receiver lagged; dropping bytes"
                );
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(None),
        }
    }
}

pub(super) async fn recv_pane_output_optional(
    pane_output: Option<&mut PaneOutputReceiver>,
) -> io::Result<Option<Vec<u8>>> {
    match pane_output {
        Some(pane_output) => recv_pane_output(pane_output).await,
        None => pending().await,
    }
}

pub(super) async fn emit_attach_data_frame(stream: &UnixStream, bytes: &[u8]) -> io::Result<()> {
    let frame =
        encode_attach_message(&AttachMessage::Data(bytes.to_vec())).map_err(io::Error::other)?;
    write_all_to_stream(stream, &frame).await
}

pub(super) async fn emit_attach_bytes(stream: &UnixStream, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }

    emit_attach_data_frame(stream, bytes).await
}

pub(super) async fn emit_attach_stop(
    stream: &UnixStream,
    current_target: &OpenAttachTarget,
) -> io::Result<()> {
    emit_attach_bytes(
        stream,
        &current_target.outer_terminal.attach_stop_sequence(),
    )
    .await
}

pub(super) async fn emit_detached_message(
    stream: &UnixStream,
    current_target: &OpenAttachTarget,
) -> io::Result<()> {
    emit_attach_bytes(
        stream,
        format!(
            "[detached (from session {})]\r\n",
            current_target.session_name
        )
        .as_bytes(),
    )
    .await
}

pub(super) async fn emit_exited_message(stream: &UnixStream) -> io::Result<()> {
    emit_attach_bytes(stream, b"[exited]\r\n").await
}

pub(super) async fn read_from_pane(
    pane_reader: &AsyncFd<OwnedFd>,
    buffer: &mut [u8],
) -> io::Result<usize> {
    loop {
        let mut ready = pane_reader.readable().await?;
        match ready.try_io(|inner| {
            rustix::io::read(inner.get_ref(), &mut *buffer).map_err(io::Error::from)
        }) {
            Ok(Ok(bytes_read)) => return Ok(bytes_read),
            Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
            Ok(Err(error))
                if error.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) =>
            {
                return Ok(0);
            }
            Ok(Err(error)) => return Err(error),
            Err(_would_block) => continue,
        }
    }
}

async fn write_all_to_stream(stream: &UnixStream, mut bytes: &[u8]) -> io::Result<()> {
    let mut probe = [0_u8; 1];
    while !bytes.is_empty() {
        let ready = stream
            .ready(Interest::READABLE | Interest::WRITABLE)
            .await?;
        if ready.is_readable() && peer_disconnected(stream, &mut probe)? {
            return Ok(());
        }
        if !ready.is_writable() {
            continue;
        }

        match stream.try_write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "write returned 0 while forwarding pane bytes",
                ));
            }
            Ok(bytes_written) => bytes = &bytes[bytes_written..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

fn peer_disconnected(stream: &UnixStream, probe: &mut [u8; 1]) -> io::Result<bool> {
    match rustix::net::recv(stream, probe, RecvFlags::PEEK) {
        Ok((_initialized, 0)) => Ok(true),
        Ok((_initialized, _available)) => Ok(false),
        Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => Ok(false),
        Err(rustix::io::Errno::PIPE | rustix::io::Errno::CONNRESET) => Ok(true),
        Err(error) => Err(io::Error::from(error)),
    }
}

pub(super) fn invalid_attach_message(error: rmux_proto::RmuxError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn make_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let flags = fcntl_getfl(fd).map_err(io::Error::other)?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK).map_err(io::Error::other)
}
