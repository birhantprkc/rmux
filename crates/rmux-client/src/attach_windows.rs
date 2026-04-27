//! Windows attach-mode client.

use std::io::{self, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::thread;

use rmux_ipc::BlockingLocalStream;
use rmux_proto::{
    encode_attach_message, AttachFrameDecoder, AttachMessage, AttachedKeystroke, RmuxError,
    TerminalSize,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::ClientError;

#[path = "attach/screen.rs"]
mod screen;
#[path = "attach_windows/terminal.rs"]
mod terminal;
#[path = "attach/terminal_cleanup.rs"]
mod terminal_cleanup;

use screen::{
    contains_subslice, AttachScreenTracker, AttachStopDetector, ALT_SCREEN_EXIT_FALLBACK,
    DETACHED_BANNER_PREFIX, EXITED_BANNER,
};
pub use terminal::{AttachError, RawTerminal, Result};

const READ_BUFFER_SIZE: usize = 8192;

/// Runs the attach loop using the process stdin/stdout streams.
pub fn attach_terminal(stream: BlockingLocalStream) -> std::result::Result<(), ClientError> {
    attach_terminal_with_initial_bytes(stream, Vec::new())
}

/// Runs the attach loop using process stdin/stdout and pre-read stream bytes.
pub fn attach_terminal_with_initial_bytes(
    stream: BlockingLocalStream,
    initial_bytes: Vec<u8>,
) -> std::result::Result<(), ClientError> {
    let input = io::stdin();
    let output = io::stdout();

    attach_with_stdio(stream, initial_bytes, input, output)
}

/// Runs the attach loop with an explicit terminal handle.
///
/// Windows console mode is process-handle based, so `terminal` is accepted for
/// API parity with Unix but stdin/stdout are used to apply and restore modes.
pub fn attach_with_terminal<Terminal, Input, Output>(
    stream: BlockingLocalStream,
    _terminal: &Terminal,
    input: Input,
    output: Output,
) -> std::result::Result<(), ClientError>
where
    Input: Read + AsRawHandle + Send + 'static,
    Output: Write + Send + 'static,
{
    attach_with_stdio(stream, Vec::new(), input, output)
}

fn attach_with_stdio<Input, Output>(
    stream: BlockingLocalStream,
    initial_bytes: Vec<u8>,
    input: Input,
    output: Output,
) -> std::result::Result<(), ClientError>
where
    Input: Read + AsRawHandle + Send + 'static,
    Output: Write + Send + 'static,
{
    let raw_terminal = RawTerminal::enter().map_err(ClientError::from)?;
    let _ = raw_terminal.flush_pending_input();
    let screen_tracker = AttachScreenTracker::default();
    let result = drive_attach_stream_with_terminal_state(
        stream,
        initial_bytes,
        &raw_terminal,
        &screen_tracker,
        input,
        output,
    );
    if result.is_err() && !screen_tracker.was_stopped() {
        let _ = raw_terminal.restore_attach_terminal_state();
    }
    let _ = raw_terminal.flush_pending_input();
    drop(raw_terminal);
    result
}

fn drive_attach_stream_with_terminal_state<Input, Output>(
    mut stream: BlockingLocalStream,
    initial_bytes: Vec<u8>,
    _raw_terminal: &RawTerminal,
    screen_tracker: &AttachScreenTracker,
    input: Input,
    output: Output,
) -> std::result::Result<(), ClientError>
where
    Input: Read + AsRawHandle + Send + 'static,
    Output: Write + Send + 'static,
{
    let initial_size = terminal::current_size();
    if let Some(size) = initial_size {
        write_attach_message(&mut stream, AttachMessage::Resize(size))?;
    }
    let (resize_tx, resize_rx) = mpsc::unbounded_channel();
    let _resize_watcher = terminal::ResizeWatcher::spawn(initial_size, resize_tx);

    drive_attach_stream_inner(
        stream,
        initial_bytes,
        screen_tracker.clone(),
        input,
        output,
        resize_rx,
    )
}

/// Drives raw attach-stream byte forwarding over an upgraded local stream.
pub fn drive_attach_stream<Input, Output>(
    stream: BlockingLocalStream,
    input: Input,
    output: Output,
) -> std::result::Result<(), ClientError>
where
    Input: Read + AsRawHandle + Send + 'static,
    Output: Write + Send + 'static,
{
    drive_attach_stream_inner(
        stream,
        Vec::new(),
        AttachScreenTracker::default(),
        input,
        output,
        closed_resize_rx(),
    )
}

fn drive_attach_stream_inner<Input, Output>(
    stream: BlockingLocalStream,
    initial_bytes: Vec<u8>,
    screen_tracker: AttachScreenTracker,
    input: Input,
    output: Output,
    resize_rx: mpsc::UnboundedReceiver<TerminalSize>,
) -> std::result::Result<(), ClientError>
where
    Input: Read + AsRawHandle + Send + 'static,
    Output: Write + Send + 'static,
{
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let input_thread = thread::spawn(move || input_loop(input, input_tx));
    let (pipe, runtime) = stream.into_async_parts();
    let output_result = runtime.block_on(async {
        let (reader, writer) = tokio::io::split(pipe);
        drive_async_attach(
            reader,
            writer,
            initial_bytes,
            input_rx,
            resize_rx,
            output,
            screen_tracker,
        )
        .await
    });

    let input_result = join_attach_thread(input_thread)?;

    output_result?;
    input_result
}

fn input_loop<Input>(
    mut input: Input,
    input_tx: mpsc::UnboundedSender<Vec<u8>>,
) -> std::result::Result<(), ClientError>
where
    Input: Read + AsRawHandle,
{
    let mut read_buffer = [0_u8; READ_BUFFER_SIZE];
    let input_handle = input.as_raw_handle();

    loop {
        if !terminal::wait_for_input(input_handle, 50).map_err(ClientError::Io)? {
            if input_tx.is_closed() {
                return Ok(());
            }
            continue;
        }

        let bytes_read = match input.read(&mut read_buffer) {
            Ok(0) => return Ok(()),
            Ok(bytes_read) => bytes_read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(ClientError::Io(error)),
        };

        if input_tx.send(read_buffer[..bytes_read].to_vec()).is_err() {
            return Ok(());
        }
    }
}

async fn drive_async_attach<Reader, Writer, Output>(
    mut reader: Reader,
    mut writer: Writer,
    initial_bytes: Vec<u8>,
    mut input_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    mut resize_rx: mpsc::UnboundedReceiver<TerminalSize>,
    mut output: Output,
    screen_tracker: AttachScreenTracker,
) -> std::result::Result<(), ClientError>
where
    Reader: tokio::io::AsyncRead + Unpin,
    Writer: tokio::io::AsyncWrite + Unpin,
    Output: Write,
{
    let mut decoder = AttachFrameDecoder::new();
    decoder.push_bytes(&initial_bytes);
    let mut read_buffer = [0_u8; READ_BUFFER_SIZE];
    let mut stop_detector = AttachStopDetector::new(screen_tracker.clone());

    loop {
        if drain_attach_messages(
            &mut decoder,
            &mut writer,
            &mut output,
            &screen_tracker,
            &mut stop_detector,
        )
        .await?
        .should_exit()
        {
            return Ok(());
        }

        tokio::select! {
            bytes = input_rx.recv() => {
                let Some(bytes) = bytes else {
                    continue;
                };
                write_async_attach_message(
                    &mut writer,
                    AttachMessage::Keystroke(AttachedKeystroke::new(bytes)),
                ).await?;
            }
            size = resize_rx.recv() => {
                let Some(size) = size else {
                    continue;
                };
                write_async_attach_message(
                    &mut writer,
                    AttachMessage::Resize(size),
                ).await?;
            }
            read = reader.read(&mut read_buffer) => {
                let bytes_read = match read {
                    Ok(0) => {
                        if screen_tracker.was_stopped() {
                            return Ok(());
                        }
                        return Err(ClientError::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "attach stream closed before attach-stop sequence",
                        )));
                    }
                    Ok(bytes_read) => bytes_read,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error)
                        if screen_tracker.was_stopped()
                            && matches!(
                                error.kind(),
                                io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
                            ) =>
                    {
                        return Ok(());
                    }
                    Err(error) => return Err(ClientError::Io(error)),
                };
                decoder.push_bytes(&read_buffer[..bytes_read]);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DrainOutcome {
    Continue,
    Exit,
}

impl DrainOutcome {
    const fn should_exit(self) -> bool {
        matches!(self, Self::Exit)
    }
}

async fn drain_attach_messages<Writer, Output>(
    decoder: &mut AttachFrameDecoder,
    writer: &mut Writer,
    output: &mut Output,
    screen_tracker: &AttachScreenTracker,
    stop_detector: &mut AttachStopDetector,
) -> std::result::Result<DrainOutcome, ClientError>
where
    Writer: tokio::io::AsyncWrite + Unpin,
    Output: Write,
{
    while let Some(message) = decoder.next_message().map_err(ClientError::from)? {
        match message {
            AttachMessage::Data(bytes) => {
                if contains_subslice(&bytes, ALT_SCREEN_EXIT_FALLBACK)
                    || contains_subslice(&bytes, DETACHED_BANNER_PREFIX)
                    || contains_subslice(&bytes, EXITED_BANNER)
                {
                    screen_tracker.mark_stopped();
                }
                stop_detector.observe(&bytes);
                output.write_all(&bytes).map_err(ClientError::Io)?;
                output.flush().map_err(ClientError::Io)?;
            }
            AttachMessage::KeyDispatched(_) => {}
            AttachMessage::DetachKill | AttachMessage::DetachExec(_) => {
                return Ok(DrainOutcome::Exit);
            }
            AttachMessage::Lock(_) | AttachMessage::Suspend => {
                write_async_attach_message(writer, AttachMessage::Unlock).await?;
            }
            AttachMessage::Resize(_) => {
                return Err(ClientError::Protocol(RmuxError::Decode(
                    "received unexpected resize message from attach stream".to_owned(),
                )));
            }
            AttachMessage::Unlock => {
                return Err(ClientError::Protocol(RmuxError::Decode(
                    "received unexpected unlock message from attach stream".to_owned(),
                )));
            }
            AttachMessage::Keystroke(_) => {
                return Err(ClientError::Protocol(RmuxError::Decode(
                    "received unexpected keystroke message from attach stream".to_owned(),
                )));
            }
        }
    }

    Ok(DrainOutcome::Continue)
}

fn write_attach_message(
    stream: &mut BlockingLocalStream,
    message: AttachMessage,
) -> std::result::Result<(), ClientError> {
    let frame = encode_attach_message(&message).map_err(ClientError::from)?;
    stream.write_all(&frame).map_err(ClientError::Io)
}

fn closed_resize_rx() -> mpsc::UnboundedReceiver<TerminalSize> {
    let (resize_tx, resize_rx) = mpsc::unbounded_channel();
    drop(resize_tx);
    resize_rx
}

async fn write_async_attach_message<Writer>(
    writer: &mut Writer,
    message: AttachMessage,
) -> std::result::Result<(), ClientError>
where
    Writer: tokio::io::AsyncWrite + Unpin,
{
    let frame = encode_attach_message(&message).map_err(ClientError::from)?;
    writer.write_all(&frame).await.map_err(ClientError::Io)
}

fn join_attach_thread(
    thread: thread::JoinHandle<std::result::Result<(), ClientError>>,
) -> std::result::Result<std::result::Result<(), ClientError>, ClientError> {
    thread
        .join()
        .map_err(|_| ClientError::Io(io::Error::other("attach thread panicked")))
}
