//! Hidden-daemon process launch policy.
//!
//! This module is the single OS boundary for launching the detached RMUX
//! daemon. CLI and SDK call sites should use these helpers instead of copying
//! platform flags or Unix session setup locally.

use std::io;
use std::process::Command;

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
    CREATE_UNICODE_ENVIRONMENT, DETACHED_PROCESS,
};

/// Configures `command` so the spawned RMUX daemon is not tied to the client
/// process' controlling terminal, console, or job object when the platform
/// supports that separation.
///
/// On Windows, `allow_job_breakaway` controls whether
/// `CREATE_BREAKAWAY_FROM_JOB` is included. On Unix it is ignored because a
/// fresh session is created in the child just before `exec`.
pub fn configure_hidden_daemon_command(command: &mut Command, allow_job_breakaway: bool) {
    configure_hidden_daemon_command_impl(command, allow_job_breakaway);
}

/// Returns whether a hidden-daemon spawn error should be retried without the
/// Windows job breakaway flag.
#[must_use]
pub fn should_retry_hidden_daemon_without_breakaway(error: &io::Error) -> bool {
    should_retry_hidden_daemon_without_breakaway_impl(error)
}

#[cfg(unix)]
fn configure_hidden_daemon_command_impl(command: &mut Command, _allow_job_breakaway: bool) {
    // SAFETY: The closure runs after fork and before exec in the daemon child.
    // It only calls `setsid`, an async-signal-safe libc/rustix operation, and
    // does not touch parent-owned Rust state.
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid().map_err(io::Error::from)?;
            Ok(())
        });
    }
}

#[cfg(windows)]
fn configure_hidden_daemon_command_impl(command: &mut Command, allow_job_breakaway: bool) {
    command.creation_flags(hidden_daemon_creation_flags(allow_job_breakaway));
}

#[cfg(not(any(unix, windows)))]
fn configure_hidden_daemon_command_impl(_command: &mut Command, _allow_job_breakaway: bool) {}

#[cfg(windows)]
fn should_retry_hidden_daemon_without_breakaway_impl(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == ERROR_ACCESS_DENIED as i32 || code == ERROR_INVALID_PARAMETER as i32
    )
}

#[cfg(not(windows))]
fn should_retry_hidden_daemon_without_breakaway_impl(_error: &io::Error) -> bool {
    false
}

/// Returns the Win32 creation flags used for hidden daemon children.
#[cfg(windows)]
#[must_use]
pub const fn hidden_daemon_creation_flags(allow_job_breakaway: bool) -> u32 {
    let base =
        DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT;
    if allow_job_breakaway {
        base | CREATE_BREAKAWAY_FROM_JOB
    } else {
        base
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn hidden_daemon_flags_detach_console_and_preserve_unicode_env() {
        let flags = hidden_daemon_creation_flags(true);

        assert_ne!(flags & DETACHED_PROCESS, 0);
        assert_ne!(flags & CREATE_NO_WINDOW, 0);
        assert_ne!(flags & CREATE_NEW_PROCESS_GROUP, 0);
        assert_ne!(flags & CREATE_UNICODE_ENVIRONMENT, 0);
        assert_ne!(flags & CREATE_BREAKAWAY_FROM_JOB, 0);

        let fallback_flags = hidden_daemon_creation_flags(false);
        assert_ne!(fallback_flags & DETACHED_PROCESS, 0);
        assert_ne!(fallback_flags & CREATE_NO_WINDOW, 0);
        assert_eq!(fallback_flags & CREATE_BREAKAWAY_FROM_JOB, 0);
    }

    #[test]
    fn hidden_daemon_retry_is_limited_to_breakaway_failures() {
        assert!(should_retry_hidden_daemon_without_breakaway(
            &io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32)
        ));
        assert!(should_retry_hidden_daemon_without_breakaway(
            &io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER as i32)
        ));
        assert!(!should_retry_hidden_daemon_without_breakaway(
            &io::Error::from_raw_os_error(2)
        ));
    }
}
