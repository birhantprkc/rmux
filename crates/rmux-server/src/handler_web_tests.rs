use super::*;
use rmux_proto::WebShareCreatedResponse;
use rmux_proto::{
    CreateWebShareRequest, KillPaneRequest, KillSessionRequest, ListWebSharesRequest,
    NewSessionRequest, PaneTarget, RenameSessionRequest, Request, Response, SessionName,
    TerminalSize, WebShareScope,
};
use tokio::time::{sleep, timeout, Duration, Instant};

#[tokio::test]
async fn web_share_create_resolves_slot_target_to_stable_pane_id() {
    let handler = RequestHandler::new();
    let session_name = new_session(&handler, "alpha").await;
    let created = create_share(
        &handler,
        share_request(WebShareScope::Pane(
            rmux_proto::PaneTarget::new(session_name.clone(), 0).into(),
        )),
    )
    .await;
    assert!(matches!(
        created.scope,
        WebShareScope::Pane(PaneTargetRef::Id {
            session_name: ref actual,
            ..
        }) if actual == &session_name
    ));
    assert!(created.read_url.contains("#e=wss://share.example/share&t="));
}

#[tokio::test]
async fn web_session_share_opens_portable_attach_transport() {
    let handler = RequestHandler::new();
    let session_name = new_session(&handler, "websession").await;
    let created = create_share(
        &handler,
        CreateWebShareRequest {
            writable: true,
            controls: true,
            ..share_request(WebShareScope::Session(session_name))
        },
    )
    .await;
    let operator_url = created.operator_url.as_deref().expect("operator URL");
    let operator_token = token_from_url(operator_url);
    let stream = handler
        .open_web_share(&operator_token, None)
        .await
        .expect("session web share opens");
    let WebShareStream::Session(mut session_stream) = stream else {
        panic!("expected session web share stream");
    };
    let mut reader = session_stream.take_attach_reader();
    let bytes = timeout(Duration::from_secs(2), reader.read_attach_bytes())
        .await
        .expect("attach stream should produce initial bytes")
        .expect("attach read succeeds")
        .expect("initial attach bytes are present");

    assert!(!bytes.is_empty());
}

#[tokio::test]
async fn web_session_operator_without_controls_remains_writable_attach() {
    let handler = RequestHandler::new();
    let session_name = new_session(&handler, "websession-write").await;
    let created = create_share(
        &handler,
        CreateWebShareRequest {
            writable: true,
            ..share_request(WebShareScope::Session(session_name.clone()))
        },
    )
    .await;
    let operator_token = token_from_url(created.operator_url.as_deref().expect("operator URL"));
    let stream = handler
        .open_web_share(&operator_token, None)
        .await
        .expect("session web share opens");
    let WebShareStream::Session(session_stream) = stream else {
        panic!("expected session web share stream");
    };
    assert!(session_stream.is_operator());
    assert!(!session_stream.controls());

    let active_attach = handler.active_attach.lock().await;
    let active = active_attach
        .by_pid
        .values()
        .find(|active| active.session_name == session_name)
        .expect("web session attach is registered");
    assert!(active.can_write);
    assert!(!active.flags.contains(ClientFlags::READONLY));
    assert!(!active.flags.contains(ClientFlags::WEB_CONTROLS));
}

#[tokio::test]
async fn web_share_expiry_kills_session_after_unix_second_rounding_window() {
    let handler = RequestHandler::new();
    let session_name = new_session(&handler, "websession-expire").await;
    create_share(
        &handler,
        CreateWebShareRequest {
            ttl_seconds: Some(1),
            kill_session_on_expire: true,
            ..share_request(WebShareScope::Session(session_name.clone()))
        },
    )
    .await;

    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let removed = {
            let state = handler.state.lock().await;
            state.sessions.session(&session_name).is_none()
        };
        if removed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "expired web-share did not kill its session"
        );
        sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn kill_session_prunes_web_session_share_before_name_reuse() {
    let handler = RequestHandler::new();
    let session_name = new_session(&handler, "websession").await;
    let created = create_share(
        &handler,
        CreateWebShareRequest {
            writable: true,
            controls: true,
            ..share_request(WebShareScope::Session(session_name.clone()))
        },
    )
    .await;
    let operator_token = token_from_url(created.operator_url.as_deref().expect("operator URL"));

    let killed = handler
        .handle(Request::KillSession(KillSessionRequest {
            target: session_name.clone(),
            kill_all_except_target: false,
            clear_alerts: false,
        }))
        .await;
    assert!(matches!(killed, Response::KillSession(_)));

    assert!(
        list_shares(&handler).await.is_empty(),
        "shares for a removed session should be pruned"
    );

    new_session(&handler, session_name.as_str()).await;

    let error = handler
        .open_web_share(&operator_token, None)
        .await
        .err()
        .expect("old share must not attach to a recreated session");
    assert!(error.to_string().contains("does not exist"));
}

#[tokio::test]
async fn killing_last_pane_prunes_web_session_share() {
    let handler = RequestHandler::new();
    let session_name = new_session(&handler, "websession-kill-pane").await;
    let created = create_share(
        &handler,
        share_request(WebShareScope::Session(session_name.clone())),
    )
    .await;
    let read_token = token_from_url(&created.read_url);

    let killed = handler
        .handle(Request::KillPane(KillPaneRequest {
            target: PaneTarget::new(session_name.clone(), 0),
            kill_all_except: false,
        }))
        .await;
    assert!(matches!(killed, Response::KillPane(_)));

    assert!(
        list_shares(&handler).await.is_empty(),
        "session shares should be pruned when the last pane destroys the session"
    );

    let error = handler
        .open_web_share(&read_token, None)
        .await
        .err()
        .expect("old share must not attach after the session was destroyed");
    assert!(error.to_string().contains("does not exist"));
}

#[tokio::test]
async fn kill_session_on_expire_follows_renamed_session_id() {
    let handler = RequestHandler::new();
    let session_name = new_session(&handler, "websession-expiry").await;
    let renamed_session = SessionName::new("websession-expiry-renamed").expect("valid session");
    create_share(
        &handler,
        CreateWebShareRequest {
            ttl_seconds: Some(2),
            writable: true,
            kill_session_on_expire: true,
            ..share_request(WebShareScope::Session(session_name.clone()))
        },
    )
    .await;

    let renamed = handler
        .handle(Request::RenameSession(RenameSessionRequest {
            target: session_name.clone(),
            new_name: renamed_session.clone(),
        }))
        .await;
    assert!(matches!(renamed, Response::RenameSession(_)));

    timeout(Duration::from_secs(4), async {
        loop {
            let session_gone = {
                let state = handler.state.lock().await;
                state.sessions.session(&renamed_session).is_none()
            };
            if session_gone {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("expiry task should kill the renamed session by id");

    let state = handler.state.lock().await;
    assert!(state.sessions.session(&session_name).is_none());
    assert!(state.sessions.session(&renamed_session).is_none());
}

fn token_from_url(url: &str) -> String {
    url.split_once('#')
        .and_then(|(_, fragment)| {
            fragment.split('&').find_map(|param| {
                let (key, value) = param.split_once('=')?;
                (key == "t").then_some(value.to_owned())
            })
        })
        .expect("URL contains access token")
}

async fn new_session(handler: &RequestHandler, name: &str) -> SessionName {
    let session_name = SessionName::new(name).expect("valid session");
    assert!(matches!(
        handler
            .handle(Request::NewSession(NewSessionRequest {
                session_name: session_name.clone(),
                detached: true,
                size: Some(TerminalSize { cols: 80, rows: 24 }),
                environment: None,
            }))
            .await,
        Response::NewSession(_)
    ));
    session_name
}

async fn create_share(
    handler: &RequestHandler,
    request: CreateWebShareRequest,
) -> WebShareCreatedResponse {
    let response = handler
        .handle(Request::WebShare(WebShareRequest::Create(request)))
        .await;
    let Response::WebShare(rmux_proto::WebShareResponse::Created(created)) = response else {
        panic!("expected created web-share response");
    };
    created
}

fn share_request(scope: WebShareScope) -> CreateWebShareRequest {
    CreateWebShareRequest {
        scope,
        public_base_url: Some("https://share.example".to_owned()),
        frontend_url: None,
        ttl_seconds: None,
        expires_at_unix: None,
        max_readers: Some(1),
        url_options: Default::default(),
        require_pin: false,
        terminal_palette: None,
        writable: false,
        controls: false,
        kill_session_on_expire: false,
    }
}

async fn list_shares(handler: &RequestHandler) -> Vec<rmux_proto::WebShareSummary> {
    let response = handler
        .handle(Request::WebShare(WebShareRequest::List(
            ListWebSharesRequest,
        )))
        .await;
    let Response::WebShare(rmux_proto::WebShareResponse::List(listed)) = response else {
        panic!("expected listed web-share response");
    };
    listed.shares
}
