#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::invalid_codeblock_attributes)]
#![forbid(unsafe_code)]

//! Public daemon-backed RMUX SDK scaffolding.
//!
//! v1 introduces a fully daemon-backed public SDK. This crate exposes the
//! compile-time vocabulary, facade handles, session ensure builders, and
//! facade-error skeletons that pin the public SDK boundary.
//!
//! `rmux-sdk` is a public integration peer of `rmux-client` and must not
//! depend on `rmux-client`, `rmux-core`, `rmux-server`, or `rmux-pty` as
//! normal dependencies. The authoritative identity newtypes
//! ([`SessionName`], [`SessionId`], [`WindowId`], [`PaneId`]) live in
//! `rmux-proto` and are re-exported here so SDK users import them through
//! `rmux_sdk` without ever depending on those internal crates.
//!
//! # Inert quickstart
//!
//! The shortest legal SDK program records intent without contacting a
//! daemon. The doctest below executes in `cargo test --workspace --doc`
//! and exercises the same surface as `examples/quickstart.rs`:
//!
//! ```
//! use std::time::Duration;
//!
//! use rmux_sdk::{
//!     EnsureSession, EnsureSessionPolicy, ProcessSpec, Rmux, RmuxEndpoint, SessionName,
//!     TerminalSizeSpec,
//! };
//!
//! let rmux = Rmux::builder()
//!     .default_endpoint()
//!     .default_timeout(Duration::from_secs(5))
//!     .build();
//! assert!(matches!(rmux.endpoint(), RmuxEndpoint::Default));
//! assert_eq!(
//!     rmux.configured_default_timeout(),
//!     Some(Duration::from_secs(5)),
//! );
//!
//! let session = SessionName::new("quickstart").expect("valid session name");
//! let ensure = EnsureSession::named(session)
//!     .policy(EnsureSessionPolicy::CreateOrReuse)
//!     .detached(true)
//!     .size(TerminalSizeSpec::new(120, 32))
//!     .process(ProcessSpec {
//!         command: Some(vec!["bash".to_owned(), "-l".to_owned()]),
//!         environment: None,
//!     });
//! assert_eq!(ensure.configured_policy(), EnsureSessionPolicy::CreateOrReuse);
//! ```
//!
//! See the crate public overview (`crates/rmux-sdk/src/lib.rs`) for the public
//! requirements, examples, errors, platform notes, cargo features, and
//! license posture. The public overview is assigned by
//! `cargo run -p xtask -- feature-inventory` and verified by
//! `cargo run -p xtask -- feature-inventory --check`.

pub mod bootstrap;
pub mod command;
pub mod diagnostics;
pub mod ensure;
pub mod error;
pub mod events;
pub mod extract;
pub mod handles;
pub mod info;
pub mod input;
pub mod snapshot;
pub mod spec;
pub mod types;
pub mod wait;

#[allow(dead_code)]
pub(crate) mod transport;

pub use command::{RmuxCommand, RmuxCommandKind};
pub use diagnostics::{
    command_feature_id, protocol_diagnostic, unsupported_feature_id, Diagnostic,
    DiagnosticSeverity, FEATURE_DAEMON_SHUTDOWN, FEATURE_PROTOCOL_CAPABILITIES,
    FEATURE_PROTOCOL_WIRE_VERSION, FEATURE_TRANSPORT_UNIX_SOCKET, FEATURE_TRANSPORT_WINDOWS_PIPE,
};
pub use ensure::{EnsureSession, EnsureSessionPolicy};
pub use error::{CollectError, Result, RmuxError};
pub use events::{
    PaneCommandStatus, PaneCommandSummary, PaneDisconnectReason, PaneEvent, PaneExitReason,
    PaneLagNotice, PaneLineItem, PaneLineStream, PaneNotification, PaneOutputChunk,
    PaneOutputStart, PaneOutputStream, PanePermissionScope, PaneRecentOutput,
};
pub use extract::{CollectedPaneOutput, PaneTextMatch};
pub use handles::{
    Pane, PaneCloseOutcome, PaneRespawnOptions, Rmux, RmuxBuilder, Session, Window,
    WindowCloseOutcome, WindowPane,
};
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
pub use wait::ArmedWait;
