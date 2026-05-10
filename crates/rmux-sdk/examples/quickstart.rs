//! Quickstart example: configure the inert [`Rmux`] facade builder and
//! describe a session through the SDK's public DTOs.
//!
//! This example is compile-tested by `cargo build --workspace --examples`
//! and `cargo clippy --workspace --all-targets --locked`. It documents
//! what the public surface looks like before any daemon is contacted; the
//! main function never opens a socket or named pipe.
//!
//! The example uses only types re-exported from `rmux_sdk` and does not
//! depend on `rmux-client`, `rmux-core`, `rmux-server`, or `rmux-pty`.

use std::time::Duration;

use rmux_sdk::{
    EnsureSession, EnsureSessionPolicy, ProcessSpec, Rmux, RmuxEndpoint, SessionName,
    TerminalSizeSpec,
};

fn main() {
    // The builder records intent only — it does not resolve the endpoint
    // and does not connect. `default_endpoint()` keeps the SDK's discovery
    // path in charge of resolving the platform IPC socket or named pipe
    // at the first operation.
    let rmux = Rmux::builder()
        .default_endpoint()
        .default_timeout(Duration::from_secs(5))
        .build();

    assert!(matches!(rmux.endpoint(), RmuxEndpoint::Default));
    assert_eq!(
        rmux.configured_default_timeout(),
        Some(Duration::from_secs(5))
    );

    // Describe the session we would ask for if we connected. The builder
    // stays inert; nothing runs until a caller awaits `ensure(&rmux)`.
    let session_name = SessionName::new("quickstart").expect("valid session name");
    let ensure = EnsureSession::named(session_name.clone())
        .policy(EnsureSessionPolicy::CreateOrReuse)
        .detached(true)
        .size(TerminalSizeSpec::new(120, 32))
        .process(ProcessSpec {
            command: Some(vec!["bash".to_owned(), "-l".to_owned()]),
            environment: None,
        })
        .working_directory("/tmp")
        .window_name("main");

    assert_eq!(ensure.configured_session_name(), Some(&session_name));
    assert_eq!(
        ensure.configured_policy(),
        EnsureSessionPolicy::CreateOrReuse
    );
    assert_eq!(ensure.resolved_timeout(&rmux), Some(Duration::from_secs(5)));

    println!(
        "quickstart configured: session={}, endpoint={:?}",
        session_name,
        rmux.endpoint(),
    );
}
