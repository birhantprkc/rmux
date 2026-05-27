use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmux_proto::{
    CommandOutput, CreateWebShareRequest, ListWebSharesRequest, LookupWebShareRequest,
    StopAllWebSharesRequest, StopWebShareRequest, WebShareConfigRequest, WebShareConfigResponse,
    WebShareCreatedResponse, WebShareListResponse, WebShareListener, WebShareLookupResponse,
    WebShareResponse, WebShareStoppedAllResponse, WebShareStoppedResponse,
};
use rmux_proto::{RmuxError, SessionId, SessionName};
use tokio::sync::watch;
use tokio::time::sleep;
use tracing::info;

#[path = "registry_output.rs"]
mod output;
#[path = "registry_state.rs"]
mod state;

use super::backoff::AuthBackoff;
use super::leases::LeaseBook;
use super::origin::{validate_frontend_url, validate_public_base_url, FrontendUrl};
use super::record::{
    system_time_to_unix, WebSessionTarget, WebShareAccess, WebShareRecord, WebShareRevokeReason,
    WebShareTarget,
};
use super::secrets::{
    random_pairing_code, random_share_id, random_token, valid_token_id_shape, SecretHash,
};
use super::settings::WebShareSettings;
use output::{created_output, list_output, lookup_output, stopped_output};
pub(crate) use state::ExpiredWebShare;
use state::{WebListenerState, WebShareState};

const DEFAULT_MAX_READERS: u16 = 5;
const DEFAULT_TTL_SECONDS: u64 = 60 * 60;
const MAX_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;

#[derive(Debug)]
pub(crate) struct ResolvedCreateWebShareRequest {
    request: CreateWebShareRequest,
    target: WebShareTarget,
}

impl ResolvedCreateWebShareRequest {
    pub(crate) fn new(request: CreateWebShareRequest, target: WebShareTarget) -> Self {
        Self { request, target }
    }

    pub(crate) fn expiry_kill_target(&self) -> Option<WebSessionTarget> {
        if !self.request.kill_session_on_expire {
            return None;
        }
        match &self.target {
            WebShareTarget::Session(target) => Some(target.clone()),
            WebShareTarget::Pane(_) => None,
        }
    }
}

#[cfg(test)]
impl From<CreateWebShareRequest> for ResolvedCreateWebShareRequest {
    fn from(request: CreateWebShareRequest) -> Self {
        let target = match &request.scope {
            rmux_proto::WebShareScope::Pane(target) => WebShareTarget::pane(target.clone()),
            rmux_proto::WebShareScope::Session(name) => {
                WebShareTarget::session(name.clone(), rmux_proto::SessionId::new(0))
            }
        };
        Self { request, target }
    }
}

#[derive(Debug)]
pub(crate) struct WebShareRegistry {
    backoff: AuthBackoff,
    inner: Mutex<WebShareState>,
    next_id: AtomicU64,
    settings: WebShareSettings,
}

impl Default for WebShareRegistry {
    fn default() -> Self {
        Self::new(WebShareSettings::default())
    }
}

impl WebShareRegistry {
    #[cfg(test)]
    pub(crate) async fn connect(
        &self,
        token: &str,
        pin: Option<&str>,
    ) -> Result<WebShareAccess, RmuxError> {
        let token_id = SecretHash::from_secret(token).token_id();
        self.connect_token_id(&token_id, pin).await
    }

    #[cfg(test)]
    pub(crate) fn known_token_origin_allowed(&self, token: &str, origin: &str) -> Option<bool> {
        let token_id = SecretHash::from_secret(token).token_id();
        self.known_token_id_origin_allowed(&token_id, origin)
    }

    pub(crate) fn new(settings: WebShareSettings) -> Self {
        Self {
            backoff: AuthBackoff::new(),
            inner: Mutex::new(WebShareState::default()),
            next_id: AtomicU64::new(1),
            settings,
        }
    }

    pub(crate) fn handle(
        &self,
        request: rmux_proto::WebShareRequest,
    ) -> Result<WebShareResponse, RmuxError> {
        match request {
            rmux_proto::WebShareRequest::Create(_) => Err(RmuxError::Server(
                "web-share create requires a resolved server target".to_owned(),
            )),
            rmux_proto::WebShareRequest::List(request) => {
                Ok(WebShareResponse::List(self.list(request)))
            }
            rmux_proto::WebShareRequest::Stop(request) => {
                Ok(WebShareResponse::Stopped(self.stop(request)))
            }
            rmux_proto::WebShareRequest::StopAll(request) => {
                Ok(WebShareResponse::StoppedAll(self.stop_all(request)))
            }
            rmux_proto::WebShareRequest::Lookup(request) => {
                Ok(WebShareResponse::Lookup(self.lookup(request)))
            }
            rmux_proto::WebShareRequest::Config(request) => {
                self.config(request).map(WebShareResponse::Config)
            }
        }
    }

    pub(crate) fn create(
        &self,
        resolved: impl Into<ResolvedCreateWebShareRequest>,
    ) -> Result<WebShareCreatedResponse, RmuxError> {
        let resolved = resolved.into();
        let ResolvedCreateWebShareRequest { request, target } = resolved;
        self.require_listener_available()?;
        if request.controls && !request.writable {
            return Err(RmuxError::Server(
                "web-share controls require --writable".to_owned(),
            ));
        }
        if request.controls && request.scope.is_pane() {
            return Err(RmuxError::Server(
                "web-share controls require a session target".to_owned(),
            ));
        }
        let max_readers = request.max_readers.unwrap_or(DEFAULT_MAX_READERS);
        if max_readers == 0 {
            return Err(RmuxError::Server(
                "web-share requires at least one read slot".to_owned(),
            ));
        }
        let endpoint_origin = self.endpoint_origin(request.public_base_url.as_deref())?;
        let frontend = self.frontend(request.frontend_url.as_deref())?;
        let share_id = self.next_share_id()?;
        let read_token = random_token()?;
        let operator_token = request.writable.then(random_token).transpose()?;
        let read_token_hash = SecretHash::from_secret(&read_token);
        let operator_token_hash = operator_token.as_deref().map(SecretHash::from_secret);
        let pairing_code = request.require_pin.then(random_pairing_code).transpose()?;
        if request.kill_session_on_expire && !matches!(target, WebShareTarget::Session(_)) {
            return Err(RmuxError::Server(
                "web-share --kill-session-on-expire requires a session target".to_owned(),
            ));
        }
        let expires_at = Some(resolve_expiration(&request)?);
        let ttl_seconds = expires_at
            .and_then(|deadline| deadline.duration_since(SystemTime::now()).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let lease_book = LeaseBook::new(usize::from(max_readers));
        let (revoke_tx, _) = watch::channel(None);
        let terminal_palette = request.terminal_palette.as_deref().cloned();

        let record = WebShareRecord {
            allow_loopback_development_origins: request.public_base_url.is_none(),
            endpoint_origin,
            expires_at,
            frontend_origin: frontend.origin,
            frontend_url: frontend.url,
            kill_session_on_expire: request.kill_session_on_expire,
            lease_book,
            max_readers,
            operator_token_hash,
            pairing_code: pairing_code.clone(),
            revoke_tx,
            controls: request.controls,
            share_id: share_id.clone(),
            target: target.clone(),
            terminal_palette,
            url_options: request.url_options,
            read_token_hash,
            writable: request.writable,
        };

        let read_url = record.read_url(&read_token);
        let operator_url = record.operator_url(operator_token.as_deref());
        let summary_scope = record.target.scope();
        let expires_at_unix = expires_at.and_then(system_time_to_unix);
        self.inner
            .lock()
            .expect("web-share registry mutex must not be poisoned")
            .insert(record);
        info!(
            share_id = %share_id,
            scope = %summary_scope,
            writable = request.writable,
            controls = request.controls,
            ttl_seconds,
            max_readers,
            public = request.public_base_url.is_some(),
            pin_required = request.require_pin,
            listener_port = self.settings.port,
            "web_share_created"
        );

        let output = created_output(
            &read_url,
            pairing_code.as_deref(),
            expires_at_unix,
            request.kill_session_on_expire,
        );
        Ok(WebShareCreatedResponse {
            share_id,
            scope: summary_scope,
            read_url,
            operator_url,
            expires_at_unix,
            pairing_code,
            max_readers,
            writable: request.writable,
            controls: request.controls,
            kill_session_on_expire: request.kill_session_on_expire,
            output,
        })
    }

    pub(crate) fn expire_if_due(&self, share_id: &str) -> Option<ExpiredWebShare> {
        self.inner
            .lock()
            .expect("web-share registry mutex must not be poisoned")
            .expire_if_due(share_id)
    }

    pub(crate) fn list(&self, _request: ListWebSharesRequest) -> WebShareListResponse {
        let mut inner = self
            .inner
            .lock()
            .expect("web-share registry mutex must not be poisoned");
        inner.prune_expired();
        let shares = inner.summaries();
        WebShareListResponse {
            output: list_output(&shares),
            shares,
        }
    }

    pub(crate) fn stop(&self, request: StopWebShareRequest) -> WebShareStoppedResponse {
        let stopped = self
            .inner
            .lock()
            .expect("web-share registry mutex must not be poisoned")
            .remove(&request.share_id, WebShareRevokeReason::StoppedByOwner);
        if stopped {
            info!(share_id = %request.share_id, reason = "cli_stop", "web_share_stopped");
        }
        WebShareStoppedResponse {
            output: stopped_output(&request.share_id, stopped),
            share_id: request.share_id,
            stopped,
        }
    }

    pub(crate) fn stop_all(&self, _request: StopAllWebSharesRequest) -> WebShareStoppedAllResponse {
        let stopped = self
            .inner
            .lock()
            .expect("web-share registry mutex must not be poisoned")
            .clear(WebShareRevokeReason::StoppedByOwner);
        if stopped > 0 {
            info!(stopped, reason = "cli_stop_all", "web_share_stop_all");
        }
        WebShareStoppedAllResponse {
            output: CommandOutput::from_stdout(format!("stopped {stopped}\n")),
            stopped,
        }
    }

    pub(crate) fn remove_targets_for_sessions(&self, sessions: &[(SessionName, SessionId)]) -> u32 {
        if sessions.is_empty() {
            return 0;
        }
        let removed = self
            .inner
            .lock()
            .expect("web-share registry mutex must not be poisoned")
            .remove_targets_for_sessions(sessions, WebShareRevokeReason::SessionGone);
        if removed > 0 {
            info!(removed, reason = "session_removed", "web_share_pruned");
        }
        removed
    }

    pub(crate) fn lookup(&self, request: LookupWebShareRequest) -> WebShareLookupResponse {
        let mut inner = self
            .inner
            .lock()
            .expect("web-share registry mutex must not be poisoned");
        inner.prune_expired();
        let share = inner.summary(&request.share_id);
        WebShareLookupResponse {
            output: lookup_output(share.as_ref()),
            share,
        }
    }

    pub(crate) fn config(
        &self,
        _request: WebShareConfigRequest,
    ) -> Result<WebShareConfigResponse, RmuxError> {
        self.require_listener_available()?;
        let listener = self.listener();
        Ok(WebShareConfigResponse {
            output: CommandOutput::from_stdout(format!(
                "{}:{} {}\n",
                listener.host, listener.port, listener.frontend_origin
            )),
            listener,
        })
    }

    pub(crate) async fn connect_token_id(
        &self,
        token_id: &str,
        pin: Option<&str>,
    ) -> Result<WebShareAccess, RmuxError> {
        if !valid_token_id_shape(token_id) {
            return Err(RmuxError::Server("invalid web-share token id".to_owned()));
        }
        let lookup = {
            let mut inner = self
                .inner
                .lock()
                .expect("web-share registry mutex must not be poisoned");
            inner.prune_expired();
            inner.capability_by_token_id(token_id)
        };
        let backoff_key = lookup
            .as_ref()
            .map(|capability| capability.share_id.clone())
            .unwrap_or_else(|| format!("token_id:{token_id}"));
        let delay = self.backoff.delay_before_next_attempt(&backoff_key);
        if !delay.is_zero() {
            sleep(delay).await;
        }

        let result = {
            let mut inner = self
                .inner
                .lock()
                .expect("web-share registry mutex must not be poisoned");
            inner.prune_expired();
            match inner.capability_by_token_id(token_id) {
                Some(capability) => match inner.records.get(&capability.share_id) {
                    Some(record) => record.connect(pin, capability.role),
                    None => Err(RmuxError::Server(
                        "web-share does not exist or has expired".to_owned(),
                    )),
                },
                None => Err(RmuxError::Server(
                    "web-share does not exist or has expired".to_owned(),
                )),
            }
        };

        match result {
            Ok(access) => {
                self.backoff.record_success(&backoff_key);
                info!(share_id = %access.share_id(), role = ?access.connect_role(), "web_share_access_granted");
                Ok(access)
            }
            Err(error) => {
                if is_auth_failure_for_backoff(&error) {
                    let failure = self.backoff.record_failure(&backoff_key);
                    info!(
                        share_id = %backoff_key,
                        fails = failure.fails,
                        next_delay_ms = failure.next_delay.as_millis(),
                        "web_share_auth_backoff"
                    );
                }
                Err(error)
            }
        }
    }

    pub(crate) fn known_token_id_origin_allowed(
        &self,
        token_id: &str,
        origin: &str,
    ) -> Option<bool> {
        if !valid_token_id_shape(token_id) {
            return None;
        }
        let mut inner = self
            .inner
            .lock()
            .expect("web-share registry mutex must not be poisoned");
        inner.prune_expired();
        let capability = inner.capability_by_token_id(token_id)?;
        inner
            .records
            .get(&capability.share_id)
            .map(|record| record.origin_allowed(origin))
    }

    pub(crate) fn token_secret(&self, token_id: &str) -> Option<SecretHash> {
        if !valid_token_id_shape(token_id) {
            return None;
        }
        let mut inner = self
            .inner
            .lock()
            .expect("web-share registry mutex must not be poisoned");
        inner.prune_expired();
        inner
            .capability_by_token_id(token_id)
            .map(|capability| capability.secret_hash)
    }

    pub(crate) fn listener(&self) -> WebShareListener {
        self.settings.listener()
    }

    pub(crate) fn mark_listener_available(&self) {
        self.inner
            .lock()
            .expect("web-share registry mutex must not be poisoned")
            .listener = WebListenerState::Available;
    }

    pub(crate) fn mark_listener_unavailable(&self, reason: impl Into<String>) {
        self.inner
            .lock()
            .expect("web-share registry mutex must not be poisoned")
            .listener = WebListenerState::Unavailable(reason.into());
    }

    fn next_share_id(&self) -> Result<String, RmuxError> {
        for _ in 0..32 {
            let share_id = random_share_id()?;
            if !self
                .inner
                .lock()
                .expect("web-share registry mutex must not be poisoned")
                .records
                .contains_key(&share_id)
            {
                return Ok(share_id);
            }
        }
        let sequence = self.next_id.fetch_add(1, Ordering::Relaxed);
        Err(RmuxError::Server(format!(
            "failed to create unique web-share id after {sequence} attempts"
        )))
    }

    fn endpoint_origin(&self, requested: Option<&str>) -> Result<String, RmuxError> {
        match requested {
            Some(value) => validate_public_base_url(value),
            None => Ok(self.settings.local_endpoint_origin()),
        }
    }

    fn frontend(&self, requested: Option<&str>) -> Result<FrontendUrl, RmuxError> {
        match requested {
            Some(value) => validate_frontend_url(value),
            None => Ok(FrontendUrl {
                origin: self.settings.frontend_origin.clone(),
                url: self.settings.frontend_url.clone(),
            }),
        }
    }

    fn require_listener_available(&self) -> Result<(), RmuxError> {
        let inner = self
            .inner
            .lock()
            .expect("web-share registry mutex must not be poisoned");
        match &inner.listener {
            WebListenerState::Available => Ok(()),
            WebListenerState::Unavailable(reason) => Err(RmuxError::Server(format!(
                "web-share listener unavailable: {reason}"
            ))),
        }
    }
}

fn is_auth_failure_for_backoff(error: &RmuxError) -> bool {
    let message = error.to_string();
    message.contains("invalid web-share key")
        || message.contains("invalid web-share pairing code")
        || message.contains("does not exist or has expired")
}

fn resolve_expiration(request: &CreateWebShareRequest) -> Result<SystemTime, RmuxError> {
    if request.ttl_seconds.is_some() && request.expires_at_unix.is_some() {
        return Err(RmuxError::Server(
            "web-share --ttl and --expires-at are mutually exclusive".to_owned(),
        ));
    }
    let now = SystemTime::now();
    let deadline = if let Some(expires_at_unix) = request.expires_at_unix {
        UNIX_EPOCH + Duration::from_secs(expires_at_unix)
    } else {
        let ttl_seconds = request.ttl_seconds.unwrap_or(DEFAULT_TTL_SECONDS);
        if ttl_seconds == 0 || ttl_seconds > MAX_TTL_SECONDS {
            return Err(RmuxError::Server(
                "web-share TTL must be between 1 second and 7 days".to_owned(),
            ));
        }
        now + Duration::from_secs(ttl_seconds)
    };
    if deadline <= now {
        return Err(RmuxError::Server(
            "web-share --expires-at must be in the future".to_owned(),
        ));
    }
    if deadline.duration_since(now).unwrap_or(Duration::ZERO) > Duration::from_secs(MAX_TTL_SECONDS)
    {
        return Err(RmuxError::Server(
            "web-share expiration must be within 7 days".to_owned(),
        ));
    }
    Ok(deadline)
}
