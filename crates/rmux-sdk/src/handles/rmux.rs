//! Opaque RMUX SDK facade handle.

use std::fmt;

use super::builder::RmuxBuilder;
use crate::transport::DropGuard;
use crate::RmuxEndpoint;

/// Inert SDK facade for daemon-backed RMUX operations.
///
/// Constructing this handle only records endpoint configuration and does not
/// contact a daemon.
pub struct Rmux {
    endpoint: RmuxEndpoint,
    _drop_guard: DropGuard,
}

impl Rmux {
    /// Creates a facade configured to use the platform default endpoint.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a builder configured to use the platform default endpoint.
    #[must_use]
    pub fn builder() -> RmuxBuilder {
        RmuxBuilder::new()
    }

    /// Returns the endpoint selection recorded by this facade.
    #[must_use]
    pub fn endpoint(&self) -> &RmuxEndpoint {
        &self.endpoint
    }

    pub(crate) fn from_endpoint(endpoint: RmuxEndpoint) -> Self {
        Self {
            endpoint,
            _drop_guard: DropGuard::noop(),
        }
    }
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
