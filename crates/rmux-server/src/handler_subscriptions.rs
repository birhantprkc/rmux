use std::collections::HashMap;
use std::time::Instant;

use rmux_core::events::{
    OutputCursor, OutputCursorItem, OutputGap, PaneOutputSubscriptionKey, SubscriptionLimitError,
    SubscriptionLimits, SubscriptionRegistry,
};
use rmux_proto::{
    ErrorResponse, PaneOutputCursor, PaneOutputCursorRequest, PaneOutputCursorResponse,
    PaneOutputEvent, PaneOutputLagNotice, PaneOutputLagResponse, PaneOutputSubscriptionId,
    PaneOutputSubscriptionStart, PaneRecentOutput, Response, RmuxError, SubscribePaneOutputRequest,
    SubscribePaneOutputResponse, UnsubscribePaneOutputRequest, UnsubscribePaneOutputResponse,
    DEFAULT_MAX_FRAME_LENGTH,
};

use crate::pane_io::PaneOutputReceiver;

use super::RequestHandler;

// Keep lag diagnostics well below the detached RPC frame cap after bincode
// overhead and the rest of the response envelope are added.
const MAX_LAG_RECENT_BYTES: usize = DEFAULT_MAX_FRAME_LENGTH / 16;

pub(crate) struct OutputSubscriptionState {
    registry: SubscriptionRegistry,
    receivers: HashMap<PaneOutputSubscriptionId, PaneOutputReceiver>,
}

impl std::fmt::Debug for OutputSubscriptionState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutputSubscriptionState")
            .field("registry", &self.registry)
            .field("receiver_count", &self.receivers.len())
            .finish()
    }
}

impl OutputSubscriptionState {
    pub(crate) fn new(limits: SubscriptionLimits) -> Self {
        Self {
            registry: SubscriptionRegistry::new(limits),
            receivers: HashMap::new(),
        }
    }

    fn limits(&self) -> SubscriptionLimits {
        self.registry.limits()
    }

    fn cleanup_stale(&mut self, now: Instant) {
        for record in self.registry.cleanup_stale(now) {
            self.receivers.remove(&record.id());
        }
    }

    fn remove_connection(&mut self, connection_id: u64) {
        for record in self.registry.remove_connection(connection_id) {
            self.receivers.remove(&record.id());
        }
    }

    fn remove_pane(&mut self, pane: &PaneOutputSubscriptionKey) {
        for record in self.registry.remove_pane(pane) {
            self.receivers.remove(&record.id());
        }
    }
}

impl RequestHandler {
    pub(in crate::handler) async fn handle_subscribe_pane_output(
        &self,
        connection_id: u64,
        request: SubscribePaneOutputRequest,
    ) -> Response {
        let now = Instant::now();
        let (subscription_id, pane_id, cursor) = {
            let state = self.state.lock().await;
            let pane_key = match state.pane_output_subscription_key_for_target(&request.target) {
                Ok(key) => key,
                Err(error) => return Response::Error(ErrorResponse { error }),
            };
            let output = match state.pane_output_for_target(
                request.target.session_name(),
                request.target.window_index(),
                request.target.pane_index(),
            ) {
                Ok(output) => output,
                Err(error) => return Response::Error(ErrorResponse { error }),
            };
            let receiver = match request.start {
                PaneOutputSubscriptionStart::Now => output.subscribe(),
                PaneOutputSubscriptionStart::Oldest => output.subscribe_from_oldest(),
            };

            let mut subscriptions = self
                .subscriptions
                .lock()
                .expect("subscription registry mutex must not be poisoned");
            subscriptions.cleanup_stale(now);
            let record =
                match subscriptions
                    .registry
                    .subscribe(connection_id, pane_key.clone(), now)
                {
                    Ok(record) => record,
                    Err(error) => {
                        return Response::Error(ErrorResponse {
                            error: subscription_limit_error(error),
                        });
                    }
                };
            let cursor = cursor_dto(receiver.cursor());
            let subscription_id = record.id();
            subscriptions.receivers.insert(record.id(), receiver);
            (subscription_id, pane_key.pane_id(), cursor)
        };

        Response::SubscribePaneOutput(SubscribePaneOutputResponse {
            subscription_id,
            target: request.target,
            pane_id,
            cursor,
        })
    }

    pub(in crate::handler) async fn handle_unsubscribe_pane_output(
        &self,
        connection_id: u64,
        request: UnsubscribePaneOutputRequest,
    ) -> Response {
        let now = Instant::now();
        let mut subscriptions = self
            .subscriptions
            .lock()
            .expect("subscription registry mutex must not be poisoned");
        subscriptions.cleanup_stale(now);

        let Some(record) = subscriptions.registry.get(request.subscription_id).cloned() else {
            return Response::UnsubscribePaneOutput(UnsubscribePaneOutputResponse {
                subscription_id: request.subscription_id,
                removed: false,
            });
        };
        if record.connection_id() != connection_id {
            return Response::Error(ErrorResponse {
                error: RmuxError::Server("subscription is not owned by this connection".to_owned()),
            });
        }

        let removed = subscriptions
            .registry
            .unsubscribe(request.subscription_id)
            .is_some();
        subscriptions.receivers.remove(&request.subscription_id);
        Response::UnsubscribePaneOutput(UnsubscribePaneOutputResponse {
            subscription_id: request.subscription_id,
            removed,
        })
    }

    pub(in crate::handler) async fn handle_pane_output_cursor(
        &self,
        connection_id: u64,
        request: PaneOutputCursorRequest,
    ) -> Response {
        let now = Instant::now();
        let mut subscriptions = self
            .subscriptions
            .lock()
            .expect("subscription registry mutex must not be poisoned");
        subscriptions.cleanup_stale(now);
        let limit =
            match cursor_event_limit(request.max_events, subscriptions.limits().batch_events()) {
                Ok(limit) => limit,
                Err(error) => return Response::Error(ErrorResponse { error }),
            };

        let Some(record) = subscriptions.registry.get(request.subscription_id).cloned() else {
            return Response::Error(ErrorResponse {
                error: RmuxError::Server("subscription not found".to_owned()),
            });
        };
        if record.connection_id() != connection_id {
            return Response::Error(ErrorResponse {
                error: RmuxError::Server("subscription is not owned by this connection".to_owned()),
            });
        }
        let _ = subscriptions.registry.touch(request.subscription_id, now);

        let Some(receiver) = subscriptions.receivers.get_mut(&request.subscription_id) else {
            let _ = subscriptions.registry.unsubscribe(request.subscription_id);
            return Response::Error(ErrorResponse {
                error: RmuxError::Server("subscription receiver not found".to_owned()),
            });
        };

        let items = receiver.try_recv_batch(limit);
        let mut events = Vec::new();
        for item in items {
            match item {
                OutputCursorItem::Event(event) => {
                    events.push(PaneOutputEvent {
                        sequence: event.sequence(),
                        bytes: event.into_bytes(),
                    });
                }
                OutputCursorItem::Gap(gap) => {
                    let cursor = cursor_dto(receiver.cursor());
                    return Response::PaneOutputLag(PaneOutputLagResponse {
                        subscription_id: request.subscription_id,
                        cursor,
                        lag: lag_dto(&gap),
                    });
                }
            }
        }

        Response::PaneOutputCursor(PaneOutputCursorResponse {
            subscription_id: request.subscription_id,
            cursor: cursor_dto(receiver.cursor()),
            limited: events.len() == limit,
            events,
        })
    }

    pub(crate) async fn cleanup_connection_subscriptions(&self, connection_id: u64) {
        let mut subscriptions = self
            .subscriptions
            .lock()
            .expect("subscription registry mutex must not be poisoned");
        subscriptions.remove_connection(connection_id);
    }

    pub(crate) async fn cleanup_pane_output_subscriptions(
        &self,
        panes: &[PaneOutputSubscriptionKey],
    ) {
        let mut subscriptions = self
            .subscriptions
            .lock()
            .expect("subscription registry mutex must not be poisoned");
        for pane in panes {
            subscriptions.remove_pane(pane);
        }
    }
}

fn cursor_event_limit(requested: Option<u16>, default: usize) -> Result<usize, RmuxError> {
    match requested {
        Some(0) => Err(RmuxError::Server(
            "pane output cursor max_events must be greater than zero".to_owned(),
        )),
        Some(value) => Ok(usize::from(value).min(default)),
        None => Ok(default),
    }
}

fn cursor_dto(cursor: &OutputCursor) -> PaneOutputCursor {
    PaneOutputCursor {
        next_sequence: cursor.next_sequence(),
        missed_events: cursor.missed_events(),
    }
}

fn lag_dto(gap: &OutputGap) -> PaneOutputLagNotice {
    let recent = gap.recent_snapshot();
    let mut recent_bytes = recent.bytes().to_vec();
    let truncated = recent_bytes.len() > MAX_LAG_RECENT_BYTES;
    if truncated {
        recent_bytes = recent_bytes[recent_bytes.len() - MAX_LAG_RECENT_BYTES..].to_vec();
    }
    PaneOutputLagNotice {
        expected_sequence: gap.expected_sequence(),
        resume_sequence: gap.resume_sequence(),
        missed_events: gap.missed_events(),
        newest_sequence: gap.newest_sequence(),
        recent: PaneRecentOutput {
            bytes: recent_bytes,
            oldest_sequence: if truncated {
                None
            } else {
                recent.oldest_sequence()
            },
            newest_sequence: recent.newest_sequence(),
        },
    }
}

fn subscription_limit_error(error: SubscriptionLimitError) -> RmuxError {
    match error {
        SubscriptionLimitError::PerConnection { limit } => RmuxError::Server(format!(
            "pane output subscription limit exceeded for connection (limit {limit})"
        )),
        SubscriptionLimitError::PerPane { limit } => RmuxError::Server(format!(
            "pane output subscription limit exceeded for pane (limit {limit})"
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use rmux_core::events::{
        OutputCursor, OutputCursorItem, OutputRing, PaneOutputSubscriptionKey,
    };
    use rmux_proto::{PaneId, PaneOutputCursorRequest, Response, SessionName};

    use crate::pane_io::pane_output_channel_with_limits;

    use super::{lag_dto, RequestHandler, MAX_LAG_RECENT_BYTES};

    #[test]
    fn lag_dto_carries_recent_output_without_replaying_missed_bytes() {
        let mut ring = OutputRing::new(1, 8);
        let mut cursor = OutputCursor::new(0);
        ring.push(b"abcdef".to_vec());
        ring.push(b"ghijk".to_vec());

        let Some(OutputCursorItem::Gap(gap)) = ring.poll_cursor(&mut cursor) else {
            panic!("cursor should lag after output ring rotation");
        };
        let notice = lag_dto(&gap);

        assert_eq!(notice.expected_sequence, 0);
        assert_eq!(notice.resume_sequence, 1);
        assert_eq!(notice.missed_events, 1);
        assert_eq!(notice.newest_sequence, 1);
        assert_eq!(notice.recent.bytes, b"defghijk");
        assert_eq!(notice.recent.oldest_sequence, Some(0));
        assert_eq!(notice.recent.newest_sequence, Some(1));

        let Some(OutputCursorItem::Event(event)) = ring.poll_cursor(&mut cursor) else {
            panic!("cursor should resume at the oldest retained output event");
        };
        assert_eq!(event.sequence(), notice.resume_sequence);
        assert_eq!(event.bytes(), b"ghijk");
        assert_ne!(event.bytes(), notice.recent.bytes.as_slice());
    }

    #[test]
    fn lag_dto_trims_recent_hint_under_detached_frame_limit() {
        let mut ring = OutputRing::new(1, MAX_LAG_RECENT_BYTES + 16);
        let mut cursor = OutputCursor::new(0);
        ring.push(vec![b'a'; 16]);
        ring.push(vec![b'b'; MAX_LAG_RECENT_BYTES + 16]);

        let Some(OutputCursorItem::Gap(gap)) = ring.poll_cursor(&mut cursor) else {
            panic!("cursor should lag after output ring rotation");
        };
        let notice = lag_dto(&gap);

        assert_eq!(notice.recent.bytes.len(), MAX_LAG_RECENT_BYTES);
        assert!(notice.recent.bytes.iter().all(|byte| *byte == b'b'));
        assert_eq!(notice.recent.oldest_sequence, None);
        assert_eq!(notice.recent.newest_sequence, Some(1));
        assert_eq!(notice.resume_sequence, 1);
    }

    #[tokio::test]
    async fn cursor_handler_trims_lag_recent_hint_for_subscription_response() {
        let handler = RequestHandler::new();
        let connection_id = 77;
        let sender = pane_output_channel_with_limits(1, MAX_LAG_RECENT_BYTES + 32);
        let receiver = sender.subscribe();
        sender.send(vec![b'a'; 32]);
        sender.send(vec![b'b'; MAX_LAG_RECENT_BYTES + 32]);

        let subscription_id = {
            let mut subscriptions = handler
                .subscriptions
                .lock()
                .expect("subscription registry mutex must not be poisoned");
            let record = subscriptions
                .registry
                .subscribe(
                    connection_id,
                    PaneOutputSubscriptionKey::new(
                        SessionName::new("runtime").expect("valid session name"),
                        PaneId::new(1),
                    ),
                    Instant::now(),
                )
                .expect("subscription is within limits");
            let subscription_id = record.id();
            subscriptions.receivers.insert(subscription_id, receiver);
            subscription_id
        };

        let response = handler
            .handle_pane_output_cursor(
                connection_id,
                PaneOutputCursorRequest {
                    subscription_id,
                    max_events: Some(8),
                },
            )
            .await;
        let Response::PaneOutputLag(lag) = response else {
            panic!("lagged subscription should produce a lag response");
        };

        assert_eq!(lag.subscription_id, subscription_id);
        assert_eq!(lag.cursor.next_sequence, 1);
        assert_eq!(lag.cursor.missed_events, 1);
        assert_eq!(lag.lag.expected_sequence, 0);
        assert_eq!(lag.lag.resume_sequence, 1);
        assert_eq!(lag.lag.missed_events, 1);
        assert_eq!(lag.lag.recent.bytes.len(), MAX_LAG_RECENT_BYTES);
        assert!(lag.lag.recent.bytes.iter().all(|byte| *byte == b'b'));
        assert_eq!(lag.lag.recent.oldest_sequence, None);
        assert_eq!(lag.lag.recent.newest_sequence, Some(1));
    }
}
