//! Opaque SDK facade and daemon-backed session handles.
//!
//! Constructing facade builders records caller intent without resolving
//! endpoints. Session handles are returned only after a daemon operation has
//! created or reused a live session.

mod builder;
mod pane;
mod rmux;
pub(crate) mod session;
mod window;

pub use builder::RmuxBuilder;
pub(crate) use pane::is_already_closed_pane_error;
pub use pane::{Pane, PaneCloseOutcome, PaneRespawnOptions};
pub(crate) use rmux::connect_transport_to_endpoint;
pub use rmux::Rmux;
pub use session::Session;
pub use window::{Window, WindowCloseOutcome, WindowPane};

fn assert_static_facade_contract<T: Send + Sync + 'static>() {
    let _ = std::marker::PhantomData::<T>;
}

const _: fn() = assert_static_facade_contract::<Rmux>;
const _: fn() = assert_static_facade_contract::<RmuxBuilder>;
const _: fn() = assert_static_facade_contract::<Session>;
const _: fn() = assert_static_facade_contract::<Window>;
const _: fn() = assert_static_facade_contract::<Pane>;
