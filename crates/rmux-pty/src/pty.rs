use std::io;

#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
#[cfg(windows)]
use std::sync::Arc;

use crate::backend;
#[cfg(all(not(unix), not(windows)))]
use crate::PtyError;
use crate::{Result, TerminalSize};

#[cfg(unix)]
/// The slave endpoint of a Unix pseudoterminal pair.
#[derive(Debug)]
pub struct PtySlave {
    fd: OwnedFd,
}

#[cfg(unix)]
impl PtySlave {
    /// Duplicates the slave terminal endpoint.
    pub fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            fd: self.fd.try_clone()?,
        })
    }

    /// Consumes the slave endpoint and returns the owned file descriptor.
    #[must_use]
    pub fn into_owned_fd(self) -> OwnedFd {
        self.fd
    }
}

#[cfg(unix)]
impl AsFd for PtySlave {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(unix)]
impl AsRawFd for PtySlave {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// The I/O endpoint for a pseudoterminal.
#[derive(Debug)]
pub struct PtyIo {
    #[cfg(unix)]
    fd: OwnedFd,
    #[cfg(windows)]
    pty: Arc<backend::WindowsPty>,
}

impl PtyIo {
    #[cfg(unix)]
    pub(crate) fn new(fd: OwnedFd) -> Self {
        Self { fd }
    }

    #[cfg(windows)]
    pub(crate) fn new(pty: Arc<backend::WindowsPty>) -> Self {
        Self { pty }
    }

    /// Queries the current terminal geometry for this PTY endpoint.
    pub fn size(&self) -> Result<TerminalSize> {
        #[cfg(unix)]
        {
            backend::query_size(self.fd.as_fd())
        }

        #[cfg(not(unix))]
        {
            #[cfg(windows)]
            {
                backend::query_size(&self.pty)
            }

            #[cfg(not(windows))]
            {
                Err(PtyError::Unsupported("query pty size"))
            }
        }
    }

    /// Resizes this PTY endpoint.
    pub fn resize(&self, size: TerminalSize) -> Result<()> {
        #[cfg(unix)]
        {
            backend::apply_size(self.fd.as_fd(), size)
        }

        #[cfg(not(unix))]
        {
            #[cfg(windows)]
            {
                backend::apply_size(&self.pty, size)
            }

            #[cfg(not(windows))]
            {
                let _ = size;
                Err(PtyError::Unsupported("resize pty"))
            }
        }
    }

    /// Duplicates this PTY I/O endpoint.
    pub fn try_clone(&self) -> Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                fd: self.fd.try_clone()?,
            })
        }

        #[cfg(not(unix))]
        {
            #[cfg(windows)]
            {
                Ok(Self {
                    pty: Arc::clone(&self.pty),
                })
            }

            #[cfg(not(windows))]
            {
                Err(PtyError::Unsupported("clone pty io"))
            }
        }
    }

    /// Reads bytes from this PTY endpoint.
    pub fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            backend::read(self.fd.as_fd(), buffer)
        }

        #[cfg(not(unix))]
        {
            #[cfg(windows)]
            {
                self.pty.read(buffer)
            }

            #[cfg(not(windows))]
            {
                let _ = buffer;
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "pty I/O is unsupported on this platform",
                ))
            }
        }
    }

    /// Writes all bytes to this PTY endpoint.
    pub fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        #[cfg(unix)]
        {
            backend::write_all(self.fd.as_fd(), bytes)
        }

        #[cfg(not(unix))]
        {
            #[cfg(windows)]
            {
                self.pty.write_all(bytes)
            }

            #[cfg(not(windows))]
            {
                let _ = bytes;
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "pty I/O is unsupported on this platform",
                ))
            }
        }
    }

    /// Makes the PTY endpoint nonblocking.
    pub fn set_nonblocking(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            backend::set_nonblocking(self.fd.as_fd())
        }

        #[cfg(not(unix))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "pty I/O is unsupported on this platform",
            ))
        }
    }

    /// Returns a borrowed Unix descriptor for integration points that still
    /// require `AsyncFd`.
    #[cfg(unix)]
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    #[cfg(unix)]
    pub(crate) fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

#[cfg(unix)]
impl AsFd for PtyIo {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(unix)]
impl AsRawFd for PtyIo {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// The master handle of a pseudoterminal.
#[derive(Debug)]
pub struct PtyMaster {
    io: PtyIo,
}

impl PtyMaster {
    #[cfg(unix)]
    pub(crate) fn new(fd: OwnedFd) -> Self {
        Self { io: PtyIo::new(fd) }
    }

    #[cfg(windows)]
    pub(crate) fn new(pty: backend::WindowsPty) -> Self {
        Self {
            io: PtyIo::new(Arc::new(pty)),
        }
    }

    /// Queries the current terminal geometry for this PTY.
    pub fn size(&self) -> Result<TerminalSize> {
        self.io.size()
    }

    /// Resizes this PTY.
    pub fn resize(&self, size: TerminalSize) -> Result<()> {
        self.io.resize(size)
    }

    /// Duplicates the master handle.
    pub fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            io: self.io.try_clone()?,
        })
    }

    /// Duplicates the master handle as an I/O endpoint.
    pub fn try_clone_io(&self) -> Result<PtyIo> {
        self.io.try_clone()
    }

    /// Consumes this master handle into its I/O endpoint.
    #[must_use]
    pub fn into_io(self) -> PtyIo {
        self.io
    }

    /// Consumes this Unix PTY master and returns the owned file descriptor.
    #[cfg(unix)]
    #[must_use]
    pub fn into_owned_fd(self) -> OwnedFd {
        self.io.fd
    }

    /// Returns the PTY I/O endpoint.
    #[must_use]
    pub fn io(&self) -> &PtyIo {
        &self.io
    }

    /// Writes all bytes to the PTY master.
    pub fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        self.io.write_all(bytes)
    }

    #[cfg(unix)]
    pub(crate) fn raw_fd(&self) -> RawFd {
        self.io.raw_fd()
    }

    #[cfg(windows)]
    pub(crate) fn windows_pty(&self) -> Arc<backend::WindowsPty> {
        Arc::clone(&self.io.pty)
    }
}

/// A freshly allocated PTY pair.
#[derive(Debug)]
pub struct PtyPair {
    master: PtyMaster,
    #[cfg(unix)]
    slave: PtySlave,
}

impl PtyPair {
    /// Allocates a PTY pair using the platform backend.
    pub fn open() -> Result<Self> {
        #[cfg(unix)]
        {
            let (master, slave) = backend::open_pty_pair()?;

            Ok(Self {
                master: PtyMaster::new(master),
                slave: PtySlave { fd: slave },
            })
        }

        #[cfg(windows)]
        {
            let master = backend::open_pty_pair(TerminalSize::new(80, 24))?;
            Ok(Self {
                master: PtyMaster::new(master),
            })
        }

        #[cfg(not(unix))]
        #[cfg(not(windows))]
        {
            Err(PtyError::Unsupported("open pty pair"))
        }
    }

    /// Allocates a PTY pair and applies an initial window size.
    pub fn open_with_size(size: TerminalSize) -> Result<Self> {
        let pair = Self::open()?;
        pair.master.resize(size)?;
        Ok(pair)
    }

    /// Returns the master endpoint.
    #[must_use]
    pub fn master(&self) -> &PtyMaster {
        &self.master
    }

    /// Returns the slave endpoint.
    #[cfg(unix)]
    #[must_use]
    pub fn slave(&self) -> &PtySlave {
        &self.slave
    }

    /// Consumes this Unix PTY pair into its master and slave endpoints.
    #[cfg(unix)]
    #[must_use]
    pub fn into_split(self) -> (PtyMaster, PtySlave) {
        (self.master, self.slave)
    }

    /// Consumes the pair and returns the master endpoint.
    #[must_use]
    pub fn into_master(self) -> PtyMaster {
        self.master
    }
}
