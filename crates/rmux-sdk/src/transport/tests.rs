use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use rmux_proto::{
    encode_frame, ErrorResponse, HasSessionRequest, HasSessionResponse, ListSessionsRequest,
    ListSessionsResponse, Request, Response, SessionName,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};

use super::{DropGuard, TransportClient};
use crate::RmuxError;

fn alpha() -> SessionName {
    SessionName::new("alpha").expect("valid session")
}

fn has_session_request() -> Request {
    Request::HasSession(HasSessionRequest { target: alpha() })
}

fn list_sessions_request() -> Request {
    Request::ListSessions(ListSessionsRequest {
        format: None,
        filter: None,
        sort_order: None,
        reversed: false,
    })
}

fn has_session_response(exists: bool) -> Response {
    Response::HasSession(HasSessionResponse { exists })
}

fn list_sessions_response(stdout: &[u8]) -> Response {
    Response::ListSessions(ListSessionsResponse {
        output: rmux_proto::CommandOutput::from_stdout(stdout),
    })
}

fn session_not_found_response() -> Response {
    Response::Error(ErrorResponse {
        error: rmux_proto::RmuxError::SessionNotFound("alpha".to_owned()),
    })
}

async fn read_request(stream: &mut tokio::io::DuplexStream) -> Request {
    let mut decoder = rmux_proto::FrameDecoder::new();
    let mut buffer = [0; 256];
    loop {
        if let Some(request) = decoder
            .next_frame::<Request>()
            .expect("request frame decodes")
        {
            return request;
        }
        let read = stream.read(&mut buffer).await.expect("read request bytes");
        assert_ne!(read, 0, "client closed before request arrived");
        decoder.push_bytes(&buffer[..read]);
    }
}

async fn write_response(stream: &mut tokio::io::DuplexStream, response: &Response) {
    let frame = encode_frame(response).expect("response encodes");
    stream.write_all(&frame).await.expect("write response");
    stream.flush().await.expect("flush response");
}

fn assert_transport_kind(result: crate::Result<Response>, expected: io::ErrorKind) {
    match result.expect_err("request must fail") {
        RmuxError::Transport { source, .. } => assert_eq!(source.kind(), expected),
        error => panic!("expected transport error, got {error:?}"),
    }
}

fn assert_transport_message(
    result: crate::Result<Response>,
    expected: io::ErrorKind,
    expected_message: &str,
) {
    match result.expect_err("request must fail") {
        RmuxError::Transport { source, .. } => {
            assert_eq!(source.kind(), expected);
            assert!(
                source.to_string().contains(expected_message),
                "transport source `{source}` must contain `{expected_message}`"
            );
        }
        error => panic!("expected transport error, got {error:?}"),
    }
}

fn spawn_request(
    client: &TransportClient,
    request: Request,
) -> JoinHandle<crate::Result<Response>> {
    let client = client.clone();
    tokio::spawn(async move { client.request(request).await })
}

async fn join_request(handle: JoinHandle<crate::Result<Response>>) -> crate::Result<Response> {
    handle.await.expect("request task must not panic")
}

#[tokio::test]
async fn actor_correlates_bare_responses_in_fifo_request_order() {
    let (client_stream, mut server_stream) = tokio::io::duplex(4096);
    let client = TransportClient::spawn(client_stream);

    let first = spawn_request(&client, has_session_request());
    assert_eq!(
        read_request(&mut server_stream).await,
        has_session_request()
    );

    let second = spawn_request(&client, list_sessions_request());
    assert_eq!(
        read_request(&mut server_stream).await,
        list_sessions_request()
    );

    write_response(&mut server_stream, &has_session_response(true)).await;
    write_response(&mut server_stream, &list_sessions_response(b"alpha\n")).await;

    let (first, second) = tokio::join!(join_request(first), join_request(second));
    assert_eq!(first.expect("first response"), has_session_response(true));
    assert_eq!(
        second.expect("second response"),
        list_sessions_response(b"alpha\n")
    );
}

#[tokio::test]
async fn actor_rejects_out_of_order_response_kinds_and_closes_transport() {
    let (client_stream, mut server_stream) = tokio::io::duplex(4096);
    let client = TransportClient::spawn(client_stream);

    let first = spawn_request(&client, has_session_request());
    assert_eq!(
        read_request(&mut server_stream).await,
        has_session_request()
    );

    let second = spawn_request(&client, list_sessions_request());
    assert_eq!(
        read_request(&mut server_stream).await,
        list_sessions_request()
    );

    write_response(&mut server_stream, &list_sessions_response(b"alpha\n")).await;

    let (first, second) = tokio::join!(join_request(first), join_request(second));
    assert_transport_message(
        first,
        io::ErrorKind::InvalidData,
        "sent `list-sessions` response for pending `has-session` request",
    );
    assert_transport_kind(second, io::ErrorKind::InvalidData);
    assert_transport_kind(
        client.request(has_session_request()).await,
        io::ErrorKind::InvalidData,
    );
}

#[tokio::test]
async fn error_response_completes_current_fifo_slot_as_protocol_error() {
    let (client_stream, mut server_stream) = tokio::io::duplex(4096);
    let client = TransportClient::spawn(client_stream);

    let request = spawn_request(&client, has_session_request());
    assert_eq!(
        read_request(&mut server_stream).await,
        has_session_request()
    );
    write_response(&mut server_stream, &session_not_found_response()).await;

    match join_request(request).await.expect_err("request must fail") {
        RmuxError::Protocol { source } => {
            assert_eq!(
                source,
                rmux_proto::RmuxError::SessionNotFound("alpha".to_owned())
            );
        }
        error => panic!("expected protocol error, got {error:?}"),
    }
}

#[tokio::test]
async fn unsolicited_response_without_pending_request_closes_transport() {
    let (client_stream, mut server_stream) = tokio::io::duplex(4096);
    let client = TransportClient::spawn(client_stream);

    write_response(&mut server_stream, &has_session_response(true)).await;
    timeout(Duration::from_secs(1), async {
        while client.state.terminal_failure().is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actor must reject unsolicited response before the next request");

    assert_transport_message(
        client.request(has_session_request()).await,
        io::ErrorKind::InvalidData,
        "sent unsolicited `has-session` response",
    );
}

#[tokio::test]
async fn actor_wakes_every_pending_caller_on_eof() {
    let (client_stream, mut server_stream) = tokio::io::duplex(4096);
    let client = TransportClient::spawn(client_stream);

    let first = spawn_request(&client, has_session_request());
    assert_eq!(
        read_request(&mut server_stream).await,
        has_session_request()
    );

    let second = spawn_request(&client, list_sessions_request());
    assert_eq!(
        read_request(&mut server_stream).await,
        list_sessions_request()
    );
    drop(server_stream);

    let (first, second) = tokio::join!(join_request(first), join_request(second));
    assert_transport_kind(first, io::ErrorKind::UnexpectedEof);
    assert_transport_kind(second, io::ErrorKind::UnexpectedEof);
    assert_transport_kind(
        client.request(has_session_request()).await,
        io::ErrorKind::UnexpectedEof,
    );
}

#[tokio::test]
async fn actor_wakes_every_pending_caller_on_bad_frame() {
    let (client_stream, mut server_stream) = tokio::io::duplex(4096);
    let client = TransportClient::spawn(client_stream);

    let first = spawn_request(&client, has_session_request());
    assert_eq!(
        read_request(&mut server_stream).await,
        has_session_request()
    );

    let second = spawn_request(&client, list_sessions_request());
    assert_eq!(
        read_request(&mut server_stream).await,
        list_sessions_request()
    );
    server_stream
        .write_all(&[0])
        .await
        .expect("write invalid frame byte");

    let (first, second) = tokio::join!(join_request(first), join_request(second));
    assert_transport_kind(first, io::ErrorKind::InvalidData);
    assert_transport_kind(second, io::ErrorKind::InvalidData);
}

#[tokio::test]
async fn actor_wakes_every_pending_caller_on_read_error() {
    let client = TransportClient::spawn(ScriptedIo::read_error_after_writes(2));

    let first = client.request(has_session_request());
    let second = client.request(list_sessions_request());

    let (first, second) = tokio::join!(first, second);
    assert_transport_kind(first, io::ErrorKind::ConnectionReset);
    assert_transport_kind(second, io::ErrorKind::ConnectionReset);
    assert_transport_kind(
        client.request(has_session_request()).await,
        io::ErrorKind::ConnectionReset,
    );
}

#[tokio::test]
async fn actor_wakes_every_pending_caller_on_write_error() {
    let client = TransportClient::spawn(ScriptedIo::write_error_on_call(2));

    let first = client.request(has_session_request());
    let second = client.request(list_sessions_request());

    let (first, second) = tokio::join!(first, second);
    assert_transport_kind(first, io::ErrorKind::BrokenPipe);
    assert_transport_kind(second, io::ErrorKind::BrokenPipe);
    assert_transport_kind(
        client.request(has_session_request()).await,
        io::ErrorKind::BrokenPipe,
    );
}

#[tokio::test]
async fn terminal_read_error_before_request_write_is_reported_explicitly() {
    let client = TransportClient::spawn(ScriptedIo::read_error_after_writes(0));

    assert_transport_kind(
        client.request(has_session_request()).await,
        io::ErrorKind::ConnectionReset,
    );
    assert_transport_kind(
        client.request(list_sessions_request()).await,
        io::ErrorKind::ConnectionReset,
    );
}

#[tokio::test]
async fn drop_guard_uses_nonblocking_best_effort_actor_send() {
    let (client_stream, mut server_stream) = tokio::io::duplex(4096);
    let client = TransportClient::spawn(client_stream);

    drop(DropGuard::best_effort(
        client.clone(),
        has_session_request(),
    ));

    assert_eq!(
        read_request(&mut server_stream).await,
        has_session_request()
    );
    write_response(&mut server_stream, &has_session_response(true)).await;

    let follow_up = spawn_request(&client, list_sessions_request());
    assert_eq!(
        read_request(&mut server_stream).await,
        list_sessions_request()
    );
    write_response(&mut server_stream, &list_sessions_response(b"alpha\n")).await;

    assert_eq!(
        join_request(follow_up).await.expect("follow-up response"),
        list_sessions_response(b"alpha\n")
    );
}

#[derive(Clone, Copy)]
enum Script {
    ReadErrorAfterWrites { writes: usize },
    WriteErrorOnCall { call: usize },
}

struct ScriptedIo {
    script: Script,
    state: Arc<Mutex<ScriptedIoState>>,
}

#[derive(Default)]
struct ScriptedIoState {
    write_calls: usize,
    read_waker: Option<Waker>,
}

impl ScriptedIo {
    fn read_error_after_writes(writes: usize) -> Self {
        Self {
            script: Script::ReadErrorAfterWrites { writes },
            state: Arc::new(Mutex::new(ScriptedIoState::default())),
        }
    }

    fn write_error_on_call(call: usize) -> Self {
        Self {
            script: Script::WriteErrorOnCall { call },
            state: Arc::new(Mutex::new(ScriptedIoState::default())),
        }
    }
}

impl AsyncRead for ScriptedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        _buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut state = self.state.lock().expect("scripted state lock");
        match self.script {
            Script::ReadErrorAfterWrites { writes } if state.write_calls >= writes => {
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "scripted read error",
                )))
            }
            _ => {
                state.read_waker = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl AsyncWrite for ScriptedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.state.lock().expect("scripted state lock");
        state.write_calls += 1;
        if matches!(self.script, Script::WriteErrorOnCall { call } if state.write_calls == call) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "scripted write error",
            )));
        }
        if let Some(waker) = state.read_waker.take() {
            waker.wake();
        }
        Poll::Ready(Ok(buffer.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
