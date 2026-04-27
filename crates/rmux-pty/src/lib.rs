#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

//! PTY allocation, sizing, and child-process management for RMUX.
//!
//! This crate confines PTY and terminal-control boundaries behind a small,
//! documented API that exposes:
//! - PTY allocation,
//! - terminal size query and resize on PTY file descriptors,
//! - child spawning into a controlling terminal-backed PTY, and
//! - child signaling and reaping.

mod backend;
mod child;
mod pty;
mod size;

use std::error::Error as StdError;
use std::ffi::NulError;
use std::fmt;

pub use child::{ChildCommand, PtyChild, SpawnedPty};
#[cfg(unix)]
pub use pty::PtySlave;
pub use pty::{PtyIo, PtyMaster, PtyPair};
pub use size::TerminalSize;

/// A crate-local result type for PTY operations.
pub type Result<T> = std::result::Result<T, PtyError>;

/// A platform-neutral process identifier for PTY children.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ProcessId(u32);

impl ProcessId {
    /// Creates a process identifier from the operating-system value.
    pub fn new(raw: u32) -> Result<Self> {
        if raw == 0 {
            return Err(PtyError::InvalidPid(raw));
        }
        Ok(Self(raw))
    }

    /// Returns the raw operating-system process id.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }

    #[cfg(unix)]
    pub(crate) fn as_rustix_pid(self) -> Result<rustix::process::Pid> {
        let raw = i32::try_from(self.0).map_err(|_| PtyError::InvalidPid(self.0))?;
        rustix::process::Pid::from_raw(raw).ok_or(PtyError::InvalidPid(self.0))
    }
}

/// A high-level child process termination request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PtySignal {
    /// Ask the foreground program to interrupt its current operation.
    Interrupt,
    /// Ask the process group to terminate gracefully.
    Terminate,
    /// Forcefully stop the process group.
    Kill,
    /// Hang up the terminal session.
    Hangup,
}

/// Compatibility signal names used by existing RMUX call sites.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Signal(PtySignal);

impl Signal {
    /// Interrupt request.
    pub const INT: Self = Self(PtySignal::Interrupt);
    /// Termination request.
    pub const TERM: Self = Self(PtySignal::Terminate);
    /// Force-kill request.
    pub const KILL: Self = Self(PtySignal::Kill);
    /// Hangup request.
    pub const HUP: Self = Self(PtySignal::Hangup);

    #[cfg(unix)]
    pub(crate) const fn as_rustix_signal(self) -> rustix::process::Signal {
        match self.0 {
            PtySignal::Interrupt => rustix::process::Signal::INT,
            PtySignal::Terminate => rustix::process::Signal::TERM,
            PtySignal::Kill => rustix::process::Signal::KILL,
            PtySignal::Hangup => rustix::process::Signal::HUP,
        }
    }
}

/// Errors produced by PTY allocation, resize, and child-process operations.
#[derive(Debug)]
pub enum PtyError {
    /// A syscall-backed PTY or terminal-control error.
    #[cfg(unix)]
    Os(rustix::io::Errno),
    /// A child-process spawn or wait error from the standard library.
    Spawn(std::io::Error),
    /// A command path, argument, or environment value contained an interior NUL.
    Nul(NulError),
    /// `std::process` returned a PID that could not be represented by RMUX.
    InvalidPid(u32),
    /// The requested PTY operation is not implemented by this platform backend yet.
    Unsupported(&'static str),
}

impl fmt::Display for PtyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(unix)]
            Self::Os(errno) => write!(formatter, "pty syscall failed: {errno}"),
            Self::Spawn(error) => write!(formatter, "child process operation failed: {error}"),
            Self::Nul(error) => write!(
                formatter,
                "interior NUL byte in process configuration: {error}"
            ),
            Self::InvalidPid(pid) => {
                write!(formatter, "child process returned an invalid pid: {pid}")
            }
            Self::Unsupported(operation) => {
                write!(
                    formatter,
                    "pty operation is unsupported on this platform: {operation}"
                )
            }
        }
    }
}

impl StdError for PtyError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            #[cfg(unix)]
            Self::Os(errno) => Some(errno),
            Self::Spawn(error) => Some(error),
            Self::Nul(error) => Some(error),
            Self::InvalidPid(_) => None,
            Self::Unsupported(_) => None,
        }
    }
}

#[cfg(unix)]
impl From<rustix::io::Errno> for PtyError {
    fn from(value: rustix::io::Errno) -> Self {
        Self::Os(value)
    }
}

impl From<std::io::Error> for PtyError {
    fn from(value: std::io::Error) -> Self {
        Self::Spawn(value)
    }
}

impl From<NulError> for PtyError {
    fn from(value: NulError) -> Self {
        Self::Nul(value)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(all(not(unix), not(windows)))]
    #[test]
    fn unsupported_backend_returns_explicit_errors() {
        use super::{ChildCommand, PtyError};

        let spawn = ChildCommand::new("cmd.exe")
            .spawn()
            .expect_err("Windows PTY backend is introduced in Milestone 5");
        assert!(matches!(spawn, PtyError::Unsupported("spawn pty child")));
    }
}
