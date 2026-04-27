#[cfg(unix)]
use std::os::fd::BorrowedFd;
#[cfg(unix)]
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use rmux_core::PaneGeometry;
use rmux_proto::RmuxError;
use rmux_pty::{PtyChild, PtyMaster, Signal, TerminalSize as PtyTerminalSize};

use crate::terminal::{spawn_pane_process, TerminalProfile};

#[derive(Debug)]
pub(crate) struct PaneTerminal {
    master: PtyMaster,
    child: PtyChild,
    exit_status: Option<ExitStatus>,
    runtime_window_name: Option<String>,
    #[cfg_attr(not(test), allow(dead_code))]
    profile: TerminalProfile,
}

impl PaneTerminal {
    pub(crate) fn new(
        master: PtyMaster,
        child: PtyChild,
        runtime_window_name: Option<String>,
        profile: TerminalProfile,
    ) -> Self {
        Self {
            master,
            child,
            exit_status: None,
            runtime_window_name,
            profile,
        }
    }

    pub(crate) fn resize(&self, size: PtyTerminalSize) -> rmux_pty::Result<()> {
        self.master.resize(size)
    }

    #[cfg(unix)]
    pub(crate) fn master_fd(&self) -> BorrowedFd<'_> {
        self.master.io().as_fd()
    }

    pub(crate) fn clone_master(&self) -> rmux_pty::Result<PtyMaster> {
        self.master.try_clone()
    }

    #[cfg(windows)]
    pub(crate) fn clone_child_for_wait(&self) -> rmux_pty::Result<PtyChild> {
        self.child.try_clone_for_wait()
    }

    pub(crate) fn pid(&self) -> u32 {
        self.child.pid().as_u32()
    }

    #[cfg(unix)]
    pub(crate) fn tty_path(&self) -> Option<PathBuf> {
        rmux_os::process::fd_path(self.pid(), 0)
    }

    pub(crate) fn is_alive(&mut self) -> rmux_pty::Result<bool> {
        if self.exit_status.is_some() {
            return Ok(false);
        }

        match self.child.try_wait()? {
            Some(status) => {
                self.exit_status = Some(status);
                Ok(false)
            }
            None => Ok(true),
        }
    }

    pub(crate) fn exit_status(&mut self) -> rmux_pty::Result<Option<ExitStatus>> {
        let _ = self.is_alive()?;
        Ok(self.exit_status)
    }

    pub(crate) fn profile(&self) -> &TerminalProfile {
        &self.profile
    }

    pub(crate) fn runtime_window_name(&self) -> Option<&str> {
        self.runtime_window_name.as_deref()
    }
}

impl Drop for PaneTerminal {
    fn drop(&mut self) {
        let _ = self.child.kill(Signal::HUP);
        let _ = self.child.kill_session_leader(Signal::HUP);
        for _ in 0..10 {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => return,
            }
        }
        let _ = self.child.kill(Signal::KILL);
        let _ = self.child.kill_session_leader(Signal::KILL);
        for _ in 0..50 {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

pub(crate) fn open_pane_terminal(
    geometry: PaneGeometry,
    profile: TerminalProfile,
    runtime_window_name: Option<String>,
    command: Option<&[String]>,
) -> Result<PaneTerminal, RmuxError> {
    let (master, child) = spawn_pane_process(pty_size_from_geometry(geometry), &profile, command)?;
    Ok(PaneTerminal::new(
        master,
        child,
        runtime_window_name,
        profile,
    ))
}

pub(crate) fn pty_size_from_geometry(geometry: PaneGeometry) -> PtyTerminalSize {
    PtyTerminalSize::new(geometry.cols(), geometry.rows())
}
