#![deny(missing_docs)]
#![forbid(unsafe_code)]

//! Compile-only ratatui integration crate scaffolding for RMUX v1.
//!
//! RMUX behavior for this satellite crate is intentionally routed through
//! `rmux-sdk`. This scaffold exposes no rendering or driver default_value API
//! before the SDK surface and budgets are recorded.

mod scaffold;
