//! Implementation of [`Pane::split`].
//!
//! Kept in its own partial so `pane.rs` stays close to its public surface
//! while the wire-level RPC details — request shape, response decoding,
//! error mapping — live next to the other lifecycle helpers.

use crate::handles::session::unexpected_response;
use crate::handles::split::SplitDirection;
use crate::transport::TransportClient;
use crate::{PaneRef, Result};
use rmux_proto::{Request, Response, SplitWindowExtRequest, SplitWindowTarget};

/// Issues the `split-window` request that backs [`Pane::split`].
///
/// The returned [`PaneRef`] addresses the freshly spawned pane.
pub(super) async fn split_pane(
    client: &TransportClient,
    target: &PaneRef,
    direction: SplitDirection,
) -> Result<PaneRef> {
    match client
        .request(Request::SplitWindowExt(SplitWindowExtRequest {
            target: SplitWindowTarget::Pane(target.into()),
            direction: direction.axis(),
            before: direction.before(),
            environment: None,
            command: None,
        }))
        .await?
    {
        Response::SplitWindow(response) => Ok(response.pane.into()),
        response => Err(unexpected_response("split-window", response)),
    }
}
