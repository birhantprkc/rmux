use super::http::{path_from_target, HttpRequest};
use super::pre_auth::PreAuthQueue;
use super::serve_connection;
use crate::handler::RequestHandler;
use crate::web::crypto::{self, FrameOpener, FrameSealer};
use crate::web::protocol::WEB_SHARE_PROTOCOL_VERSION;
use crate::web::websocket::WebSocketMessage;
use crate::web::SecretHashForCrypto;
use rmux_proto::{
    CreateWebShareRequest, KillSessionRequest, NewSessionRequest, PaneTarget, Request, Response,
    SessionName, StopWebShareRequest, TerminalSize, WebShareCreatedResponse, WebShareRequest,
    WebShareResponse, WebShareScope,
};
use serde_json::Value;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{timeout, Duration};

#[test]
fn websocket_upgrade_requires_upgrade_token() {
    let request = request_with_headers([
        ("upgrade", "websocket"),
        ("connection", "keep-alive, Upgrade"),
    ]);
    assert!(request.is_websocket_upgrade());

    let request = request_with_headers([("upgrade", "websocket"), ("connection", "close")]);
    assert!(!request.is_websocket_upgrade());
}

#[test]
fn target_path_ignores_query_for_routing() {
    assert_eq!(path_from_target("/share?ignored=true"), "/share");
    assert_eq!(path_from_target("/assets/app.js"), "/assets/app.js");
}

#[tokio::test]
async fn non_websocket_http_paths_return_404() {
    for target in ["/", "/assets/app.js", "/index.html"] {
        let response = response_for(format!("GET {target} HTTP/1.1\r\nHost: local\r\n\r\n")).await;
        assert!(
            response.starts_with("HTTP/1.1 404 Not Found"),
            "{target}: {response}"
        );
    }
}

#[tokio::test]
async fn non_get_head_methods_return_405() {
    let response = response_for("POST /share HTTP/1.1\r\nHost: local\r\n\r\n").await;
    assert!(response.starts_with("HTTP/1.1 405 Method Not Allowed"));
}

#[tokio::test]
async fn pre_auth_queue_evicts_oldest_pending_connection() {
    let queue = PreAuthQueue::new(1);
    let mut first = queue.register();
    let mut second = queue.register();

    timeout(Duration::from_secs(1), first.evicted())
        .await
        .expect("oldest pending auth slot is evicted");
    assert!(
        timeout(Duration::from_millis(25), second.evicted())
            .await
            .is_err(),
        "newest pending auth slot remains open"
    );
}

#[tokio::test]
async fn pre_auth_fifo_eviction_closes_the_oldest_idle_connection() {
    let handler = Arc::new(RequestHandler::new());
    let queue = PreAuthQueue::new(1);
    let (mut first_client, first_task) = raw_connection(Arc::clone(&handler), queue.clone()).await;
    wait_for_pending_pre_auth(&queue, 1).await;

    let (mut second_client, second_task) = raw_connection(handler, queue).await;
    let mut byte = [0u8; 1];
    let read = timeout(Duration::from_secs(1), first_client.read(&mut byte))
        .await
        .expect("oldest connection should be closed")
        .expect("read oldest connection");
    assert_eq!(read, 0);

    second_client
        .write_all(b"GET / HTTP/1.1\r\nHost: local\r\n\r\n")
        .await
        .expect("write request");
    let response = read_http_response(&mut second_client).await;
    assert!(response.starts_with("HTTP/1.1 404 Not Found"));

    drop(first_client);
    drop(second_client);
    let _ = first_task.await.expect("first connection task joins");
    let _ = second_task.await.expect("second connection task joins");
}

#[tokio::test]
async fn share_websocket_upgrade_returns_101() {
    let request = concat!(
        "GET /share HTTP/1.1\r\n",
        "Host: local\r\n",
        "Connection: Upgrade\r\n",
        "Upgrade: websocket\r\n",
        "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
        "Sec-WebSocket-Version: 13\r\n",
        "\r\n"
    );
    let response = response_for(request).await;
    assert!(response.starts_with("HTTP/1.1 101 Switching Protocols"));
}

#[tokio::test]
async fn share_websocket_upgrade_requires_version_13_and_valid_key() {
    let missing_version = concat!(
        "GET /share HTTP/1.1\r\n",
        "Host: local\r\n",
        "Connection: Upgrade\r\n",
        "Upgrade: websocket\r\n",
        "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
        "\r\n"
    );
    let response = response_for(missing_version).await;
    assert!(response.starts_with("HTTP/1.1 400 Bad Request"));

    let invalid_key = concat!(
        "GET /share HTTP/1.1\r\n",
        "Host: local\r\n",
        "Connection: Upgrade\r\n",
        "Upgrade: websocket\r\n",
        "Sec-WebSocket-Key: Zm9v\r\n",
        "Sec-WebSocket-Version: 13\r\n",
        "\r\n"
    );
    let response = response_for(invalid_key).await;
    assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
}

#[tokio::test]
async fn share_websocket_auth_ready_snapshot_operator_and_revoke_loop() {
    let handler = Arc::new(RequestHandler::new());
    let session_name = create_session(&handler, "websocket-e2e").await;
    let created = create_share(
        &handler,
        CreateWebShareRequest {
            writable: true,
            ..share_request(WebShareScope::Pane(PaneTarget::new(session_name, 0).into()))
        },
    )
    .await;
    let mut client = TestWebSocket::connect(
        Arc::clone(&handler),
        &token_from_url(created.operator_url.as_deref().expect("operator URL")),
    )
    .await;
    let ready = client.read_json().await;
    assert_eq!(ready["type"], "ready");
    assert_eq!(
        ready["protocol_version"].as_u64(),
        Some(u64::from(WEB_SHARE_PROTOCOL_VERSION))
    );
    assert_eq!(ready["scope"], "pane");
    assert_eq!(ready["role"], "operator");
    assert_eq!(ready["writable"], true);
    assert_eq!(ready["show_viewers"], false);
    assert_eq!(ready["readers_active"], 0);
    assert_eq!(ready["readers_max"], 1);
    assert_eq!(ready["operator_connected"], true);
    assert_eq!(ready["viewers_connected"], 1);
    assert!(ready["capabilities"]
        .as_array()
        .expect("capabilities array")
        .iter()
        .any(|capability| capability == "e2ee-token-auth"));

    client.read_binary_with_prefix(0x10, "snapshot").await;

    client.send_binary(&[0x82, 0x00, 0x2c, 0x00, 0x24]).await;
    client.read_binary_with_prefix(0x02, "resize notify").await;
    client
        .read_binary_with_prefix(0x10, "resized snapshot")
        .await;

    client.send_binary(&[0x80, b'p', b'w', b'd', b'\n']).await;
    let stopped = handler
        .handle(Request::WebShare(WebShareRequest::Stop(
            StopWebShareRequest {
                share_id: created.share_id,
            },
        )))
        .await;
    assert!(matches!(
        stopped,
        Response::WebShare(WebShareResponse::Stopped(_))
    ));

    let revoked = client.read_json().await;
    assert_eq!(revoked["type"], "share_revoked");
    assert_eq!(revoked["reason"], "stopped_by_owner");

    client.close().await;
}

#[tokio::test]
async fn session_share_sends_revoked_before_closing_when_session_is_killed() {
    let handler = Arc::new(RequestHandler::new());
    let session_name = create_session(&handler, "websocket-session-gone").await;
    let created = create_share(
        &handler,
        share_request(WebShareScope::Session(session_name.clone())),
    )
    .await;
    let mut client =
        TestWebSocket::connect(Arc::clone(&handler), &token_from_url(&created.read_url)).await;
    let ready = client.read_json().await;
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["scope"], "session");

    let killed = handler
        .handle(Request::KillSession(KillSessionRequest {
            target: session_name,
            kill_all_except_target: false,
            clear_alerts: false,
        }))
        .await;
    assert!(matches!(killed, Response::KillSession(_)));

    let revoked = client.read_json().await;
    assert_eq!(revoked["type"], "share_revoked");
    assert_eq!(revoked["reason"], "session_gone");

    client.close().await;
}

fn request_with_headers<const N: usize>(headers: [(&str, &str); N]) -> HttpRequest {
    HttpRequest {
        method: "GET".to_owned(),
        path: "/share".to_owned(),
        headers: headers
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect::<HashMap<_, _>>(),
    }
}

async fn create_session(handler: &RequestHandler, name: &str) -> SessionName {
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
    let Response::WebShare(WebShareResponse::Created(created)) = response else {
        panic!("expected web share creation");
    };
    created
}

fn share_request(scope: WebShareScope) -> CreateWebShareRequest {
    CreateWebShareRequest {
        scope,
        public_base_url: Some("https://terminal.example".to_owned()),
        frontend_url: None,
        ttl_seconds: Some(60),
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

async fn response_for(request: impl AsRef<[u8]>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("listener addr");
    let client = TcpStream::connect(addr);
    let server = listener.accept();
    let (client, server) = tokio::join!(client, server);
    let mut client = client.expect("client connects");
    let (server, _) = server.expect("server accepts");
    let task = tokio::spawn(serve_connection(
        server,
        Arc::new(RequestHandler::new()),
        PreAuthQueue::new(16),
    ));

    client
        .write_all(request.as_ref())
        .await
        .expect("write request");
    let mut buffer = [0u8; 4096];
    let read = client.read(&mut buffer).await.expect("read response");
    drop(client);
    let _ = task.await.expect("connection task joins");
    String::from_utf8_lossy(&buffer[..read]).into_owned()
}

async fn websocket_client(
    handler: Arc<RequestHandler>,
) -> (TcpStream, tokio::task::JoinHandle<io::Result<()>>) {
    let (mut client, task) = raw_connection(handler, PreAuthQueue::new(16)).await;
    client
        .write_all(
            concat!(
                "GET /share HTTP/1.1\r\n",
                "Host: local\r\n",
                "Connection: Upgrade\r\n",
                "Upgrade: websocket\r\n",
                "Origin: https://share.rmux.io\r\n",
                "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
                "Sec-WebSocket-Version: 13\r\n",
                "\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("write upgrade request");
    let response = read_http_response(&mut client).await;
    assert!(
        response.starts_with("HTTP/1.1 101 Switching Protocols"),
        "{response}"
    );
    (client, task)
}

struct TestWebSocket {
    stream: TcpStream,
    task: tokio::task::JoinHandle<io::Result<()>>,
    opener: FrameOpener,
    sealer: FrameSealer,
}

impl TestWebSocket {
    async fn connect(handler: Arc<RequestHandler>, token: &str) -> Self {
        let secret = SecretHashForCrypto::from_secret(token);
        let token_id = secret.token_id();
        let (mut stream, task) = websocket_client(handler).await;
        write_client_text_frame(
            &mut stream,
            format!(
                r#"{{"type":"hello","protocol_version":{},"capabilities":["e2ee-token-auth","terminal-palette-v1"],"token_id":"{}","client_nonce":"{}"}}"#,
                WEB_SHARE_PROTOCOL_VERSION, token_id, TEST_CLIENT_NONCE
            )
            .as_bytes(),
        )
        .await;

        let challenge = read_server_frame(&mut stream).await;
        assert_eq!(challenge.opcode, OPCODE_TEXT);
        let challenge: Value =
            serde_json::from_slice(&challenge.payload).expect("challenge is json");
        assert_eq!(challenge["type"], "challenge");
        let server_nonce = challenge["server_nonce"]
            .as_str()
            .expect("challenge has server nonce");
        let (opener, mut sealer) = crypto::derive_client_crypto_for_test(
            secret,
            &token_id,
            TEST_CLIENT_NONCE,
            server_nonce,
        )
        .expect("client crypto");
        write_client_binary_frame(
            &mut stream,
            &sealer.seal_text(auth_text().as_bytes()).expect("seal auth"),
        )
        .await;

        Self {
            stream,
            task,
            opener,
            sealer,
        }
    }

    async fn read_json(&mut self) -> Value {
        serde_json::from_str(&read_encrypted_text(&mut self.stream, &mut self.opener).await)
            .expect("encrypted text frame json")
    }

    async fn read_binary_with_prefix(&mut self, prefix: u8, label: &str) {
        read_encrypted_binary_frame_with_prefix(&mut self.stream, &mut self.opener, prefix, label)
            .await;
    }

    async fn send_binary(&mut self, payload: &[u8]) {
        write_client_binary_frame(
            &mut self.stream,
            &self.sealer.seal_binary(payload).expect("seal binary"),
        )
        .await;
    }

    async fn close(self) {
        drop(self.stream);
        let _ = self.task.await.expect("server task joins");
    }
}

fn auth_text() -> String {
    format!(
        r#"{{"type":"auth","protocol_version":{},"capabilities":["e2ee-token-auth","terminal-palette-v1"]}}"#,
        WEB_SHARE_PROTOCOL_VERSION
    )
}

async fn raw_connection(
    handler: Arc<RequestHandler>,
    pre_auth: PreAuthQueue,
) -> (TcpStream, tokio::task::JoinHandle<io::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("listener addr");
    let client = TcpStream::connect(addr);
    let server = listener.accept();
    let (client, server) = tokio::join!(client, server);
    let client = client.expect("client connects");
    let (server, _) = server.expect("server accepts");
    let task = tokio::spawn(serve_connection(server, handler, pre_auth));
    (client, task)
}

async fn wait_for_pending_pre_auth(queue: &PreAuthQueue, expected: usize) {
    timeout(Duration::from_secs(1), async {
        while queue.pending_count() != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("pre-auth queue reached expected size");
}

async fn read_http_response(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        timeout(Duration::from_secs(2), stream.read_exact(&mut byte))
            .await
            .expect("HTTP response timeout")
            .expect("read HTTP response byte");
        buffer.push(byte[0]);
        if buffer.ends_with(b"\r\n\r\n") {
            return String::from_utf8_lossy(&buffer).into_owned();
        }
    }
}

async fn write_client_text_frame(stream: &mut TcpStream, payload: &[u8]) {
    write_client_frame(stream, OPCODE_TEXT, payload).await;
}

async fn write_client_binary_frame(stream: &mut TcpStream, payload: &[u8]) {
    write_client_frame(stream, OPCODE_BINARY, payload).await;
}

async fn write_client_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) {
    let mask = [0x12, 0x34, 0x56, 0x78];
    let mut frame = Vec::with_capacity(14 + payload.len());
    frame.push(0x80 | opcode);
    push_client_frame_len(&mut frame, payload.len());
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % mask.len()]),
    );
    stream
        .write_all(&frame)
        .await
        .expect("write websocket frame");
}

fn push_client_frame_len(frame: &mut Vec<u8>, len: usize) {
    if len < 126 {
        frame.push(0x80 | len as u8);
    } else if u16::try_from(len).is_ok() {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
}

async fn read_encrypted_text(stream: &mut TcpStream, opener: &mut FrameOpener) -> String {
    loop {
        let frame = read_server_frame(stream).await;
        match frame.opcode {
            OPCODE_BINARY => {
                if let WebSocketMessage::Text(text) = opener
                    .open_message(&frame.payload)
                    .expect("encrypted server text opens")
                {
                    return text;
                }
            }
            OPCODE_CLOSE => panic!("websocket closed before encrypted text frame"),
            opcode => panic!("unexpected websocket opcode {opcode} before encrypted text frame"),
        }
    }
}

async fn read_encrypted_binary_frame_with_prefix(
    stream: &mut TcpStream,
    opener: &mut FrameOpener,
    prefix: u8,
    label: &str,
) {
    for _ in 0..8 {
        let frame = read_server_frame(stream).await;
        match frame.opcode {
            OPCODE_BINARY => {
                if let WebSocketMessage::Binary(payload) = opener
                    .open_message(&frame.payload)
                    .expect("encrypted server binary opens")
                {
                    if payload.first() == Some(&prefix) {
                        return;
                    }
                }
            }
            OPCODE_CLOSE => panic!("websocket closed before {label} frame"),
            opcode => panic!("unexpected websocket opcode {opcode} before {label} frame"),
        }
    }
    panic!("did not receive {label} frame");
}

async fn read_server_frame(stream: &mut TcpStream) -> ServerFrame {
    timeout(Duration::from_secs(2), read_server_frame_inner(stream))
        .await
        .expect("websocket frame timeout")
        .expect("read websocket frame")
}

async fn read_server_frame_inner(stream: &mut TcpStream) -> io::Result<ServerFrame> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await?;
    let opcode = head[0] & 0x0f;
    let masked = head[1] & 0x80 != 0;
    assert!(!masked, "server frames must not be masked");
    let mut len = u64::from(head[1] & 0x7f);
    if len == 126 {
        let mut bytes = [0u8; 2];
        stream.read_exact(&mut bytes).await?;
        len = u64::from(u16::from_be_bytes(bytes));
    } else if len == 127 {
        let mut bytes = [0u8; 8];
        stream.read_exact(&mut bytes).await?;
        len = u64::from_be_bytes(bytes);
    }
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).await?;
    Ok(ServerFrame { opcode, payload })
}

struct ServerFrame {
    opcode: u8,
    payload: Vec<u8>,
}

fn token_from_url(url: &str) -> String {
    url.split_once("#")
        .and_then(|(_, fragment)| {
            fragment.split('&').find_map(|param| {
                let (key, value) = param.split_once('=')?;
                (key == "t").then_some(value.to_owned())
            })
        })
        .expect("URL contains access token")
}

const OPCODE_TEXT: u8 = 0x1;
const OPCODE_BINARY: u8 = 0x2;
const OPCODE_CLOSE: u8 = 0x8;
const TEST_CLIENT_NONCE: &str = "AQIDBAUGBwgJCgsMDQ4PEA";
