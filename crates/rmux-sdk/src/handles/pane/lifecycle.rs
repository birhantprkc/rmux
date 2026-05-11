use crate::handles::session::unexpected_response;
use crate::transport::TransportClient;
use crate::{PaneCloseOutcome, PaneRef, PaneRespawnOptions, Result};
use rmux_proto::{KillPaneRequest, Request, RespawnPaneRequest, Response};

use super::target::is_already_closed_pane_error;

pub(super) async fn close_pane(
    client: &TransportClient,
    target: PaneRef,
) -> Result<PaneCloseOutcome> {
    let response = client
        .request(Request::KillPane(KillPaneRequest {
            target: (&target).into(),
            kill_all_except: false,
        }))
        .await;

    match response {
        Ok(Response::KillPane(response)) => Ok(PaneCloseOutcome::Closed {
            target: response.target.into(),
            window_destroyed: response.window_destroyed,
        }),
        Ok(response) => Err(unexpected_response("kill-pane", response)),
        Err(error) if is_already_closed_pane_error(&error, &target) => {
            Ok(PaneCloseOutcome::AlreadyClosed { target })
        }
        Err(error) => Err(error),
    }
}

pub(super) async fn respawn_pane(
    client: &TransportClient,
    target: &PaneRef,
    options: PaneRespawnOptions,
) -> Result<PaneRef> {
    let response = client
        .request(Request::RespawnPane(RespawnPaneRequest {
            target: target.into(),
            kill: options.kill,
            start_directory: options.start_directory,
            environment: options.process.environment,
            command: options.process.command,
        }))
        .await?;

    match response {
        Response::RespawnPane(response) => Ok(response.target.into()),
        response => Err(unexpected_response("respawn-pane", response)),
    }
}
