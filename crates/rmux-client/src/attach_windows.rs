//! Windows attach-mode client.

use std::io::{self, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use rmux_ipc::BlockingLocalStream;
use rmux_proto::{
    encode_attach_message, AttachFrameDecoder, AttachMessage, AttachedKeystroke, RmuxError,
};

use crate::ClientError;

#[path = "attach/screen.rs"]
mod screen;
#[path = "attach_windows/terminal.rs"]
mod terminal;

use screen::{
    contains_subslice, AttachScreenTracker, AttachStopDetector, ALT_SCREEN_EXIT_FALLBACK,
    DETACHED_BANNER_PREFIX, EXITED_BANNER,
};
pub use terminal::{AttachError, RawTerminal, Result};

const READ_BUFFER_SIZE: usize = 8192;

/// Runs the attach loop using the process stdin/stdout streams.
pub fn attach_terminal(stream: BlockingLocalStream) -> std::result::Result<(), ClientError> {
    let input = io::stdin();
    let output = io::stdout();

    attach_with_stdio(stream, input, output)
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
    attach_with_stdio(stream, input, output)
}

fn attach_with_stdio<Input, Output>(
    stream: BlockingLocalStream,
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
    _raw_terminal: &RawTerminal,
    screen_tracker: &AttachScreenTracker,
    input: Input,
    output: Output,
) -> std::result::Result<(), ClientError>
where
    Input: Read + AsRawHandle + Send + 'static,
    Output: Write + Send + 'static,
{
    if let Some(size) = terminal::current_size() {
        write_attach_message(&mut stream, AttachMessage::Resize(size))?;
    }

    drive_attach_stream_inner(stream, screen_tracker.clone(), input, output)
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
    drive_attach_stream_inner(stream, AttachScreenTracker::default(), input, output)
}

fn drive_attach_stream_inner<Input, Output>(
    stream: BlockingLocalStream,
    screen_tracker: AttachScreenTracker,
    input: Input,
    output: Output,
) -> std::result::Result<(), ClientError>
where
    Input: Read + AsRawHandle + Send + 'static,
    Output: Write + Send + 'static,
{
    let mut input_stream = stream.try_clone().map_err(ClientError::Io)?;
    let closed = Arc::new(AtomicBool::new(false));
    let input_closed = Arc::clone(&closed);

    let input_thread = thread::spawn(move || input_loop(&mut input_stream, input, input_closed));
    let output_result = output_loop(stream, output, Arc::clone(&closed), screen_tracker);
    closed.store(true, Ordering::SeqCst);

    if input_thread.is_finished() {
        join_attach_thread(input_thread)??;
    }

    output_result
}

fn input_loop<Input>(
    stream: &mut BlockingLocalStream,
    mut input: Input,
    closed: Arc<AtomicBool>,
) -> std::result::Result<(), ClientError>
where
    Input: Read + AsRawHandle,
{
    let mut read_buffer = [0_u8; READ_BUFFER_SIZE];
    let input_handle = input.as_raw_handle();

    loop {
        if closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        if !terminal::wait_for_input(input_handle, 50).map_err(ClientError::Io)? {
            continue;
        }

        let bytes_read = match input.read(&mut read_buffer) {
            Ok(0) => return Ok(()),
            Ok(bytes_read) => bytes_read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(ClientError::Io(error)),
        };

        write_attach_message(
            stream,
            AttachMessage::Keystroke(AttachedKeystroke::new(read_buffer[..bytes_read].to_vec())),
        )?;
    }
}

fn output_loop<Output>(
    mut stream: BlockingLocalStream,
    mut output: Output,
    closed: Arc<AtomicBool>,
    screen_tracker: AttachScreenTracker,
) -> std::result::Result<(), ClientError>
where
    Output: Write,
{
    let mut decoder = AttachFrameDecoder::new();
    let mut read_buffer = [0_u8; READ_BUFFER_SIZE];
    let mut stop_detector = AttachStopDetector::new(screen_tracker.clone());

    loop {
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
                    closed.store(true, Ordering::SeqCst);
                    return Ok(());
                }
                AttachMessage::Lock(_) | AttachMessage::Suspend => {
                    write_attach_message(&mut stream, AttachMessage::Unlock)?;
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

        let bytes_read = match stream.read(&mut read_buffer) {
            Ok(0) => {
                closed.store(true, Ordering::SeqCst);
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

fn write_attach_message(
    stream: &mut BlockingLocalStream,
    message: AttachMessage,
) -> std::result::Result<(), ClientError> {
    let frame = encode_attach_message(&message).map_err(ClientError::from)?;
    stream.write_all(&frame).map_err(ClientError::Io)
}

fn join_attach_thread(
    thread: thread::JoinHandle<std::result::Result<(), ClientError>>,
) -> std::result::Result<std::result::Result<(), ClientError>, ClientError> {
    thread
        .join()
        .map_err(|_| ClientError::Io(io::Error::other("attach thread panicked")))
}
