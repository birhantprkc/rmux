//! Runtime bootstrap helpers for SDK daemon discovery.

pub mod discovery;
#[cfg(unix)]
pub mod startup_unix;
#[cfg(windows)]
pub mod startup_windows;
