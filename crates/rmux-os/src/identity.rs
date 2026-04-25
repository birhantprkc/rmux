//! Process identity helpers.

/// Returns the real user id for the current process.
#[must_use]
pub fn real_user_id() -> u32 {
    rustix::process::getuid().as_raw()
}
