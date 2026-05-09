#![deny(missing_docs)]
#![forbid(unsafe_code)]

//! Public daemon-backed RMUX SDK scaffolding.
//!
//! v1 introduces a fully daemon-backed public SDK. This crate currently
//! exposes the compile-time vocabulary, inert facade handles, and facade-error
//! skeletons that pin the public SDK boundary.
//!
//! `rmux-sdk` is a public integration peer of `rmux-client` and must not
//! depend on `rmux-client`, `rmux-core`, `rmux-server`, or `rmux-pty` as
//! normal dependencies. The authoritative identity newtypes
//! (`SessionName`, `SessionId`, `WindowId`, `PaneId`) live in
//! `rmux-proto` and are re-exported here so SDK users import them through
//! `rmux_sdk` without ever depending on those internal crates.

pub mod command;
pub mod error;
pub mod events;
pub mod handles;
pub mod info;
pub mod input;
pub mod snapshot;
pub mod spec;
pub mod types;

#[allow(dead_code)]
pub(crate) mod transport;

pub use command::{RmuxCommand, RmuxCommandKind};
pub use error::{CollectError, Result, RmuxError};
pub use events::{
    PaneCommandStatus, PaneCommandSummary, PaneDisconnectReason, PaneEvent, PaneExitReason,
    PaneNotification, PanePermissionScope,
};
pub use handles::{Rmux, RmuxBuilder};
pub use info::{InfoSnapshot, PaneExitState, PaneInfo, PaneProcessState, SessionInfo, WindowInfo};
pub use input::{
    DetachChord, DetachDetector, DetachOutcome, KeyCode, KeyConversionError, KeyEvent, KeyModifiers,
};
pub use snapshot::{
    PaneAttributes, PaneCell, PaneColor, PaneCursor, PaneGlyph, PaneSnapshot,
    PaneSnapshotShapeError,
};
pub use spec::{
    AttachSessionReuse, AttachSessionSpec, ClientTerminalSpec, NewSessionReuse, NewSessionSpec,
    ProcessSpec, RefreshClientSpec, SplitDirectionSpec, SplitSpec, SplitTargetSpec,
    SubscriptionSpec,
};
pub use types::{
    PaneId, PaneRef, RmuxEndpoint, SessionId, SessionName, TargetRef, TerminalSizeSpec, WindowId,
    WindowRef,
};
