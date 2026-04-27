use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Write};

use rmux_core::alternate_screen_exit_sequence;
use rmux_proto::TerminalSize;
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_INVALID_HANDLE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Console::{
    FlushConsoleInputBuffer, GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle,
    SetConsoleMode, CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
    ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const DISABLE_MOUSE_FALLBACK: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1006l";
const DISABLE_BRACKETED_PASTE_FALLBACK: &[u8] = b"\x1b[?2004l";
const DISABLE_FOCUS_FALLBACK: &[u8] = b"\x1b[?1004l";
const DISABLE_EXTENDED_KEYS_FALLBACK: &[u8] = b"\x1b[>4m";
const DISABLE_MARGINS_FALLBACK: &[u8] = b"\x1b[?69l";
const RESET_CURSOR_STYLE_FALLBACK: &[u8] = b"\x1b[2 q";
const RESET_CURSOR_COLOUR_FALLBACK: &[u8] = b"\x1b]112\x07";

/// Result type for raw-terminal lifecycle operations.
pub type Result<T> = std::result::Result<T, AttachError>;

/// Errors produced while entering or restoring raw terminal mode.
#[derive(Debug)]
pub enum AttachError {
    /// A Win32 console operation failed.
    Io(io::Error),
}

impl fmt::Display for AttachError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "terminal console operation failed: {error}"),
        }
    }
}

impl StdError for AttachError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io(error) => Some(error),
        }
    }
}

impl From<io::Error> for AttachError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A drop guard that applies Windows console raw-ish VT mode and restores the
/// original settings when dropped.
#[derive(Debug)]
#[must_use = "keep the guard alive for as long as raw terminal mode is required"]
pub struct RawTerminal {
    input: Option<ConsoleMode>,
    output: Option<ConsoleMode>,
}

impl RawTerminal {
    /// Enters raw mode for process stdin/stdout console handles.
    pub fn enter() -> Result<Self> {
        let input = ConsoleMode::for_std_handle(STD_INPUT_HANDLE)?;
        let output = ConsoleMode::for_std_handle(STD_OUTPUT_HANDLE)?;

        if let Some(input) = &input {
            let raw = (input.original | ENABLE_VIRTUAL_TERMINAL_INPUT)
                & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
            input.set(raw)?;
        }
        if let Some(output) = &output {
            output.set(output.original | ENABLE_VIRTUAL_TERMINAL_PROCESSING)?;
        }

        Ok(Self { input, output })
    }

    /// Restores the terminal settings captured when the guard was created.
    pub fn restore(&self) -> Result<()> {
        if let Some(input) = &self.input {
            input.restore()?;
        }
        if let Some(output) = &self.output {
            output.restore()?;
        }
        Ok(())
    }

    pub(super) fn restore_attach_terminal_state(&self) -> Result<()> {
        let mut stdout = io::stdout();
        let term = std::env::var("TERM").unwrap_or_default();
        stdout.write_all(&fallback_attach_stop_sequence(&term))?;
        stdout.flush()?;
        Ok(())
    }

    pub(super) fn flush_pending_input(&self) -> Result<()> {
        let Some(input) = &self.input else {
            return Ok(());
        };
        let ok = unsafe {
            // SAFETY: input.handle is a valid console input handle captured by ConsoleMode.
            FlushConsoleInputBuffer(input.handle)
        };
        if ok == 0 {
            return Err(AttachError::Io(io::Error::last_os_error()));
        }
        Ok(())
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[derive(Debug)]
struct ConsoleMode {
    handle: HANDLE,
    original: u32,
}

impl ConsoleMode {
    fn for_std_handle(handle_id: u32) -> Result<Option<Self>> {
        let handle = std_handle(handle_id)?;
        let Some(handle) = handle else {
            return Ok(None);
        };

        let mut mode = 0;
        let ok = unsafe {
            // SAFETY: handle is a valid std handle and mode points to writable storage.
            GetConsoleMode(handle, &mut mode)
        };
        if ok == 0 {
            return console_mode_absent_or_error();
        }

        Ok(Some(Self {
            handle,
            original: mode,
        }))
    }

    fn set(&self, mode: u32) -> Result<()> {
        let ok = unsafe {
            // SAFETY: self.handle is a console handle and mode is a bitmask accepted by Win32.
            SetConsoleMode(self.handle, mode)
        };
        if ok == 0 {
            return Err(AttachError::Io(io::Error::last_os_error()));
        }
        Ok(())
    }

    fn restore(&self) -> Result<()> {
        self.set(self.original)
    }
}

pub(super) fn current_size() -> Option<TerminalSize> {
    let handle = std_handle(STD_OUTPUT_HANDLE).ok().flatten()?;
    let mut info = std::mem::MaybeUninit::<CONSOLE_SCREEN_BUFFER_INFO>::zeroed();
    let ok = unsafe {
        // SAFETY: info is writable for the Win32 structure expected by this API.
        GetConsoleScreenBufferInfo(handle, info.as_mut_ptr())
    };
    if ok == 0 {
        return None;
    }

    let info = unsafe {
        // SAFETY: Win32 reported that it initialized the structure.
        info.assume_init()
    };
    let width = info.srWindow.Right - info.srWindow.Left + 1;
    let height = info.srWindow.Bottom - info.srWindow.Top + 1;
    let cols = u16::try_from(width).ok()?;
    let rows = u16::try_from(height).ok()?;
    (cols > 0 && rows > 0).then_some(TerminalSize { cols, rows })
}

pub(super) fn wait_for_input(handle: HANDLE, timeout_ms: u32) -> io::Result<bool> {
    match unsafe {
        // SAFETY: handle is borrowed only for the duration of this wait.
        WaitForSingleObject(handle, timeout_ms)
    } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

fn std_handle(handle_id: u32) -> Result<Option<HANDLE>> {
    let handle = unsafe {
        // SAFETY: GetStdHandle accepts the documented STD_* constants.
        GetStdHandle(handle_id)
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Ok(None);
    }
    Ok(Some(handle))
}

fn console_mode_absent_or_error<T>() -> Result<Option<T>> {
    let error = unsafe {
        // SAFETY: GetLastError reads the calling thread's last Win32 error.
        GetLastError()
    };
    if error == ERROR_INVALID_HANDLE {
        return Ok(None);
    }
    Err(AttachError::Io(io::Error::from_raw_os_error(
        i32::try_from(error).unwrap_or(i32::MAX),
    )))
}

fn fallback_attach_stop_sequence(term: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(RESET_CURSOR_COLOUR_FALLBACK);
    bytes.extend_from_slice(RESET_CURSOR_STYLE_FALLBACK);
    bytes.extend_from_slice(DISABLE_FOCUS_FALLBACK);
    bytes.extend_from_slice(DISABLE_EXTENDED_KEYS_FALLBACK);
    bytes.extend_from_slice(DISABLE_MARGINS_FALLBACK);
    bytes.extend_from_slice(DISABLE_MOUSE_FALLBACK);
    bytes.extend_from_slice(DISABLE_BRACKETED_PASTE_FALLBACK);
    bytes.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
    bytes.extend_from_slice(alternate_screen_exit_sequence(term));
    bytes
}
