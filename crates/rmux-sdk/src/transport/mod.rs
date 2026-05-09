//! Crate-private Tokio transport actor for detached SDK RPC.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

use rmux_proto::{encode_frame, FrameDecoder, Request, Response};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::{Result, RmuxError};

const ACTOR_QUEUE_CAPACITY: usize = 64;
const READ_BUFFER_SIZE: usize = 8192;

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

    fn try_send_best_effort(&self, request: Request) {
        if self.state.terminal_failure().is_some() {
            return;
        }

        let _ = self.commands.try_send(ActorMessage::BestEffort { request });
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

    fn best_effort(client: TransportClient, request: Request) -> Self {
        Self {
            action: DropAction::BestEffort {
                client,
                request: Some(Box::new(request)),
            },
        }
    }
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        if let DropAction::BestEffort { client, request } = &mut self.action {
            if let Some(request) = request.take() {
                client.try_send_best_effort(*request);
            }
        }
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
    BestEffort {
        request: Request,
    },
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
            let _ = reply.send(Err(failure.to_error(&self.operation)));
        }
    }
}

async fn run_actor<S>(
    stream: S,
    mut commands: mpsc::Receiver<ActorMessage>,
    state: Arc<TransportState>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut decoder = FrameDecoder::new();
    let mut pending = VecDeque::new();
    let mut commands_closed = false;

    loop {
        if commands_closed && pending.is_empty() {
            let _ = writer.shutdown().await;
            return;
        }

        tokio::select! {
            message = commands.recv(), if !commands_closed => {
                match message {
                    Some(ActorMessage::Request { request, operation, reply }) => {
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
                            return;
                        }
                    }
                    Some(ActorMessage::BestEffort { request }) => {
                        let command_name = request.command_name();
                        let Ok(frame) = encode_request(&request) else {
                            continue;
                        };
                        pending.push_back(PendingCall::discard(command_name, request_operation(&request)));
                        if let Err(failure) = write_frame(&mut writer, &frame).await {
                            fail_transport(&mut pending, &state, failure);
                            return;
                        }
                    }
                    None => {
                        commands_closed = true;
                    }
                }
            }
            result = read_response(&mut reader, &mut decoder) => {
                match result {
                    Ok(response) => {
                        let Some(pending_call) = pending.pop_front() else {
                            fail_transport(
                                &mut pending,
                                &state,
                                TransportFailure::unsolicited_response(&response),
                            );
                            return;
                        };
                        if let Err(failure) = pending_call.validate_response(&response) {
                            pending_call.fail(&failure);
                            fail_transport(&mut pending, &state, failure);
                            return;
                        }
                        pending_call.complete(response);
                    }
                    Err(failure) => {
                        fail_transport(&mut pending, &state, failure);
                        return;
                    }
                }
            }
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

#[derive(Debug, Default)]
struct TransportState {
    terminal_failure: Mutex<Option<TransportFailure>>,
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
}

impl TransportFailure {
    fn io(error: io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    fn frame(error: rmux_proto::RmuxError) -> Self {
        Self {
            kind: io::ErrorKind::InvalidData,
            message: error.to_string(),
        }
    }

    fn eof() -> Self {
        Self {
            kind: io::ErrorKind::UnexpectedEof,
            message: "rmux daemon closed the transport".to_owned(),
        }
    }

    fn mismatched_response(expected: &'static str, actual: &'static str) -> Self {
        Self {
            kind: io::ErrorKind::InvalidData,
            message: format!(
                "rmux daemon sent `{actual}` response for pending `{expected}` request"
            ),
        }
    }

    fn unsolicited_response(response: &Response) -> Self {
        Self {
            kind: io::ErrorKind::InvalidData,
            message: format!(
                "rmux daemon sent unsolicited `{}` response",
                response.command_name()
            ),
        }
    }

    fn actor_closed() -> Self {
        Self {
            kind: io::ErrorKind::BrokenPipe,
            message: "rmux transport actor is closed".to_owned(),
        }
    }

    fn to_error(&self, operation: &str) -> RmuxError {
        RmuxError::transport(operation, io::Error::new(self.kind, self.message.clone()))
    }
}

#[cfg(test)]
mod tests;
