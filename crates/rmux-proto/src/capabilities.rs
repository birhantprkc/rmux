//! Detached RPC capability handshake DTOs.

use serde::{Deserialize, Serialize};

use crate::{RmuxError, RMUX_WIRE_VERSION};

/// Stable feature id for the detached bincode RPC transport.
pub const CAPABILITY_DETACHED_RPC: &str = "rpc.detached";
/// Stable feature id for the capabilities handshake request.
pub const CAPABILITY_HANDSHAKE: &str = "protocol.capabilities";
/// Stable feature id for framed protocol errors returned as `Response::Error`.
pub const CAPABILITY_FRAMED_ERRORS: &str = "protocol.framed_errors";
/// Stable feature id for `attach-session` framed-to-raw stream upgrades.
pub const CAPABILITY_ATTACH_STREAM: &str = "stream.attach";
/// Stable feature id for control-mode framed-to-raw stream upgrades.
pub const CAPABILITY_CONTROL_STREAM: &str = "stream.control";
/// Stable feature id for daemon shutdown over detached RPC.
pub const CAPABILITY_DAEMON_SHUTDOWN: &str = "daemon.shutdown";
/// Stable feature id for daemon-backed SDK waits and cancellation.
pub const CAPABILITY_SDK_WAITS: &str = "sdk.waits";

/// Capabilities advertised by this protocol build.
pub const SUPPORTED_CAPABILITIES: &[&str] = &[
    CAPABILITY_DETACHED_RPC,
    CAPABILITY_HANDSHAKE,
    CAPABILITY_FRAMED_ERRORS,
    CAPABILITY_ATTACH_STREAM,
    CAPABILITY_CONTROL_STREAM,
    CAPABILITY_DAEMON_SHUTDOWN,
    CAPABILITY_SDK_WAITS,
];

/// Client-to-server version and capability negotiation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeRequest {
    /// Lowest detached RPC wire version accepted by the caller.
    pub minimum_wire_version: u32,
    /// Highest detached RPC wire version accepted by the caller.
    pub maximum_wire_version: u32,
    /// Capability ids the caller requires before issuing follow-up requests.
    pub required_capabilities: Vec<String>,
}

impl HandshakeRequest {
    /// Builds a current-version handshake with no mandatory capabilities.
    #[must_use]
    pub fn current() -> Self {
        Self::requiring(std::iter::empty::<&str>())
    }

    /// Builds a current-version handshake with explicit mandatory capabilities.
    #[must_use]
    pub fn requiring<I, S>(required_capabilities: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            minimum_wire_version: RMUX_WIRE_VERSION,
            maximum_wire_version: RMUX_WIRE_VERSION,
            required_capabilities: required_capabilities
                .into_iter()
                .map(|capability| capability.as_ref().to_owned())
                .collect(),
        }
    }

    /// Validates this request against the supplied capability list.
    pub fn validate_against(&self, supported_capabilities: &[&str]) -> Result<(), RmuxError> {
        if self.minimum_wire_version > RMUX_WIRE_VERSION
            || self.maximum_wire_version < RMUX_WIRE_VERSION
        {
            return Err(RmuxError::UnsupportedWireVersion {
                got: RMUX_WIRE_VERSION,
                minimum: self.minimum_wire_version,
                maximum: self.maximum_wire_version,
            });
        }

        if let Some(feature) = self
            .required_capabilities
            .iter()
            .find(|feature| !supported_capabilities.contains(&feature.as_str()))
        {
            return Err(RmuxError::UnsupportedCapability {
                feature: feature.clone(),
                supported: supported_capabilities
                    .iter()
                    .copied()
                    .map(str::to_owned)
                    .collect(),
            });
        }

        Ok(())
    }
}

/// Server-to-client version and capability negotiation response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeResponse {
    /// Detached RPC wire version selected for this connection.
    pub wire_version: u32,
    /// Capability ids supported by the daemon.
    pub capabilities: Vec<String>,
}

impl HandshakeResponse {
    /// Builds the response advertised by this protocol build.
    #[must_use]
    pub fn current() -> Self {
        Self {
            wire_version: RMUX_WIRE_VERSION,
            capabilities: SUPPORTED_CAPABILITIES
                .iter()
                .copied()
                .map(str::to_owned)
                .collect(),
        }
    }
}
