use std::os::fd::{BorrowedFd, OwnedFd, RawFd};

use std::io;

use rustix::fs::{fcntl_getfl, fcntl_setfl, open, Mode, OFlags};
use rustix::process::{
    getpid, ioctl_tiocsctty, kill_process as rustix_kill_process, kill_process_group, setsid,
};
use rustix::pty::{grantpt, ioctl_tiocgptpeer, openpt, ptsname, unlockpt, OpenptFlags};
use rustix::termios::{tcgetwinsize, tcsetpgrp, tcsetwinsize};

use crate::{size, ProcessId, Result, Signal, TerminalSize};

pub(crate) fn open_pty_pair() -> Result<(OwnedFd, OwnedFd)> {
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?;
    grantpt(&master)?;
    unlockpt(&master)?;

    let slave = open_slave(&master)?;

    Ok((master, slave))
}

fn open_slave(master: &OwnedFd) -> Result<OwnedFd> {
    match ioctl_tiocgptpeer(
        master,
        OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC,
    ) {
        Ok(slave) => Ok(slave),
        Err(peer_error) => open_slave_by_name(master).map_err(|_| peer_error.into()),
    }
}

fn open_slave_by_name(master: &OwnedFd) -> Result<OwnedFd> {
    let slave_name = ptsname(master, Vec::new())?;
    Ok(open(
        slave_name.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

pub(crate) fn query_size(fd: BorrowedFd<'_>) -> Result<TerminalSize> {
    Ok(size::from_winsize(tcgetwinsize(fd)?))
}

pub(crate) fn apply_size(fd: BorrowedFd<'_>, size: TerminalSize) -> Result<()> {
    tcsetwinsize(fd, size::into_winsize(size))?;
    Ok(())
}

pub(crate) fn setup_child_controlling_terminal(raw_master_fd: RawFd) -> std::io::Result<()> {
    // SAFETY: This closes only the child process' inherited copy of the PTY
    // master fd. The parent still owns its separate descriptor.
    unsafe { rustix::io::close(raw_master_fd) };

    setsid().map_err(std::io::Error::from)?;

    // SAFETY: `stdin` has already been wired to the PTY slave by `Command`, so
    // fd 0 is a valid borrowed descriptor for the rest of the pre-exec setup.
    let slave_stdin = unsafe { BorrowedFd::borrow_raw(0) };
    ioctl_tiocsctty(slave_stdin).map_err(std::io::Error::from)?;
    tcsetpgrp(slave_stdin, getpid()).map_err(std::io::Error::from)?;

    Ok(())
}

pub(crate) fn kill_foreground_process_group(pid: ProcessId, signal: Signal) -> Result<()> {
    kill_process_group(pid.as_rustix_pid()?, signal.as_rustix_signal())?;
    Ok(())
}

pub(crate) fn kill_process(pid: ProcessId, signal: Signal) -> Result<()> {
    rustix_kill_process(pid.as_rustix_pid()?, signal.as_rustix_signal())?;
    Ok(())
}

pub(crate) fn read(fd: BorrowedFd<'_>, buffer: &mut [u8]) -> io::Result<usize> {
    rustix::io::read(fd, buffer).map_err(io::Error::from)
}

pub(crate) fn write_all(fd: BorrowedFd<'_>, mut buffer: &[u8]) -> io::Result<()> {
    while !buffer.is_empty() {
        match rustix::io::write(fd, buffer) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")),
            Ok(bytes_written) => buffer = &buffer[bytes_written..],
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }

    Ok(())
}

pub(crate) fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    let flags = fcntl_getfl(fd).map_err(io::Error::other)?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK).map_err(io::Error::other)?;
    Ok(())
}
