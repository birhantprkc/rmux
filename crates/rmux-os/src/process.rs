//! Process inspection helpers.

#[cfg(target_os = "macos")]
use std::ffi::CStr;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};

use rustix::termios::tcgetpgrp;

/// Returns the foreground process id for a terminal file descriptor.
#[must_use]
pub fn foreground_pid(fd: BorrowedFd<'_>) -> Option<u32> {
    let pgrp = tcgetpgrp(fd).ok()?;
    u32::try_from(pgrp.as_raw_nonzero().get()).ok()
}

/// Returns the current working directory for `pid`, when the platform exposes it.
#[must_use]
pub fn current_path(pid: u32) -> Option<String> {
    current_path_impl(pid)
}

/// Returns the executable command name for `pid`, when available.
#[must_use]
pub fn command_name(pid: u32) -> Option<String> {
    command_name_impl(pid)
}

/// Returns the path for a process file descriptor, when the platform exposes it.
#[must_use]
pub fn fd_path(pid: u32, fd: i32) -> Option<PathBuf> {
    if fd < 0 {
        return None;
    }
    fd_path_impl(pid, fd)
}

#[cfg(target_os = "linux")]
fn current_path_impl(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

#[cfg(target_os = "linux")]
fn command_name_impl(pid: u32) -> Option<String> {
    command_name_from_linux_cmdline(pid).or_else(|| command_name_from_linux_comm(pid))
}

#[cfg(target_os = "linux")]
fn command_name_from_linux_cmdline(pid: u32) -> Option<String> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let first = cmdline
        .split(|byte| *byte == 0)
        .find(|segment| !segment.is_empty())?;
    executable_name(std::str::from_utf8(first).ok()?)
}

#[cfg(target_os = "linux")]
fn command_name_from_linux_comm(pid: u32) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    executable_name(comm.trim())
}

#[cfg(target_os = "linux")]
fn fd_path_impl(pid: u32, fd: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).ok()
}

#[cfg(target_os = "macos")]
fn current_path_impl(pid: u32) -> Option<String> {
    let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>();
    let read = unsafe {
        // SAFETY: `info` points to writable memory sized for the requested flavor.
        libc::proc_pidinfo(
            pid.try_into().ok()?,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size.try_into().ok()?,
        )
    };
    if usize::try_from(read).ok()? < size {
        return None;
    }

    let info = unsafe {
        // SAFETY: `proc_pidinfo` reported that it initialized the full structure.
        info.assume_init()
    };
    string_from_c_chars(info.pvi_cdir.vip_path.as_ptr().cast())
}

#[cfg(target_os = "macos")]
fn command_name_impl(pid: u32) -> Option<String> {
    command_name_from_macos_pidpath(pid).or_else(|| command_name_from_macos_proc_name(pid))
}

#[cfg(target_os = "macos")]
fn command_name_from_macos_pidpath(pid: u32) -> Option<String> {
    let mut buffer = [0 as libc::c_char; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let written = unsafe {
        // SAFETY: `buffer` is writable for the size passed to `proc_pidpath`.
        libc::proc_pidpath(
            pid.try_into().ok()?,
            buffer.as_mut_ptr().cast(),
            buffer.len().try_into().ok()?,
        )
    };
    if written <= 0 {
        return None;
    }
    executable_name(&string_from_c_chars(buffer.as_ptr())?)
}

#[cfg(target_os = "macos")]
fn command_name_from_macos_proc_name(pid: u32) -> Option<String> {
    let mut buffer = [0 as libc::c_char; 1024];
    let written = unsafe {
        // SAFETY: `buffer` is writable for the size passed to `proc_name`.
        libc::proc_name(
            pid.try_into().ok()?,
            buffer.as_mut_ptr().cast(),
            buffer.len().try_into().ok()?,
        )
    };
    if written <= 0 {
        return None;
    }
    string_from_c_chars(buffer.as_ptr()).and_then(|name| executable_name(&name))
}

#[cfg(target_os = "macos")]
fn fd_path_impl(pid: u32, fd: i32) -> Option<PathBuf> {
    let mut info = std::mem::MaybeUninit::<MacosVnodeFdInfoWithPath>::zeroed();
    let size = std::mem::size_of::<MacosVnodeFdInfoWithPath>();
    let read = unsafe {
        // SAFETY: `info` points to writable memory sized for the requested flavor.
        libc::proc_pidfdinfo(
            pid.try_into().ok()?,
            fd,
            MACOS_PROC_PIDFDVNODEPATHINFO,
            info.as_mut_ptr().cast(),
            size.try_into().ok()?,
        )
    };
    if usize::try_from(read).ok()? < size {
        return None;
    }

    let info = unsafe {
        // SAFETY: `proc_pidfdinfo` reported that it initialized the full structure.
        info.assume_init()
    };
    string_from_c_chars(info.pvip.vip_path.as_ptr().cast()).map(PathBuf::from)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn current_path_impl(_pid: u32) -> Option<String> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn command_name_impl(_pid: u32) -> Option<String> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn fd_path_impl(_pid: u32, _fd: i32) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "macos")]
const MACOS_PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

#[cfg(target_os = "macos")]
#[repr(C)]
struct MacosProcFileInfo {
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: libc::off_t,
    fi_type: i32,
    fi_guardflags: u32,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct MacosVnodeFdInfoWithPath {
    pfi: MacosProcFileInfo,
    pvip: libc::vnode_info_path,
}

#[cfg(target_os = "macos")]
fn string_from_c_chars(chars: *const libc::c_char) -> Option<String> {
    let value = unsafe {
        // SAFETY: macOS libproc path/name buffers are nul-terminated on success.
        CStr::from_ptr(chars)
    }
    .to_string_lossy()
    .into_owned();
    (!value.is_empty()).then_some(value)
}

fn executable_name(path: &str) -> Option<String> {
    let name = Path::new(path).file_name()?.to_string_lossy();
    let trimmed = name.trim_start_matches('-');
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}
