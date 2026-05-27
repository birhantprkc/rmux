use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rmux_core::events::OutputCursorItem;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

mod http;
mod pre_auth;
mod rate_limit;

use super::outbound::{OutboundQueueResult, WebSocketOutbound};
use super::protocol::{
    close_for_auth_error, handle_pane_client_text, handle_pane_operator_binary_frame,
    handle_session_client_text, handle_session_operator_binary_frame, queue_output, queue_snapshot,
    read_auth_message, read_client_hello, send_challenge, send_ready, send_revoked,
    send_viewer_count, PaneOperatorAction, PRE_AUTH_TIMEOUT, UNIFORM_AUTH_DELAY,
};
use super::websocket::{valid_client_key, WebSocket, WebSocketMessage};
use super::{crypto, crypto::EncryptedWebSocketReader};
use super::{WebShareConnectionCounts, WebShareRevokeReason};
use crate::handler::{RequestHandler, WebPaneStream, WebSessionStream, WebShareStream};
use http::{read_http_request, write_response, HttpRequest};
use pre_auth::{PreAuthGuard, PreAuthQueue};
use rate_limit::OperatorRateLimiter;

const PRE_AUTH_SLOTS: usize = 16;
const WEB_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const SLOW_VIEWER_CLOSE_CODE: u16 = 4001;

pub(crate) async fn spawn(handler: Arc<RequestHandler>) -> io::Result<()> {
    let listener_config = handler.web_listener();
    let bind_addr = format!("{}:{}", listener_config.host, listener_config.port);
    let listener = match TcpListener::bind(&bind_addr).await {
        Ok(listener) => listener,
        Err(error) => {
            handler.mark_web_listener_unavailable(error.to_string());
            warn!("web-share listener unavailable: {error}");
            return Err(error);
        }
    };
    handler.mark_web_listener_available();
    let task_handler = Arc::clone(&handler);
    tokio::spawn(async move {
        if let Err(error) = serve(handler, listener, bind_addr).await {
            task_handler.mark_web_listener_unavailable(error.to_string());
            warn!("web-share listener stopped: {error}");
        }
    });
    Ok(())
}

async fn serve(
    handler: Arc<RequestHandler>,
    listener: TcpListener,
    bind_addr: String,
) -> io::Result<()> {
    let pre_auth = PreAuthQueue::new(PRE_AUTH_SLOTS);
    debug!("web-share listener bound to {bind_addr}");
    loop {
        let (stream, _) = listener.accept().await?;
        let handler = Arc::clone(&handler);
        let pre_auth = pre_auth.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_connection(stream, handler, pre_auth).await {
                debug!("web-share connection ended: {error}");
            }
        });
    }
}

async fn serve_connection(
    mut stream: TcpStream,
    handler: Arc<RequestHandler>,
    pre_auth: PreAuthQueue,
) -> io::Result<()> {
    let mut pre_auth_guard = pre_auth.register();
    let request = match tokio::select! {
        result = timeout(PRE_AUTH_TIMEOUT, read_http_request(&mut stream)) => result,
        _ = pre_auth_guard.evicted() => return Ok(()),
    } {
        Ok(Ok(request)) => request,
        Ok(Err(error)) if error.kind() == io::ErrorKind::InvalidData => {
            return write_response(
                &mut stream,
                431,
                "text/plain; charset=utf-8",
                b"request headers too large or invalid\n",
            )
            .await;
        }
        Ok(Err(error)) => return Err(error),
        Err(_) => return Ok(()),
    };
    if request.method != "GET" && request.method != "HEAD" {
        return write_response(
            &mut stream,
            405,
            "text/plain; charset=utf-8",
            b"unsupported method\n",
        )
        .await;
    }
    if request.method == "GET" && request.path == "/share" && request.is_websocket_upgrade() {
        return serve_websocket(stream, request, handler, pre_auth_guard).await;
    }
    write_response(
        &mut stream,
        404,
        "text/plain; charset=utf-8",
        b"not found\n",
    )
    .await
}

async fn serve_websocket(
    mut stream: TcpStream,
    request: HttpRequest,
    handler: Arc<RequestHandler>,
    mut pre_auth_guard: PreAuthGuard,
) -> io::Result<()> {
    let Some(key) = request.headers.get("sec-websocket-key") else {
        return write_response(
            &mut stream,
            400,
            "text/plain; charset=utf-8",
            b"missing websocket key\n",
        )
        .await;
    };
    if request
        .headers
        .get("sec-websocket-version")
        .is_none_or(|version| version.trim() != "13")
    {
        return write_response(
            &mut stream,
            400,
            "text/plain; charset=utf-8",
            b"unsupported websocket version\n",
        )
        .await;
    }
    if !valid_client_key(key) {
        return write_response(
            &mut stream,
            400,
            "text/plain; charset=utf-8",
            b"invalid websocket key\n",
        )
        .await;
    }
    let mut socket = WebSocket::accept(stream, key).await?;
    let Some(origin) = request.headers.get("origin") else {
        sleep(UNIFORM_AUTH_DELAY).await;
        let _ = socket.write_close_code(4004, "origin_required").await;
        return Ok(());
    };
    let hello = match tokio::select! {
        result = read_client_hello(&mut socket) => result,
        _ = pre_auth_guard.evicted() => return Ok(()),
    } {
        Ok(hello) => hello,
        Err((code, reason)) => {
            sleep(UNIFORM_AUTH_DELAY).await;
            let _ = socket.write_close_code(code, reason).await;
            return Ok(());
        }
    };
    if handler
        .known_web_share_token_id_origin_allowed(&hello.token_id, origin)
        .is_some_and(|allowed| !allowed)
    {
        sleep(UNIFORM_AUTH_DELAY).await;
        let _ = socket.write_close_code(4004, "origin_not_allowed").await;
        return Ok(());
    }
    let Some(secret) = handler.web_share_token_secret(&hello.token_id) else {
        sleep(UNIFORM_AUTH_DELAY).await;
        let _ = socket.write_close_code(4000, "invalid_auth").await;
        return Ok(());
    };
    let server_nonce = crypto::random_handshake_nonce()?;
    let (mut opener, sealer) =
        crypto::derive_server_crypto(secret, &hello.token_id, &hello.client_nonce, &server_nonce)?;
    write_with_timeout(send_challenge(&mut socket, &server_nonce)).await?;
    let auth = match tokio::select! {
        result = read_auth_message(&mut socket, &mut opener) => result,
        _ = pre_auth_guard.evicted() => return Ok(()),
    } {
        Ok(auth) => auth,
        Err((code, reason)) => {
            sleep(UNIFORM_AUTH_DELAY).await;
            let _ = socket.write_close_code(code, reason).await;
            return Ok(());
        }
    };
    drop(pre_auth_guard);
    let share = match handler
        .open_web_share_token_id(&hello.token_id, auth.pin.as_deref())
        .await
    {
        Ok(pane) => pane,
        Err(error) => {
            sleep(UNIFORM_AUTH_DELAY).await;
            let (code, reason) = close_for_auth_error(&error.to_string());
            info!(close_code = code, reason, "web_share_auth_failed");
            let _ = socket.write_close_code(code, reason).await;
            return Ok(());
        }
    };
    let share_id = share.share_id().to_owned();
    if !share.origin_allowed(origin) {
        sleep(UNIFORM_AUTH_DELAY).await;
        let _ = socket.write_close_code(4004, "origin_not_allowed").await;
        return Ok(());
    }
    sleep(UNIFORM_AUTH_DELAY).await;
    info!(
        share_id = %share_id,
        role = share.role(),
        "web_share_auth_ok"
    );
    let (reader, writer) = socket.split();
    let socket = EncryptedWebSocketReader::new(reader, opener);
    let outbound = WebSocketOutbound::spawn(writer, sealer);
    write_with_timeout(send_ready(&outbound, &share)).await?;
    match share {
        WebShareStream::Pane(pane) => {
            serve_pane_loop(handler, socket, outbound, share_id, *pane).await
        }
        WebShareStream::Session(session) => {
            serve_session_loop(handler, socket, outbound, share_id, *session).await
        }
    }
}

async fn serve_pane_loop(
    handler: Arc<RequestHandler>,
    mut socket: EncryptedWebSocketReader,
    outbound: WebSocketOutbound,
    share_id: String,
    mut pane: WebPaneStream,
) -> io::Result<()> {
    queue_or_close(
        &outbound,
        queue_snapshot(&outbound, &pane.snapshot),
        &share_id,
    )
    .await?;
    let mut rate_limiter = OperatorRateLimiter::new();
    let mut last_connection_counts = pane.connection_counts();
    let mut alive_tick = tokio::time::interval(Duration::from_millis(500));
    let ttl_delay = pane
        .expires_at()
        .map(duration_until)
        .unwrap_or_else(|| Duration::from_secs(365 * 24 * 60 * 60));
    let ttl_sleep = sleep(ttl_delay);
    tokio::pin!(ttl_sleep);

    loop {
        tokio::select! {
            item = pane.output.recv() => {
                match item {
                    OutputCursorItem::Event(event) => {
                        match queue_output(&outbound, event.bytes()) {
                            OutboundQueueResult::Queued => {}
                            OutboundQueueResult::Backpressure => {
                                debug!(share_id = %share_id, "web-share viewer backlog exceeded; resyncing");
                                queue_fresh_pane_snapshot(handler.as_ref(), &outbound, &mut pane, &share_id).await?;
                            }
                            result => {
                                close_slow_viewer(&outbound, &share_id, result).await?;
                                return Ok(());
                            }
                        }
                    }
                    OutputCursorItem::Gap(gap) => {
                        debug!(missed = gap.missed_events(), "web-share read resync");
                        queue_fresh_pane_snapshot(handler.as_ref(), &outbound, &mut pane, &share_id).await?;
                    }
                }
            }
            message = socket.read_message() => {
                match message? {
                    WebSocketMessage::Text(text) => {
                        handle_pane_client_text(&outbound, &mut pane, &text).await?;
                    }
                    WebSocketMessage::Binary(bytes) => {
                        if !pane.is_operator() {
                            let _ = outbound.write_close_code(4006, "read_no_binary").await;
                            return Ok(());
                        }
                        if !rate_limiter.try_acquire() {
                            info!(share_id = %share_id, "web_share_operator_rate_limit_hit");
                            continue;
                        }
                        if handle_pane_operator_binary_frame(&handler, &outbound, &pane, &bytes).await?
                            == PaneOperatorAction::Resized
                        {
                            queue_fresh_pane_snapshot(handler.as_ref(), &outbound, &mut pane, &share_id).await?;
                        }
                    }
                    WebSocketMessage::Close => {
                        let _ = outbound.write_close().await;
                        return Ok(());
                    }
                    WebSocketMessage::Ping(payload) => {
                        outbound.write_pong(&payload).await?;
                    }
                    WebSocketMessage::Pong => {}
                }
            }
            changed = pane.revoke_rx.changed() => {
                if changed.is_ok() {
                    let reason = *pane.revoke_rx.borrow();
                    if let Some(reason) = reason {
                        notify_revoked_and_close(&outbound, reason).await?;
                        return Ok(());
                    }
                }
            }
            _ = ttl_sleep.as_mut() => {
                notify_revoked_and_close(&outbound, WebShareRevokeReason::TtlExpired).await?;
                return Ok(());
            }
            _ = alive_tick.tick() => {
                if !handler.web_target_alive(pane.target()).await {
                    notify_revoked_and_close(&outbound, WebShareRevokeReason::PaneGone).await?;
                    return Ok(());
                }
                send_viewer_count_if_changed(
                    &outbound,
                    &mut last_connection_counts,
                    pane.connection_counts(),
                )
                .await?;
            }
        }
    }
}

async fn queue_fresh_pane_snapshot(
    handler: &RequestHandler,
    outbound: &WebSocketOutbound,
    pane: &mut WebPaneStream,
    share_id: &str,
) -> io::Result<()> {
    let target = pane.target().clone();
    let (snapshot, output) = handler
        .web_resnapshot(&target)
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    pane.snapshot = snapshot;
    pane.output = output;
    queue_or_close(outbound, queue_snapshot(outbound, &pane.snapshot), share_id).await
}

async fn serve_session_loop(
    handler: Arc<RequestHandler>,
    mut socket: EncryptedWebSocketReader,
    outbound: WebSocketOutbound,
    share_id: String,
    mut session: WebSessionStream,
) -> io::Result<()> {
    let mut attach_reader = session.take_attach_reader();
    let mut rate_limiter = OperatorRateLimiter::new();
    let mut last_connection_counts = session.connection_counts();
    let mut alive_tick = tokio::time::interval(Duration::from_millis(500));
    let ttl_delay = session
        .expires_at()
        .map(duration_until)
        .unwrap_or_else(|| Duration::from_secs(365 * 24 * 60 * 60));
    let ttl_sleep = sleep(ttl_delay);
    tokio::pin!(ttl_sleep);

    loop {
        tokio::select! {
            output = attach_reader.read_attach_bytes() => {
                match output? {
                    Some(bytes) => {
                        match queue_output(&outbound, &bytes) {
                            OutboundQueueResult::Queued => {}
                            OutboundQueueResult::Backpressure => {
                                info!(share_id = %share_id, "web-share session viewer backlog exceeded; closing slow viewer");
                                let _ = outbound.write_close_code(SLOW_VIEWER_CLOSE_CODE, "viewer_backpressure").await;
                                return Ok(());
                            }
                            result => {
                                close_slow_viewer(&outbound, &share_id, result).await?;
                                return Ok(());
                            }
                        }
                    }
                    None => {
                        notify_revoked_and_close(&outbound, WebShareRevokeReason::SessionGone).await?;
                        return Ok(());
                    }
                }
            }
            message = socket.read_message() => {
                match message? {
                    WebSocketMessage::Text(text) => {
                        handle_session_client_text(handler.as_ref(), &outbound, &mut session, &text).await?;
                    }
                    WebSocketMessage::Binary(bytes) => {
                        if !session.is_operator() {
                            let _ = outbound.write_close_code(4006, "read_no_binary").await;
                            return Ok(());
                        }
                        if !rate_limiter.try_acquire() {
                            info!(share_id = %share_id, "web_share_operator_rate_limit_hit");
                            continue;
                        }
                        handle_session_operator_binary_frame(&outbound, &mut session, &bytes).await?;
                    }
                    WebSocketMessage::Close => {
                        let _ = outbound.write_close().await;
                        return Ok(());
                    }
                    WebSocketMessage::Ping(payload) => {
                        outbound.write_pong(&payload).await?;
                    }
                    WebSocketMessage::Pong => {}
                }
            }
            changed = session.revoke_rx.changed() => {
                if changed.is_ok() {
                    let reason = *session.revoke_rx.borrow();
                    if let Some(reason) = reason {
                        notify_revoked_and_close(&outbound, reason).await?;
                        return Ok(());
                    }
                }
            }
            _ = ttl_sleep.as_mut() => {
                notify_revoked_and_close(&outbound, WebShareRevokeReason::TtlExpired).await?;
                return Ok(());
            }
            _ = alive_tick.tick() => {
                if !handler.web_session_alive(session.target()).await {
                    notify_revoked_and_close(&outbound, WebShareRevokeReason::SessionGone).await?;
                    return Ok(());
                }
                send_viewer_count_if_changed(
                    &outbound,
                    &mut last_connection_counts,
                    session.connection_counts(),
                )
                .await?;
            }
        }
    }
}

async fn send_viewer_count_if_changed(
    socket: &WebSocketOutbound,
    last: &mut WebShareConnectionCounts,
    current: WebShareConnectionCounts,
) -> io::Result<()> {
    if *last == current {
        return Ok(());
    }
    send_viewer_count(socket, current).await?;
    *last = current;
    Ok(())
}

async fn notify_revoked_and_close(
    socket: &WebSocketOutbound,
    reason: WebShareRevokeReason,
) -> io::Result<()> {
    let _ = send_revoked(socket, reason).await;
    let _ = socket.write_close_code(1000, reason.as_str()).await;
    Ok(())
}

async fn queue_or_close(
    socket: &WebSocketOutbound,
    result: OutboundQueueResult,
    share_id: &str,
) -> io::Result<()> {
    match result {
        OutboundQueueResult::Queued => Ok(()),
        other => close_slow_viewer(socket, share_id, other).await,
    }
}

async fn close_slow_viewer(
    socket: &WebSocketOutbound,
    share_id: &str,
    result: OutboundQueueResult,
) -> io::Result<()> {
    info!(
        share_id = %share_id,
        ?result,
        "web-share viewer output queue closed"
    );
    let _ = socket
        .write_close_code(SLOW_VIEWER_CLOSE_CODE, "viewer_backpressure")
        .await;
    Ok(())
}

async fn write_with_timeout<F>(operation: F) -> io::Result<()>
where
    F: Future<Output = io::Result<()>>,
{
    match timeout(WEB_WRITE_TIMEOUT, operation).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "web-share client write timed out",
        )),
    }
}

fn duration_until(deadline: SystemTime) -> Duration {
    deadline
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO)
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
