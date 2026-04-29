use super::*;

#[tokio::test]
async fn lock_client_emits_lock_control_before_refresh() {
    let handler = RequestHandler::new();
    let requester_pid = std::process::id();
    let alpha = session_name("alpha");

    let created = handler
        .handle(Request::NewSession(NewSessionRequest {
            session_name: alpha.clone(),
            detached: true,
            size: Some(TerminalSize {
                cols: 120,
                rows: 40,
            }),
            environment: None,
        }))
        .await;
    assert!(matches!(created, Response::NewSession(_)));

    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let _attach_id = handler
        .register_attach(requester_pid, alpha, control_tx)
        .await;

    let response = handler
        .handle(Request::LockClient(rmux_proto::LockClientRequest {
            target_client: "=".to_owned(),
        }))
        .await;
    assert_eq!(
        response,
        Response::LockClient(rmux_proto::LockClientResponse {
            target_client: "=".to_owned(),
        })
    );

    match control_rx.recv().await {
        Some(AttachControl::LockShellCommand(command)) => assert!(!command.command().is_empty()),
        Some(other) => panic!("expected lock control, got {other:?}"),
        None => panic!("attach control closed"),
    }
}

#[tokio::test]
async fn lock_server_skips_already_suspended_clients() {
    let handler = RequestHandler::new();
    let alpha = session_name("alpha");

    let created = handler
        .handle(Request::NewSession(NewSessionRequest {
            session_name: alpha.clone(),
            detached: true,
            size: Some(TerminalSize { cols: 80, rows: 24 }),
            environment: None,
        }))
        .await;
    assert!(matches!(created, Response::NewSession(_)));

    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let _attach_id = handler
        .register_attach(std::process::id(), alpha, control_tx)
        .await;

    let first_lock = handler
        .handle(Request::LockServer(rmux_proto::LockServerRequest))
        .await;
    assert!(matches!(first_lock, Response::LockServer(_)));
    match control_rx.try_recv() {
        Ok(AttachControl::LockShellCommand(_)) => {}
        other => panic!("expected Lock control from first lock-server, got {other:?}"),
    }
    assert!(
        control_rx.try_recv().is_err(),
        "unexpected extra control message after first lock"
    );

    let second_lock = handler
        .handle(Request::LockServer(rmux_proto::LockServerRequest))
        .await;
    assert!(matches!(second_lock, Response::LockServer(_)));
    match control_rx.try_recv() {
        Err(TryRecvError::Empty) => {}
        Ok(msg) => panic!("suspended client must not receive a second lock, got {msg:?}"),
        Err(e) => panic!("unexpected channel state: {e:?}"),
    }
}

#[tokio::test]
async fn lock_session_for_nonexistent_session_returns_error() {
    let handler = RequestHandler::new();

    let response = handler
        .handle(Request::LockSession(rmux_proto::LockSessionRequest {
            target: session_name("nonexistent"),
        }))
        .await;
    assert!(matches!(response, Response::Error(_)));
}

#[tokio::test]
async fn kill_server_sets_shutdown_flag() {
    let handler = RequestHandler::new();

    let response = handler
        .handle(Request::KillServer(rmux_proto::KillServerRequest))
        .await;
    assert!(matches!(response, Response::KillServer(_)));

    assert!(
        handler.request_shutdown_if_pending(),
        "shutdown flag must be set after kill-server"
    );
    assert!(
        !handler.request_shutdown_if_pending(),
        "shutdown flag must be consumed after first check"
    );
}

#[tokio::test]
async fn server_access_protects_owner_uid() {
    let handler = RequestHandler::with_owner_uid(current_owner_uid());

    let response = handler
        .handle(Request::ServerAccess(rmux_proto::ServerAccessRequest {
            add: true,
            deny: false,
            list: false,
            read_only: false,
            write: false,
            user: Some(current_owner_uid().to_string()),
        }))
        .await;
    assert!(
        matches!(response, Response::Error(_)),
        "modifying the owner UID must fail"
    );
}

#[tokio::test]
async fn server_access_deny_nonexistent_user_returns_error() {
    let handler = RequestHandler::new();

    let response = handler
        .handle(Request::ServerAccess(rmux_proto::ServerAccessRequest {
            add: false,
            deny: true,
            list: false,
            read_only: false,
            write: false,
            user: Some("99999".to_owned()),
        }))
        .await;
    assert!(matches!(response, Response::Error(_)));
}

#[tokio::test]
async fn server_access_list_skips_uid_zero() {
    let handler = RequestHandler::with_owner_uid(1000);

    let response = handler
        .handle(Request::ServerAccess(rmux_proto::ServerAccessRequest {
            add: false,
            deny: false,
            list: true,
            read_only: false,
            write: false,
            user: None,
        }))
        .await;
    match response {
        Response::ServerAccess(ref access) => {
            let stdout = String::from_utf8_lossy(&access.output.stdout);
            assert!(
                !stdout.contains("root"),
                "UID 0 must be skipped in server-access -l output"
            );
        }
        _ => panic!("expected ServerAccess response"),
    }
}

#[tokio::test]
async fn server_access_mutual_exclusion_validated() {
    let handler = RequestHandler::new();

    let response = handler
        .handle(Request::ServerAccess(rmux_proto::ServerAccessRequest {
            add: true,
            deny: true,
            list: false,
            read_only: false,
            write: false,
            user: Some("alice".to_owned()),
        }))
        .await;
    assert!(
        matches!(response, Response::Error(_)),
        "-a and -d together must be rejected"
    );

    let response = handler
        .handle(Request::ServerAccess(rmux_proto::ServerAccessRequest {
            add: false,
            deny: false,
            list: false,
            read_only: true,
            write: true,
            user: Some("alice".to_owned()),
        }))
        .await;
    assert!(
        matches!(response, Response::Error(_)),
        "-r and -w together must be rejected"
    );
}
