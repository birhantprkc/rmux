//! Opaque RMUX SDK facade handle.

use std::fmt;
use std::io;
use std::time::Duration;

use super::builder::RmuxBuilder;
use crate::diagnostics::FEATURE_PROTOCOL_CAPABILITIES;
use crate::transport::{DropGuard, TransportClient};
use crate::{
    bootstrap::discovery, ensure::EnsureSession, handles::Session, Pane, PaneRef, Result,
    RmuxEndpoint, RmuxError, SessionName, Window, WindowRef,
};
use rmux_proto::{
    HandshakeRequest, KillServerRequest, Request, Response, CAPABILITY_DAEMON_SHUTDOWN,
    CAPABILITY_HANDSHAKE, RMUX_WIRE_VERSION,
};

#[path = "rmux/connect.rs"]
mod connect;

use connect::connect_transport;
pub(crate) use connect::{connect_or_start_transport, connect_transport_to_endpoint};

/// Inert SDK facade for daemon-backed RMUX operations.
///
/// Constructing this handle only records endpoint configuration and does not
/// contact a daemon.
pub struct Rmux {
    endpoint: RmuxEndpoint,
    default_timeout: Option<Duration>,
    transport: Option<TransportClient>,
    drop_guard: DropGuard,
}

impl Rmux {
    /// Creates a facade configured to use default endpoint discovery.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a builder configured to use default endpoint discovery.
    #[must_use]
    pub fn builder() -> RmuxBuilder {
        RmuxBuilder::new()
    }

    /// Connects to a running daemon at `endpoint`.
    ///
    /// Passing [`RmuxEndpoint::Default`] resolves the platform default through
    /// SDK discovery. This method never starts a daemon.
    pub async fn connect(endpoint: RmuxEndpoint) -> Result<Self> {
        RmuxBuilder::new().endpoint(endpoint).connect().await
    }

    /// Connects to the default daemon, starting it if no daemon is reachable.
    ///
    /// The hidden daemon binary is resolved from
    /// [`crate::bootstrap::discovery::SDK_DAEMON_BINARY_ENV`] when set,
    /// otherwise `rmux` is resolved through the host `PATH`. Startup races are
    /// serialized by the platform bootstrap layer.
    pub async fn connect_or_start() -> Result<Self> {
        RmuxBuilder::new().connect_or_start().await
    }

    /// Connects to `endpoint`, starting a hidden daemon if no daemon is
    /// reachable there.
    ///
    /// This is the explicit-endpoint form of [`Self::connect_or_start`].
    pub async fn connect_or_start_at(endpoint: RmuxEndpoint) -> Result<Self> {
        RmuxBuilder::new()
            .endpoint(endpoint)
            .connect_or_start()
            .await
    }

    /// Returns the endpoint selection recorded by this facade.
    #[must_use]
    pub fn endpoint(&self) -> &RmuxEndpoint {
        &self.endpoint
    }

    /// Returns the operation timeout default recorded by this facade.
    #[must_use]
    pub const fn configured_default_timeout(&self) -> Option<Duration> {
        self.default_timeout
    }

    /// Resolves the endpoint that would be used by runtime SDK operations.
    ///
    /// This consults SDK discovery only when the recorded endpoint is
    /// [`RmuxEndpoint::Default`].
    pub fn resolved_endpoint(&self) -> Result<RmuxEndpoint> {
        discovery::resolve_endpoint(&self.endpoint)
    }

    /// Resolves the timeout that would be used by one runtime SDK operation.
    ///
    /// `per_operation_timeout` has precedence over this facade's configured
    /// default and can use `Duration::MAX` to request no timeout.
    #[must_use]
    pub fn resolved_timeout(&self, per_operation_timeout: Option<Duration>) -> Option<Duration> {
        discovery::resolve_timeout(per_operation_timeout, self.default_timeout)
    }

    /// Ensures a daemon session from a session builder.
    pub async fn ensure_session(&self, ensure: EnsureSession) -> Result<Session> {
        ensure.ensure(self).await
    }

    /// Returns a handle for an existing daemon session.
    pub async fn session(&self, session_name: SessionName) -> Result<Session> {
        self.ensure_session(EnsureSession::named(session_name).reuse_only())
            .await
    }

    /// Returns a daemon-backed handle for an exact window slot.
    ///
    /// Creating the handle connects to the configured daemon endpoint but
    /// does not require the window slot to exist yet. Operations on the
    /// returned handle observe the live daemon state for that session/index,
    /// including linked-window and grouped-session updates.
    pub async fn window(&self, target: WindowRef) -> Result<Window> {
        let endpoint = self.resolved_endpoint()?;
        let timeout = self.resolved_timeout(None);
        let transport = self
            .connect_resolved_transport_for_operation(&endpoint, timeout)
            .await?;
        Ok(Window::new(
            target,
            endpoint,
            self.configured_default_timeout(),
            transport,
        ))
    }

    /// Returns a daemon-backed handle for an exact pane slot.
    ///
    /// Creating the handle connects to the configured daemon endpoint but
    /// does not require the pane slot to exist yet. Operations on the
    /// returned handle resolve `(session, window, pane)` against live daemon
    /// state on every call, so linked windows and grouped sessions report
    /// the same stable pane identity through every sibling view.
    pub async fn pane(&self, target: PaneRef) -> Result<Pane> {
        let endpoint = self.resolved_endpoint()?;
        let timeout = self.resolved_timeout(None);
        let transport = self
            .connect_resolved_transport_for_operation(&endpoint, timeout)
            .await?;
        Ok(Pane::new(
            target,
            endpoint,
            self.configured_default_timeout(),
            transport,
        ))
    }

    /// Checks the live daemon for an exact session name.
    pub async fn has_session(&self, session_name: SessionName) -> Result<bool> {
        let client = self
            .connect_transport_for_operation(self.resolved_timeout(None))
            .await?;
        super::session::has_session(&client, session_name).await
    }

    /// Lists exact session names currently reported by the daemon.
    pub async fn list_sessions(&self) -> Result<Vec<SessionName>> {
        let client = self
            .connect_transport_for_operation(self.resolved_timeout(None))
            .await?;
        super::session::list_session_names(&client).await
    }

    /// Negotiates daemon capabilities, requests daemon shutdown, and waits for
    /// the SDK transport to close.
    ///
    /// This method contacts the configured daemon endpoint. Transport and
    /// protocol errors are returned to the caller; dropping an [`Rmux`] handle
    /// remains cleanup-only and never waits for daemon shutdown.
    pub async fn shutdown(mut self) -> Result<()> {
        self.drop_guard.disarm();
        let client = match self.transport.take() {
            Some(client) => client,
            None => self.connect_transport().await?,
        };

        negotiate_shutdown_capability(&client).await?;
        let response = client
            .request(Request::KillServer(KillServerRequest))
            .await?;
        match response {
            Response::KillServer(_) => {
                if let Err(error) = client.shutdown().await {
                    if !is_clean_shutdown_close(&error) {
                        return Err(error);
                    }
                }
                Ok(())
            }
            Response::Error(error) => Err(error.into()),
            response => Err(RmuxError::protocol(rmux_proto::RmuxError::Server(format!(
                "rmux daemon sent `{}` response for shutdown request",
                response.command_name()
            )))),
        }
    }

    pub(crate) fn from_config(endpoint: RmuxEndpoint, default_timeout: Option<Duration>) -> Self {
        Self {
            endpoint,
            default_timeout,
            transport: None,
            drop_guard: DropGuard::noop(),
        }
    }

    pub(crate) fn from_connected_transport(
        endpoint: RmuxEndpoint,
        default_timeout: Option<Duration>,
        transport: TransportClient,
    ) -> Self {
        Self {
            endpoint,
            default_timeout,
            transport: Some(transport),
            drop_guard: DropGuard::noop(),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_transport_for_test(
        client: TransportClient,
        drop_request: Option<Request>,
    ) -> Self {
        let drop_guard = drop_request
            .map(|request| DropGuard::best_effort(client.clone(), request))
            .unwrap_or_else(DropGuard::noop);
        Self {
            endpoint: RmuxEndpoint::Default,
            default_timeout: None,
            transport: Some(client),
            drop_guard,
        }
    }

    async fn connect_transport(&self) -> Result<TransportClient> {
        let endpoint = self.resolved_endpoint()?;
        connect_transport(&endpoint, self.resolved_timeout(None)).await
    }

    pub(crate) async fn connect_transport_for_operation(
        &self,
        timeout: Option<Duration>,
    ) -> Result<TransportClient> {
        if let Some(client) = self.transport.as_ref() {
            return Ok(client.clone());
        }

        let endpoint = self.resolved_endpoint()?;
        connect_transport(&endpoint, timeout).await
    }

    pub(crate) async fn connect_resolved_transport_for_operation(
        &self,
        endpoint: &RmuxEndpoint,
        timeout: Option<Duration>,
    ) -> Result<TransportClient> {
        if let Some(client) = self.transport.as_ref() {
            return Ok(client.clone());
        }

        connect_transport(endpoint, timeout).await
    }
}

fn is_clean_shutdown_close(error: &RmuxError) -> bool {
    matches!(
        error,
        RmuxError::Transport { source, .. }
            if matches!(
                source.kind(),
                io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::BrokenPipe
            )
    )
}

impl Default for Rmux {
    fn default() -> Self {
        RmuxBuilder::default().build()
    }
}

impl fmt::Debug for Rmux {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Rmux").finish_non_exhaustive()
    }
}

async fn negotiate_shutdown_capability(client: &TransportClient) -> Result<()> {
    let response = client
        .request(Request::Handshake(HandshakeRequest::requiring([
            CAPABILITY_HANDSHAKE,
            CAPABILITY_DAEMON_SHUTDOWN,
        ])))
        .await
        .map_err(normalize_handshake_error)?;

    match response {
        Response::Handshake(response) => {
            ensure_selected_wire_version(response.wire_version)?;
            ensure_capability(&response.capabilities, CAPABILITY_HANDSHAKE)?;
            ensure_capability(&response.capabilities, CAPABILITY_DAEMON_SHUTDOWN)
        }
        Response::Error(error) => Err(error.into()),
        response => Err(RmuxError::protocol(rmux_proto::RmuxError::Server(format!(
            "rmux daemon sent `{}` response for capability handshake",
            response.command_name()
        )))),
    }
}

fn normalize_handshake_error(error: RmuxError) -> RmuxError {
    match error {
        RmuxError::Protocol {
            source: rmux_proto::RmuxError::Decode(message),
        } => unsupported_handshake_error(&message),
        RmuxError::Unsupported { feature, .. }
            if feature == crate::diagnostics::command_feature_id("handshake") =>
        {
            unsupported_handshake_error("daemon did not recognize the handshake request")
        }
        error => error,
    }
}

fn unsupported_handshake_error(detail: &str) -> RmuxError {
    RmuxError::unsupported(
        FEATURE_PROTOCOL_CAPABILITIES,
        format!(
            "upgrade the rmux daemon to one that advertises `{CAPABILITY_HANDSHAKE}` before using SDK daemon shutdown; {detail}"
        ),
    )
}

fn ensure_selected_wire_version(wire_version: u32) -> Result<()> {
    if wire_version == RMUX_WIRE_VERSION {
        return Ok(());
    }

    Err(RmuxError::protocol(
        rmux_proto::RmuxError::UnsupportedWireVersion {
            got: wire_version,
            minimum: RMUX_WIRE_VERSION,
            maximum: RMUX_WIRE_VERSION,
        },
    ))
}

fn ensure_capability(capabilities: &[String], feature: &str) -> Result<()> {
    if capabilities
        .iter()
        .any(|capability| capability.as_str() == feature)
    {
        return Ok(());
    }

    Err(RmuxError::protocol(
        rmux_proto::RmuxError::UnsupportedCapability {
            feature: feature.to_owned(),
            supported: capabilities.to_vec(),
        },
    ))
}

#[cfg(test)]
#[path = "rmux/tests.rs"]
mod tests;
