use crate::handles::session::unexpected_response;
use crate::transport::TransportClient;
use crate::{PaneRef, Result, TerminalSizeSpec};
use rmux_proto::{
    Request, ResizePaneAdjustment, ResizePaneRequest, Response, SendKeysExtRequest, SendKeysRequest,
};

use super::info::fetch_live_details_or_default;

pub(super) async fn send_text(
    client: &TransportClient,
    target: &PaneRef,
    text: &str,
) -> Result<()> {
    let response = client
        .request(Request::SendKeysExt(SendKeysExtRequest {
            target: Some(target.into()),
            keys: vec![text.to_owned()],
            expand_formats: false,
            hex: false,
            literal: true,
            dispatch_key_table: false,
            copy_mode_command: false,
            forward_mouse_event: false,
            reset_terminal: false,
            repeat_count: None,
        }))
        .await?;

    match response {
        Response::SendKeys(_) => Ok(()),
        response => Err(unexpected_response("send-keys", response)),
    }
}

pub(super) async fn send_key(
    client: &TransportClient,
    target: &PaneRef,
    key: String,
) -> Result<()> {
    let response = client
        .request(Request::SendKeys(SendKeysRequest {
            target: target.into(),
            keys: vec![key],
        }))
        .await?;

    match response {
        Response::SendKeys(_) => Ok(()),
        response => Err(unexpected_response("send-keys", response)),
    }
}

pub(super) async fn resize_to_size(
    client: &TransportClient,
    target: &PaneRef,
    requested: TerminalSizeSpec,
) -> Result<()> {
    let current = live_pane_size(client, target).await?;
    let mut sent_non_noop_adjustment = false;

    if current.cols != requested.cols {
        request_resize_pane(
            client,
            target,
            ResizePaneAdjustment::AbsoluteWidth {
                columns: requested.cols,
            },
        )
        .await?;
        sent_non_noop_adjustment = true;
    }

    if current.rows != requested.rows {
        request_resize_pane(
            client,
            target,
            ResizePaneAdjustment::AbsoluteHeight {
                rows: requested.rows,
            },
        )
        .await?;
        sent_non_noop_adjustment = true;
    }

    if !sent_non_noop_adjustment {
        request_resize_pane(client, target, ResizePaneAdjustment::NoOp).await?;
    }

    Ok(())
}

async fn live_pane_size(client: &TransportClient, target: &PaneRef) -> Result<TerminalSizeSpec> {
    let details = fetch_live_details_or_default(client, target).await?;
    Ok(TerminalSizeSpec::new(details.cols, details.rows))
}

async fn request_resize_pane(
    client: &TransportClient,
    target: &PaneRef,
    adjustment: ResizePaneAdjustment,
) -> Result<()> {
    let response = client
        .request(Request::ResizePane(ResizePaneRequest {
            target: target.into(),
            adjustment,
        }))
        .await?;

    match response {
        Response::ResizePane(_) => Ok(()),
        response => Err(unexpected_response("resize-pane", response)),
    }
}
