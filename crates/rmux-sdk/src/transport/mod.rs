//! Crate-private Tokio transport actor for detached SDK RPC.

use std::collections::hash_map::RandomState;
use std::collections::VecDeque;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rmux_proto::{encode_frame, FrameDecoder, Request, Response, SdkWaitId, SdkWaitOwnerId};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::{Result, RmuxError};

const ACTOR_QUEUE_CAPACITY: usize = 64;
const READ_BUFFER_SIZE: usize = 8192;
const TRANSPORT_SHUTDOWN_OPERATION: &str = "shut down rmux SDK transport";
static NEXT_SDK_WAIT_OWNER_ID: AtomicU64 = AtomicU64::new(1);
static SDK_WAIT_OWNER_PROCESS_SEED: OnceLock<u64> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct TransportClient {
    commands: mpsc::Sender<ActorMessage>,
    state: Arc<TransportState>,
}

impl TransportClient {
    pub(crate) fn spawn<S>(stream: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (commands, receiver) = mpsc::channel(ACTOR_QUEUE_CAPACITY);
        let state = Arc::new(TransportState::default());
        tokio::spawn(run_actor(stream, receiver, state.clone()));
        Self { commands, state }
    }

    pub(crate) async fn request(&self, request: Request) -> Result<Response> {
        let operation = request_operation(&request);
        if let Some(failure) = self.state.terminal_failure() {
            return Err(failure.to_error(&operation));
        }

        let (reply, response) = oneshot::channel();
        self.commands
            .send(ActorMessage::Request {
                request,
                operation: operation.clone(),
                reply,
            })
            .await
            .map_err(|_| self.closed_error(&operation))?;

        response.await.map_err(|_| self.closed_error(&operation))?
    }

    pub(crate) async fn armed_request(&self, request: Request) -> Result<PendingResponse> {
        let operation = request_operation(&request);
        if let Some(failure) = self.state.terminal_failure() {
            return Err(failure.to_error(&operation));
        }

        let (reply, response) = oneshot::channel();
        let (armed, armed_response) = oneshot::channel();
        self.commands
            .send(ActorMessage::ArmedRequest {
                request,
                operation: operation.clone(),
                reply,
                armed,
            })
            .await
            .map_err(|_| self.closed_error(&operation))?;

        armed_response
            .await
            .map_err(|_| self.closed_error(&operation))?
            .map_err(|failure| failure.to_error(&operation))?;

        Ok(PendingResponse {
            operation,
            response,
        })
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        if let Some(failure) = self.state.terminal_failure() {
            if failure.is_eof() {
                return Ok(());
            }
            return Err(failure.to_error(TRANSPORT_SHUTDOWN_OPERATION));
        }

        let (reply, response) = oneshot::channel();
        self.commands
            .send(ActorMessage::Shutdown { reply })
            .await
            .map_err(|_| self.closed_error(TRANSPORT_SHUTDOWN_OPERATION))?;

        response
            .await
            .map_err(|_| self.closed_error(TRANSPORT_SHUTDOWN_OPERATION))?
    }

    fn try_send_best_effort(&self, request: Request) {
        if self.state.terminal_failure().is_some() {
            return;
        }

        let _ = self.commands.try_send(ActorMessage::BestEffort { request });
    }

    pub(crate) fn sdk_wait_owner_id(&self) -> SdkWaitOwnerId {
        self.state.sdk_wait_owner_id
    }

    pub(crate) fn allocate_sdk_wait_id(&self) -> SdkWaitId {
        let id = allocate_bounded_atomic_id(
            &self.state.next_sdk_wait_id,
            u64::MAX,
            "SDK wait id space exhausted for transport",
        );
        SdkWaitId::new(id)
    }

    fn closed_error(&self, operation: &str) -> RmuxError {
        self.state
            .terminal_failure()
            .unwrap_or_else(TransportFailure::actor_closed)
            .to_error(operation)
    }
}

impl fmt::Debug for TransportClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransportClient")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
pub(crate) struct DropGuard {
    action: DropAction,
}

impl DropGuard {
    pub(crate) fn noop() -> Self {
        Self {
            action: DropAction::None,
        }
    }

    pub(crate) fn best_effort(client: TransportClient, request: Request) -> Self {
        Self {
            action: DropAction::BestEffort {
                client,
                request: Some(Box::new(request)),
            },
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.action = DropAction::None;
    }

    pub(crate) fn trigger(&mut self) {
        if let DropAction::BestEffort { client, request } = &mut self.action {
            if let Some(request) = request.take() {
                client.try_send_best_effort(*request);
            }
        }
        self.action = DropAction::None;
    }
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        self.trigger();
    }
}

#[derive(Debug, Default)]
enum DropAction {
    #[default]
    None,
    BestEffort {
        client: TransportClient,
        request: Option<Box<Request>>,
    },
}

enum ActorMessage {
    Request {
        request: Request,
        operation: String,
        reply: oneshot::Sender<Result<Response>>,
    },
    ArmedRequest {
        request: Request,
        operation: String,
        reply: oneshot::Sender<Result<Response>>,
        armed: oneshot::Sender<core::result::Result<(), TransportFailure>>,
    },
    BestEffort {
        request: Request,
    },
    Shutdown {
        reply: oneshot::Sender<Result<()>>,
    },
}

enum ActorEvent {
    Command(ActorMessage),
    CommandsClosed,
    Response(core::result::Result<Response, TransportFailure>),
}

pub(crate) struct PendingResponse {
    operation: String,
    response: oneshot::Receiver<Result<Response>>,
}

impl std::future::Future for PendingResponse {
    type Output = Result<Response>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match std::pin::Pin::new(&mut self.response).poll(cx) {
            std::task::Poll::Ready(Ok(result)) => std::task::Poll::Ready(result),
            std::task::Poll::Ready(Err(_)) => std::task::Poll::Ready(Err(
                TransportFailure::actor_closed().to_error(&self.operation),
            )),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

struct PendingCall {
    command_name: &'static str,
    operation: String,
    reply: Option<oneshot::Sender<Result<Response>>>,
}

impl PendingCall {
    fn reply(
        command_name: &'static str,
        operation: String,
        reply: oneshot::Sender<Result<Response>>,
    ) -> Self {
        Self {
            command_name,
            operation,
            reply: Some(reply),
        }
    }

    fn discard(command_name: &'static str, operation: String) -> Self {
        Self {
            command_name,
            operation,
            reply: None,
        }
    }

    fn validate_response(&self, response: &Response) -> core::result::Result<(), TransportFailure> {
        if response.is_error() {
            return Ok(());
        }

        let actual = response.command_name();
        if self.command_name == actual {
            return Ok(());
        }

        // The pane-output cursor endpoint is the one daemon RPC that resolves
        // to two distinct response variants: a regular cursor batch
        // (`pane-output-cursor`) or a lag notice (`pane-output-lag`) when the
        // server-side receiver detected a sequence gap. Both are valid
        // replies for the same `pane-output-cursor` request.
        if self.command_name == "pane-output-cursor" && actual == "pane-output-lag" {
            return Ok(());
        }

        Err(TransportFailure::mismatched_response(
            self.command_name,
            actual,
        ))
    }

    fn complete(self, response: Response) {
        if let Some(reply) = self.reply {
            let _ = reply.send(response_to_result(response));
        }
    }

    fn fail(self, failure: &TransportFailure) {
        if let Some(reply) = self.reply {
            let error = failure.to_error_for_command(&self.operation, self.command_name);
            let _ = reply.send(Err(error));
        }
    }
}

async fn run_actor<S>(stream: S, commands: mpsc::Receiver<ActorMessage>, state: Arc<TransportState>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let (events, mut event_receiver) = mpsc::channel(ACTOR_QUEUE_CAPACITY * 2);
    let command_task = tokio::spawn(forward_commands(commands, events.clone()));
    let read_task = tokio::spawn(forward_responses(reader, events));
    let mut pending = VecDeque::new();
    let mut commands_closed = false;
    let mut shutdown_reply = None;

    while let Some(event) = event_receiver.recv().await {
        match event {
            ActorEvent::Command(message) => {
                if shutdown_reply.is_some() {
                    reject_command_after_shutdown(message);
                    continue;
                }

                match message {
                    ActorMessage::Request {
                        request,
                        operation,
                        reply,
                    } => {
                        let command_name = request.command_name();
                        let frame = match encode_request(&request) {
                            Ok(frame) => frame,
                            Err(failure) => {
                                let _ = reply.send(Err(failure.to_error(&operation)));
                                continue;
                            }
                        };
                        pending.push_back(PendingCall::reply(command_name, operation, reply));
                        if let Err(failure) = write_frame(&mut writer, &frame).await {
                            fail_transport(&mut pending, &state, failure);
                            break;
                        }
                    }
                    ActorMessage::ArmedRequest {
                        request,
                        operation,
                        reply,
                        armed,
                    } => {
                        let command_name = request.command_name();
                        let frame = match encode_request(&request) {
                            Ok(frame) => frame,
                            Err(failure) => {
                                let _ = reply.send(Err(failure.to_error(&operation)));
                                let _ = armed.send(Err(failure));
                                continue;
                            }
                        };
                        pending.push_back(PendingCall::reply(
                            command_name,
                            operation.clone(),
                            reply,
                        ));
                        if let Err(failure) = write_frame(&mut writer, &frame).await {
                            let _ = armed.send(Err(failure.clone()));
                            fail_transport(&mut pending, &state, failure);
                            break;
                        }
                        let _ = armed.send(Ok(()));
                    }
                    ActorMessage::BestEffort { request } => {
                        let command_name = request.command_name();
                        let Ok(frame) = encode_request(&request) else {
                            continue;
                        };
                        pending.push_back(PendingCall::discard(
                            command_name,
                            request_operation(&request),
                        ));
                        if let Err(failure) = write_frame(&mut writer, &frame).await {
                            fail_transport(&mut pending, &state, failure);
                            break;
                        }
                    }
                    ActorMessage::Shutdown { reply } => {
                        match writer.shutdown().await.map_err(TransportFailure::io) {
                            Ok(()) => {
                                shutdown_reply = Some(reply);
                            }
                            Err(failure) => {
                                let _ =
                                    reply.send(Err(failure.to_error(TRANSPORT_SHUTDOWN_OPERATION)));
                                fail_transport(&mut pending, &state, failure);
                                break;
                            }
                        }
                    }
                }
            }
            ActorEvent::CommandsClosed => {
                commands_closed = true;
            }
            ActorEvent::Response(result) => match result {
                Ok(response) => {
                    let Some(pending_call) = pending.pop_front() else {
                        let failure = TransportFailure::unsolicited_response(&response);
                        fail_shutdown(&mut shutdown_reply, &failure);
                        fail_transport(&mut pending, &state, failure);
                        break;
                    };
                    if let Err(failure) = pending_call.validate_response(&response) {
                        pending_call.fail(&failure);
                        fail_shutdown(&mut shutdown_reply, &failure);
                        fail_transport(&mut pending, &state, failure);
                        break;
                    }
                    pending_call.complete(response);
                }
                Err(failure) => {
                    if shutdown_reply.is_some() && pending.is_empty() && failure.is_eof() {
                        complete_shutdown(&mut shutdown_reply);
                        break;
                    }

                    fail_shutdown(&mut shutdown_reply, &failure);
                    fail_transport(&mut pending, &state, failure);
                    break;
                }
            },
        }

        if commands_closed && pending.is_empty() && shutdown_reply.is_none() {
            let _ = writer.shutdown().await;
            break;
        }
    }

    command_task.abort();
    read_task.abort();
}

fn reject_command_after_shutdown(message: ActorMessage) {
    match message {
        ActorMessage::Request {
            operation, reply, ..
        } => {
            let failure = TransportFailure::actor_closed();
            let _ = reply.send(Err(failure.to_error(&operation)));
        }
        ActorMessage::ArmedRequest {
            operation,
            reply,
            armed,
            ..
        } => {
            let failure = TransportFailure::actor_closed();
            let _ = reply.send(Err(failure.to_error(&operation)));
            let _ = armed.send(Err(failure));
        }
        ActorMessage::BestEffort { .. } => {}
        ActorMessage::Shutdown { reply } => {
            let failure = TransportFailure::actor_closed();
            let _ = reply.send(Err(failure.to_error(TRANSPORT_SHUTDOWN_OPERATION)));
        }
    }
}

fn complete_shutdown(reply: &mut Option<oneshot::Sender<Result<()>>>) {
    if let Some(reply) = reply.take() {
        let _ = reply.send(Ok(()));
    }
}

fn fail_shutdown(reply: &mut Option<oneshot::Sender<Result<()>>>, failure: &TransportFailure) {
    if let Some(reply) = reply.take() {
        let _ = reply.send(Err(failure.to_error(TRANSPORT_SHUTDOWN_OPERATION)));
    }
}

async fn forward_commands(
    mut commands: mpsc::Receiver<ActorMessage>,
    events: mpsc::Sender<ActorEvent>,
) {
    while let Some(message) = commands.recv().await {
        if events.send(ActorEvent::Command(message)).await.is_err() {
            return;
        }
    }

    let _ = events.send(ActorEvent::CommandsClosed).await;
}

async fn forward_responses<R>(mut reader: R, events: mpsc::Sender<ActorEvent>)
where
    R: AsyncRead + Unpin,
{
    let mut decoder = FrameDecoder::new();
    loop {
        let result = read_response(&mut reader, &mut decoder).await;
        let stop = result.is_err();
        if events.send(ActorEvent::Response(result)).await.is_err() {
            return;
        }
        if stop {
            return;
        }
    }
}

fn encode_request(request: &Request) -> core::result::Result<Vec<u8>, TransportFailure> {
    encode_frame(request).map_err(TransportFailure::frame)
}

async fn write_frame<W>(writer: &mut W, frame: &[u8]) -> core::result::Result<(), TransportFailure>
where
    W: AsyncWrite + Unpin,
{
    writer
        .write_all(frame)
        .await
        .map_err(TransportFailure::io)?;
    writer.flush().await.map_err(TransportFailure::io)
}

async fn read_response<R>(
    reader: &mut R,
    decoder: &mut FrameDecoder,
) -> core::result::Result<Response, TransportFailure>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0; READ_BUFFER_SIZE];
    loop {
        if let Some(response) = decoder
            .next_frame::<Response>()
            .map_err(TransportFailure::frame)?
        {
            return Ok(response);
        }

        let read = reader
            .read(&mut buffer)
            .await
            .map_err(TransportFailure::io)?;
        if read == 0 {
            return Err(TransportFailure::eof());
        }
        decoder.push_bytes(&buffer[..read]);
    }
}

fn response_to_result(response: Response) -> Result<Response> {
    match response {
        Response::Error(error) => Err(error.into()),
        response => Ok(response),
    }
}

fn fail_all(pending: &mut VecDeque<PendingCall>, failure: &TransportFailure) {
    while let Some(call) = pending.pop_front() {
        call.fail(failure);
    }
}

fn fail_transport(
    pending: &mut VecDeque<PendingCall>,
    state: &TransportState,
    failure: TransportFailure,
) {
    state.set_terminal_failure(failure.clone());
    fail_all(pending, &failure);
}

fn request_operation(request: &Request) -> String {
    format!(
        "complete `{}` request/response exchange with rmux daemon",
        request.command_name()
    )
}

#[derive(Debug)]
struct TransportState {
    terminal_failure: Mutex<Option<TransportFailure>>,
    sdk_wait_owner_id: SdkWaitOwnerId,
    next_sdk_wait_id: AtomicU64,
}

impl Default for TransportState {
    fn default() -> Self {
        Self {
            terminal_failure: Mutex::new(None),
            sdk_wait_owner_id: allocate_sdk_wait_owner_id(),
            next_sdk_wait_id: AtomicU64::new(1),
        }
    }
}

fn allocate_sdk_wait_owner_id() -> SdkWaitOwnerId {
    let local_id = allocate_bounded_atomic_id(
        &NEXT_SDK_WAIT_OWNER_ID,
        u64::MAX - 1,
        "SDK wait owner id space exhausted for process",
    );
    SdkWaitOwnerId::new(mix_sdk_wait_owner_id(
        sdk_wait_owner_process_seed(),
        local_id,
    ))
}

fn sdk_wait_owner_process_seed() -> u64 {
    *SDK_WAIT_OWNER_PROCESS_SEED.get_or_init(|| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(now as u64);
        hasher.write_u64((now >> 64) as u64);
        hasher.write_u32(std::process::id());
        splitmix64(hasher.finish())
    })
}

fn mix_sdk_wait_owner_id(process_seed: u64, local_id: u64) -> u64 {
    let mixed = splitmix64(process_seed ^ local_id.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    if mixed == 0 {
        1
    } else {
        mixed
    }
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

fn allocate_bounded_atomic_id(
    counter: &AtomicU64,
    max_inclusive: u64,
    exhausted_message: &'static str,
) -> u64 {
    loop {
        let current = counter.load(Ordering::Relaxed);
        assert!(current <= max_inclusive, "{exhausted_message}");
        let next = current.checked_add(1).expect(exhausted_message);
        if counter
            .compare_exchange(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return current;
        }
    }
}

impl TransportState {
    fn terminal_failure(&self) -> Option<TransportFailure> {
        self.lock_terminal_failure().clone()
    }

    fn set_terminal_failure(&self, failure: TransportFailure) {
        let mut terminal_failure = self.lock_terminal_failure();
        if terminal_failure.is_none() {
            *terminal_failure = Some(failure);
        }
    }

    fn lock_terminal_failure(&self) -> std::sync::MutexGuard<'_, Option<TransportFailure>> {
        self.terminal_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Clone, Debug)]
struct TransportFailure {
    kind: io::ErrorKind,
    message: String,
    protocol_error: Option<rmux_proto::RmuxError>,
}

impl TransportFailure {
    fn io(error: io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
            protocol_error: None,
        }
    }

    fn frame(error: rmux_proto::RmuxError) -> Self {
        let message = error.to_string();
        Self {
            kind: io::ErrorKind::InvalidData,
            message,
            protocol_error: Some(error),
        }
    }

    fn eof() -> Self {
        Self {
            kind: io::ErrorKind::UnexpectedEof,
            message: "rmux daemon closed the transport".to_owned(),
            protocol_error: None,
        }
    }

    fn mismatched_response(expected: &'static str, actual: &'static str) -> Self {
        Self {
            kind: io::ErrorKind::InvalidData,
            message: format!(
                "rmux daemon sent `{actual}` response for pending `{expected}` request"
            ),
            protocol_error: None,
        }
    }

    fn unsolicited_response(response: &Response) -> Self {
        Self {
            kind: io::ErrorKind::InvalidData,
            message: format!(
                "rmux daemon sent unsolicited `{}` response",
                response.command_name()
            ),
            protocol_error: None,
        }
    }

    fn actor_closed() -> Self {
        Self {
            kind: io::ErrorKind::BrokenPipe,
            message: "rmux transport actor is closed".to_owned(),
            protocol_error: None,
        }
    }

    const fn is_eof(&self) -> bool {
        matches!(self.kind, io::ErrorKind::UnexpectedEof)
    }

    fn to_error(&self, operation: &str) -> RmuxError {
        RmuxError::transport(operation, io::Error::new(self.kind, self.message.clone()))
    }

    fn to_error_for_command(&self, operation: &str, command_name: &'static str) -> RmuxError {
        if command_name == "handshake" {
            if let Some(error) = self.protocol_error.as_ref() {
                return handshake_protocol_error(error);
            }
        }

        self.to_error(operation)
    }
}

fn handshake_protocol_error(error: &rmux_proto::RmuxError) -> RmuxError {
    match error {
        rmux_proto::RmuxError::Decode(message) => RmuxError::unsupported(
            crate::diagnostics::FEATURE_PROTOCOL_CAPABILITIES,
            format!(
                "upgrade the rmux daemon to one that advertises `{}` before using SDK capability negotiation; {message}",
                rmux_proto::CAPABILITY_HANDSHAKE
            ),
        ),
        error => RmuxError::protocol(error.clone()),
    }
}

#[cfg(test)]
mod tests;
