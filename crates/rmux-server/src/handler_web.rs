use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmux_os::identity::UserIdentity;
use rmux_proto::{
    CreateWebShareRequest, ErrorResponse, KillSessionRequest, PaneInputRequest, PaneResizeRequest,
    PaneTargetRef, ResizePaneAdjustment, Response, RmuxError, SessionId, SessionName,
    WebShareRequest, WebShareScope,
};
use tokio::sync::{mpsc, watch};

use super::attach_support::{attach_target_for_session, AttachRegistration, ClientFlags};
use super::pane_support::resolve_pane_target_ref;
use super::RequestHandler;
use crate::outer_terminal::OuterTerminalContext;
use crate::pane_io::{self, AttachControl, LiveAttachInputContext, PaneOutputReceiver};
use crate::pane_terminal_lookup::pane_id_for_target;
use crate::server_access::current_owner_uid;
use crate::web::{ResolvedCreateWebShareRequest, WebSessionTarget, WebShareAccess, WebShareTarget};
use rmux_core::input::mode;

const WEB_ATTACH_PID_BASE: u32 = 0x8000_0000;

#[path = "handler_web_snapshot.rs"]
mod snapshot;
#[path = "handler_web_stream.rs"]
mod stream;

use snapshot::snapshot_ansi_lines;
pub(crate) use snapshot::WebPaneSnapshot;
pub(crate) use stream::{WebPaneStream, WebSessionAttachReader, WebSessionStream, WebShareStream};

impl RequestHandler {
    #[cfg(test)]
    pub(crate) async fn open_web_share(
        &self,
        token: &str,
        pin: Option<&str>,
    ) -> Result<WebShareStream, RmuxError> {
        let token_id = crate::web::SecretHashForCrypto::from_secret(token).token_id();
        self.open_web_share_token_id(&token_id, pin).await
    }

    pub(crate) fn web_listener(&self) -> rmux_proto::WebShareListener {
        self.web_shares.listener()
    }

    pub(crate) fn mark_web_listener_available(&self) {
        self.web_shares.mark_listener_available();
    }

    pub(crate) fn mark_web_listener_unavailable(&self, reason: impl Into<String>) {
        self.web_shares.mark_listener_unavailable(reason);
    }

    pub(in crate::handler) async fn handle_web_share(&self, request: WebShareRequest) -> Response {
        let response = match request {
            WebShareRequest::Create(request) => {
                let request = match self.resolve_create_web_share(request).await {
                    Ok(request) => request,
                    Err(error) => return Response::Error(ErrorResponse { error }),
                };
                let expiry_kill_target = request.expiry_kill_target();
                match self.web_shares.create(request) {
                    Ok(created) => {
                        self.spawn_web_share_expiry_task(
                            created.share_id.clone(),
                            created.expires_at_unix,
                            expiry_kill_target,
                        );
                        Ok(rmux_proto::WebShareResponse::Created(created))
                    }
                    Err(error) => Err(error),
                }
            }
            other => self.web_shares.handle(other),
        };
        match response {
            Ok(response) => Response::WebShare(response),
            Err(error) => Response::Error(ErrorResponse { error }),
        }
    }

    pub(crate) async fn open_web_share_token_id(
        &self,
        token_id: &str,
        pin: Option<&str>,
    ) -> Result<WebShareStream, RmuxError> {
        let access = self.web_shares.connect_token_id(token_id, pin).await?;
        self.open_web_share_access(access).await
    }

    pub(crate) fn web_share_token_secret(
        &self,
        token_id: &str,
    ) -> Option<crate::web::SecretHashForCrypto> {
        self.web_shares.token_secret(token_id)
    }

    pub(crate) fn known_web_share_token_id_origin_allowed(
        &self,
        token_id: &str,
        origin: &str,
    ) -> Option<bool> {
        self.web_shares
            .known_token_id_origin_allowed(token_id, origin)
    }

    pub(in crate::handler) fn prune_web_session(&self, removed: Option<(SessionName, SessionId)>) {
        if let Some((name, id)) = removed {
            self.web_shares.remove_targets_for_sessions(&[(name, id)]);
        }
    }

    async fn open_web_share_access(
        &self,
        access: WebShareAccess,
    ) -> Result<WebShareStream, RmuxError> {
        match access.target().clone() {
            WebShareTarget::Pane(target) => {
                let target = self.stable_web_target(&target).await?;
                let (snapshot, output) = self.web_resnapshot(&target).await?;
                let revoke_rx = access.revoke_receiver();
                Ok(WebShareStream::Pane(Box::new(WebPaneStream {
                    access,
                    output,
                    revoke_rx,
                    snapshot,
                    target,
                })))
            }
            WebShareTarget::Session(session_target) => {
                let stream = self.open_web_session_share(access, session_target).await?;
                Ok(WebShareStream::Session(Box::new(stream)))
            }
        }
    }

    async fn open_web_session_share(
        &self,
        access: WebShareAccess,
        session_target: WebSessionTarget,
    ) -> Result<WebSessionStream, RmuxError> {
        let session_target = self.current_web_session_target(&session_target).await?;
        let (server_transport, client_stream) = pane_io::in_process_attach_pair();
        let attach_pid = self.allocate_web_attach_pid().await?;
        let controls = access.controls();
        let mut flags = ClientFlags::default();
        let can_write = access.is_operator();
        if !can_write {
            flags = flags.with_read_only();
        }
        if controls {
            flags.insert(ClientFlags::WEB_CONTROLS);
        }

        let terminal_context = OuterTerminalContext::default();
        let (control_tx, control_rx) = mpsc::unbounded_channel::<AttachControl>();
        let closing = Arc::new(AtomicBool::new(false));
        let persistent_overlay_epoch = Arc::new(AtomicU64::new(0));
        let attached_count = self
            .active_attach
            .lock()
            .await
            .attached_count(session_target.name());
        let (session_target, target, initial_size) = {
            let state = self.state.lock().await;
            let session = state
                .sessions
                .session_by_id(session_target.id())
                .ok_or_else(|| session_not_found_web(session_target.name()))?;
            let current_target = WebSessionTarget::new(session.name().clone(), session.id());
            let size = session.window().size();
            (
                current_target.clone(),
                attach_target_for_session(
                    &state,
                    current_target.name(),
                    attached_count,
                    &terminal_context,
                )?,
                size,
            )
        };
        let attach_id = self
            .register_attach_with_access(
                attach_pid,
                session_target.name().clone(),
                AttachRegistration {
                    control_tx,
                    closing: closing.clone(),
                    persistent_overlay_epoch: persistent_overlay_epoch.clone(),
                    terminal_context,
                    flags,
                    uid: current_owner_uid(),
                    user: UserIdentity::Uid(current_owner_uid()),
                    can_write,
                    client_size: None,
                },
            )
            .await;
        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        let task_handler = self.clone();
        tokio::spawn(async move {
            let _keep_shutdown_open = _shutdown_tx;
            let result = pane_io::forward_attach(
                server_transport,
                target,
                Vec::new(),
                shutdown_rx,
                control_rx,
                closing,
                persistent_overlay_epoch,
                LiveAttachInputContext {
                    handler: Arc::new(task_handler.clone()),
                    attach_pid,
                },
            )
            .await;
            task_handler.finish_attach(attach_pid, attach_id).await;
            if let Err(error) = result {
                tracing::debug!(attach_pid, "web session attach ended: {error}");
            }
        });

        let revoke_rx = access.revoke_receiver();
        let (reader, writer) = tokio::io::split(client_stream);
        Ok(WebSessionStream {
            access,
            revoke_rx,
            target: session_target,
            initial_size,
            writer,
            reader: Some(WebSessionAttachReader::new(reader)),
        })
    }

    pub(crate) async fn web_resnapshot(
        &self,
        target: &PaneTargetRef,
    ) -> Result<(WebPaneSnapshot, PaneOutputReceiver), RmuxError> {
        let (pane_output, transcript) = {
            let state = self.state.lock().await;
            let target = resolve_pane_target_ref(&state, target)?;
            let pane_output = state.pane_output_for_target(
                target.session_name(),
                target.window_index(),
                target.pane_index(),
            )?;
            let transcript = state.transcript_handle(&target)?;
            (pane_output, transcript)
        };
        let (output_sequence, snapshot) = pane_output.capture_with_next_sequence(|| {
            let transcript = transcript
                .lock()
                .expect("pane transcript mutex must not be poisoned");
            let screen = transcript.clone_screen();
            let size = screen.size();
            let (cursor_col, cursor_row) = screen.cursor_position();
            WebPaneSnapshot {
                cols: size.cols,
                rows: size.rows,
                output_sequence: 0,
                ansi_lines: snapshot_ansi_lines(&screen),
                cursor_row: cursor_row.min(u32::from(size.rows.saturating_sub(1))) as u16,
                cursor_col: cursor_col.min(u32::from(size.cols.saturating_sub(1))) as u16,
                cursor_visible: screen.mode() & mode::MODE_CURSOR != 0,
            }
        });
        let snapshot = WebPaneSnapshot {
            output_sequence,
            ..snapshot
        };
        let output = pane_output.subscribe_from_sequence(output_sequence);
        Ok((snapshot, output))
    }

    pub(crate) async fn web_send_text(
        &self,
        target: &PaneTargetRef,
        text: String,
    ) -> Result<(), RmuxError> {
        let response = self
            .handle_pane_input_ref(PaneInputRequest {
                target: target.clone(),
                keys: vec![text],
                literal: true,
            })
            .await;
        response_to_result(response)
    }

    pub(crate) async fn web_send_key(
        &self,
        target: &PaneTargetRef,
        key: String,
    ) -> Result<(), RmuxError> {
        let response = self
            .handle_pane_input_ref(PaneInputRequest {
                target: target.clone(),
                keys: vec![key],
                literal: false,
            })
            .await;
        response_to_result(response)
    }

    pub(crate) async fn web_session_logout(
        &self,
        session_target: &WebSessionTarget,
    ) -> Result<(), RmuxError> {
        let session_target = self.current_web_session_target(session_target).await?;
        let response = self
            .handle_kill_session(KillSessionRequest {
                target: session_target.name().clone(),
                kill_all_except_target: false,
                clear_alerts: false,
            })
            .await;
        response_to_result(response)
    }

    fn spawn_web_share_expiry_task(
        &self,
        share_id: String,
        expires_at_unix: Option<u64>,
        kill_target: Option<WebSessionTarget>,
    ) {
        let Some(expires_at_unix) = expires_at_unix else {
            return;
        };
        let handler = self.clone();
        tokio::spawn(async move {
            // The public response carries whole Unix seconds, while the registry
            // keeps the exact SystemTime deadline. Wake after the advertised
            // second and retry through the rounding window before giving up.
            tokio::time::sleep(duration_until_unix(expires_at_unix)).await;
            let Some(expired) = handler
                .wait_for_web_share_expiry(&share_id, expires_at_unix)
                .await
            else {
                return;
            };
            tracing::info!(share_id = %expired.share_id, "web_share_expired");
            let target = kill_target.or(expired.kill_session);
            if let Some(target) = target {
                if let Err(error) = handler.web_session_logout(&target).await {
                    tracing::debug!(
                        share_id = %expired.share_id,
                        session = %target.name(),
                        "web-share expiry session kill skipped: {error}"
                    );
                }
            }
        });
    }

    async fn wait_for_web_share_expiry(
        &self,
        share_id: &str,
        expires_at_unix: u64,
    ) -> Option<crate::web::ExpiredWebShare> {
        let retry_until =
            UNIX_EPOCH + Duration::from_secs(expires_at_unix) + Duration::from_secs(1);
        loop {
            if let Some(expired) = self.web_shares.expire_if_due(share_id) {
                return Some(expired);
            }
            if SystemTime::now() >= retry_until {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub(crate) async fn web_resize(
        &self,
        target: &PaneTargetRef,
        cols: u16,
        rows: u16,
    ) -> Result<(), RmuxError> {
        let response = self
            .handle_pane_resize_ref(PaneResizeRequest {
                target: target.clone(),
                adjustment: ResizePaneAdjustment::AbsoluteSize {
                    columns: cols,
                    rows,
                },
            })
            .await;
        response_to_result(response)
    }

    async fn resolve_create_web_share(
        &self,
        request: CreateWebShareRequest,
    ) -> Result<ResolvedCreateWebShareRequest, rmux_proto::RmuxError> {
        let state = self.state.lock().await;
        let target = match &request.scope {
            WebShareScope::Pane(raw_target) => {
                let target = resolve_pane_target_ref(&state, raw_target)?;
                let pane_id = pane_id_for_target(
                    &state.sessions,
                    target.session_name(),
                    target.window_index(),
                    target.pane_index(),
                )?;
                WebShareTarget::pane(PaneTargetRef::by_id(target.session_name().clone(), pane_id))
            }
            WebShareScope::Session(session_name) => {
                let session = state
                    .sessions
                    .session(session_name)
                    .ok_or_else(|| session_not_found_web(session_name))?;
                WebShareTarget::session(session.name().clone(), session.id())
            }
        };
        Ok(ResolvedCreateWebShareRequest::new(request, target))
    }

    async fn stable_web_target(&self, target: &PaneTargetRef) -> Result<PaneTargetRef, RmuxError> {
        let state = self.state.lock().await;
        let target = resolve_pane_target_ref(&state, target)?;
        let pane_id = pane_id_for_target(
            &state.sessions,
            target.session_name(),
            target.window_index(),
            target.pane_index(),
        )?;
        Ok(PaneTargetRef::by_id(target.session_name().clone(), pane_id))
    }

    pub(crate) async fn web_target_alive(&self, target: &PaneTargetRef) -> bool {
        let state = self.state.lock().await;
        resolve_pane_target_ref(&state, target).is_ok()
    }

    pub(crate) async fn web_session_alive(&self, session_target: &WebSessionTarget) -> bool {
        self.current_web_session_target(session_target)
            .await
            .is_ok()
    }

    async fn current_web_session_target(
        &self,
        session_target: &WebSessionTarget,
    ) -> Result<WebSessionTarget, RmuxError> {
        let state = self.state.lock().await;
        state
            .sessions
            .session_by_id(session_target.id())
            .map(|session| WebSessionTarget::new(session.name().clone(), session.id()))
            .ok_or_else(|| session_not_found_web(session_target.name()))
    }

    async fn allocate_web_attach_pid(&self) -> Result<u32, RmuxError> {
        for _ in 0..1024 {
            let id = self.allocate_connection_id();
            let candidate = WEB_ATTACH_PID_BASE | (id as u32 & !WEB_ATTACH_PID_BASE);
            if !self
                .active_attach
                .lock()
                .await
                .by_pid
                .contains_key(&candidate)
            {
                return Ok(candidate);
            }
        }
        Err(RmuxError::Server(
            "failed to allocate web attach client id".to_owned(),
        ))
    }
}

fn duration_until_unix(expires_at_unix: u64) -> Duration {
    let deadline = UNIX_EPOCH + Duration::from_secs(expires_at_unix);
    deadline
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO)
}

fn session_not_found_web(session_name: &SessionName) -> RmuxError {
    RmuxError::Server(format!("can't find session: {session_name}"))
}

fn response_to_result(response: Response) -> Result<(), RmuxError> {
    match response {
        Response::Error(error) => Err(error.error),
        _ => Ok(()),
    }
}

#[cfg(test)]
#[path = "handler_web_tests.rs"]
mod tests;
