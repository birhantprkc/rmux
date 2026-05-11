//! SDK-owned facade over the v1 pane-output subscription protocol.
//!
//! The two opaque streams in this module — [`PaneOutputStream`] for raw
//! bytes plus sequence/lag notices, and [`PaneLineStream`] for rendered
//! UTF-8 lines — are constructed through fallible [`crate::Pane`] methods and
//! drive the daemon's `SubscribePaneOutput`, `PaneOutputCursor`, and
//! `UnsubscribePaneOutput` endpoints internally. They never expose
//! [`rmux_proto::PaneOutputSubscriptionId`] to SDK callers.
//!
//! ## Raw bytes vs rendered lines
//!
//! [`PaneOutputStream`] emits [`PaneOutputChunk`] items. Bytes
//! ([`PaneOutputChunk::Bytes`]) preserve every payload byte the daemon
//! delivered, including NUL and bytes that are not valid UTF-8, and pair
//! them with the monotonic per-pane [`PaneOutputChunk::Bytes::sequence`]
//! the daemon assigned. Lag notices ([`PaneOutputChunk::Lag`]) surface
//! the daemon-side gap between the cursor's expected sequence and the
//! oldest retained sequence verbatim, including the bounded recent live
//! bytes the daemon retained at gap detection time. The raw byte stream
//! never converts payloads through `String::from_utf8_lossy` and never
//! alters the byte sequence the daemon delivered.
//!
//! [`PaneLineStream`] is a strict superset built on top of the raw stream
//! that adds two well-isolated transformations:
//!
//! * **Lossy UTF-8 rendering.** Each completed line's bytes are decoded
//!   through `String::from_utf8_lossy`, which replaces every byte
//!   sequence that is not valid UTF-8 with the Unicode replacement
//!   character `U+FFFD`. The lossy conversion is applied only when the
//!   line is yielded — not on the underlying byte stream — so a caller
//!   that wants byte-faithful output should use [`PaneOutputStream`]
//!   instead. Embedded NUL bytes survive into the rendered string as
//!   `\0`, only invalid UTF-8 byte sequences are replaced.
//! * **Partial-line buffering.** The line stream splits on the LF byte
//!   `b'\n'` only. Carriage returns and any other bytes are preserved
//!   inside the line. Bytes that are not yet terminated by an LF stay in
//!   an internal buffer and are not yielded; the buffer is flushed only
//!   when the next LF arrives. A trailing partial line that the daemon
//!   never terminates with LF is dropped when the stream ends or lag
//!   fires, because the next sequence's bytes may not begin at a line
//!   boundary.
//!
//! On a [`PaneOutputChunk::Lag`] the line stream drops the partial-line
//! buffer (the next sequence may be discontinuous with the buffered
//! bytes), forwards the lag notice as [`PaneLineItem::Lag`], and resumes
//! line splitting from a clean state on subsequent bytes. Callers that
//! want to recover the dropped partial bytes can read
//! [`PaneLagNotice::recent`].
//!
//! ## Drop / unsubscribe contract
//!
//! Each stream owns one per-connection subscription on the daemon, and
//! every drop emits at most one best-effort
//! [`UnsubscribePaneOutput`](rmux_proto::UnsubscribePaneOutputRequest)
//! request through the same transport actor. The unsubscribe is fire and
//! forget — its response is discarded, late or duplicate
//! `unsubscribe-pane-output` errors do not propagate, and a closed
//! transport silently no-ops. The daemon's unsubscribe handler only
//! removes the subscription record; it does not close the pane, the
//! window, the session, the underlying child process, or the daemon
//! itself, so dropping an unfinished stream is always safe.
//!
//! Wrapping the line stream around the byte stream means the inner byte
//! stream still owns its own transport drop guard and emits its own
//! unsubscribe — there is exactly one unsubscribe per subscription
//! regardless of which wrapper is dropped.

use std::collections::VecDeque;
use std::time::Duration;

use rmux_proto::{
    PaneOutputCursorRequest, PaneOutputEvent, PaneOutputLagNotice as ProtoLagNotice,
    PaneOutputSubscriptionId, PaneOutputSubscriptionStart, PaneRecentOutput as ProtoRecentOutput,
    PaneTarget, Request, Response, SubscribePaneOutputRequest, UnsubscribePaneOutputRequest,
};

use crate::handles::session::unexpected_response;
use crate::transport::{DropGuard, TransportClient};
use crate::{Result, RmuxError};

const PANE_OUTPUT_BATCH_SIZE: u16 = 256;
const POLL_INITIAL_DELAY: Duration = Duration::from_millis(2);
const POLL_MAX_DELAY: Duration = Duration::from_millis(50);

/// Where a pane-output stream should anchor its cursor at subscription time.
///
/// Mirrors the daemon's own
/// [`rmux_proto::PaneOutputSubscriptionStart`]
/// vocabulary as a SDK-owned enum so callers do not depend on
/// `rmux-proto` directly.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PaneOutputStart {
    /// Start after the newest output currently retained by the pane. The
    /// stream will only deliver bytes the daemon appends after this call.
    #[default]
    Now,
    /// Start at the oldest retained output event, replaying the daemon's
    /// retained backlog before delivering newly produced bytes.
    Oldest,
}

impl PaneOutputStart {
    fn into_proto(self) -> PaneOutputSubscriptionStart {
        match self {
            Self::Now => PaneOutputSubscriptionStart::Now,
            Self::Oldest => PaneOutputSubscriptionStart::Oldest,
        }
    }
}

/// Recent retained pane bytes attached to a [`PaneLagNotice`].
///
/// The byte payload is never converted through `String::from_utf8_lossy`;
/// it is the exact byte run the daemon retained at gap-detection time,
/// bounded by the daemon's `MAX_LAG_RECENT_BYTES` window.
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct PaneRecentOutput {
    /// Retained recent raw pane output bytes.
    pub bytes: Vec<u8>,
    /// Oldest output sequence contributing retained bytes.
    pub oldest_sequence: Option<u64>,
    /// Newest output sequence contributing retained bytes.
    pub newest_sequence: Option<u64>,
}

impl PaneRecentOutput {
    fn from_proto(value: ProtoRecentOutput) -> Self {
        Self {
            bytes: value.bytes,
            oldest_sequence: value.oldest_sequence,
            newest_sequence: value.newest_sequence,
        }
    }
}

/// Detailed gap report carried by [`PaneOutputChunk::Lag`].
///
/// Sequence numbers are exact mirrors of the daemon's own per-pane output
/// counter. `expected_sequence` is the next sequence the cursor was
/// waiting for before lag was detected; `resume_sequence` is the oldest
/// retained sequence the daemon will start delivering from again.
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct PaneLagNotice {
    /// Sequence the subscriber expected before lag was detected.
    pub expected_sequence: u64,
    /// Oldest retained sequence where the subscriber will resume.
    pub resume_sequence: u64,
    /// Number of output events skipped by this lag notice.
    pub missed_events: u64,
    /// Newest output sequence appended when lag was detected.
    pub newest_sequence: u64,
    /// Bounded recent live output the daemon retained at lag time.
    pub recent: PaneRecentOutput,
}

impl PaneLagNotice {
    fn from_proto(value: ProtoLagNotice) -> Self {
        Self {
            expected_sequence: value.expected_sequence,
            resume_sequence: value.resume_sequence,
            missed_events: value.missed_events,
            newest_sequence: value.newest_sequence,
            recent: PaneRecentOutput::from_proto(value.recent),
        }
    }
}

/// One item delivered by [`PaneOutputStream`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PaneOutputChunk {
    /// Raw decoded pane bytes paired with the daemon-assigned monotonic
    /// per-pane sequence.
    Bytes {
        /// Per-pane monotonic output sequence.
        sequence: u64,
        /// Arbitrary raw pane bytes — may include NUL or non-UTF-8 byte
        /// sequences.
        bytes: Vec<u8>,
    },
    /// A daemon-side gap report. Subsequent [`Self::Bytes`] chunks resume
    /// at [`PaneLagNotice::resume_sequence`].
    Lag(PaneLagNotice),
}

/// One item delivered by [`PaneLineStream`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PaneLineItem {
    /// Decoded line text, with `String::from_utf8_lossy` already applied.
    /// The trailing `\n` and any other line-terminator bytes have been
    /// stripped from `Line.text`.
    Line {
        /// Rendered line text.
        text: String,
    },
    /// A daemon-side gap report propagated unchanged from the underlying
    /// raw byte stream. The line stream drops its partial-line buffer
    /// when this fires; subsequent line splitting starts from a clean
    /// state.
    Lag(PaneLagNotice),
}

/// Opaque live stream of pane output bytes plus sequence/lag notices.
///
/// Construction goes through [`Pane::output_stream`](crate::Pane::output_stream).
/// Use [`PaneOutputStream::next`] to drive the cursor; the per-call
/// polling cadence and any backoff is internal and unspecified. The
/// daemon's [`PaneOutputSubscriptionId`] is *not* observable through this
/// type.
pub struct PaneOutputStream {
    inner: PaneSubscription,
    pending: VecDeque<PaneOutputChunk>,
    poll_delay: Duration,
}

/// Opaque live stream of rendered pane output lines.
///
/// Construction goes through [`Pane::line_stream`](crate::Pane::line_stream).
/// See the module docs for the lossy UTF-8 and partial-line buffering
/// rules.
pub struct PaneLineStream {
    inner: PaneOutputStream,
    line_buffer: Vec<u8>,
    pending: VecDeque<PaneLineItem>,
}

struct PaneSubscription {
    transport: TransportClient,
    subscription_id: PaneOutputSubscriptionId,
    // The drop guard is held only for its destructor side effect: it
    // fires the best-effort `unsubscribe-pane-output` request when the
    // parent stream is dropped. The rename signals to the linter that
    // we never read it; the guard's own [`Drop`] is the entire reason
    // it lives in this struct.
    _drop_guard: DropGuard,
    closed: bool,
}

impl PaneOutputStream {
    pub(crate) async fn open(
        transport: TransportClient,
        target: PaneTarget,
        start: PaneOutputStart,
    ) -> Result<Self> {
        let response = transport
            .request(Request::SubscribePaneOutput(SubscribePaneOutputRequest {
                target,
                start: start.into_proto(),
            }))
            .await?;

        let subscription_id = match response {
            Response::SubscribePaneOutput(response) => response.subscription_id,
            response => return Err(unexpected_response("subscribe-pane-output", response)),
        };

        let unsubscribe =
            Request::UnsubscribePaneOutput(UnsubscribePaneOutputRequest { subscription_id });
        let drop_guard = DropGuard::best_effort(transport.clone(), unsubscribe);

        Ok(Self {
            inner: PaneSubscription {
                transport,
                subscription_id,
                _drop_guard: drop_guard,
                closed: false,
            },
            pending: VecDeque::new(),
            poll_delay: POLL_INITIAL_DELAY,
        })
    }

    /// Returns the next chunk, awaiting daemon output if necessary.
    ///
    /// Returns `Ok(None)` once the daemon reports the subscription is no
    /// longer alive — for example after the pane closed and the daemon
    /// removed the subscription record. The drop-time best-effort
    /// unsubscribe still runs in that case.
    pub async fn next(&mut self) -> Result<Option<PaneOutputChunk>> {
        if let Some(chunk) = self.pending.pop_front() {
            return Ok(Some(chunk));
        }
        if self.inner.closed {
            return Ok(None);
        }

        loop {
            match self.refill_once().await? {
                RefillOutcome::Closed => {
                    self.inner.closed = true;
                    return Ok(None);
                }
                RefillOutcome::Filled => {
                    if let Some(chunk) = self.pending.pop_front() {
                        self.poll_delay = POLL_INITIAL_DELAY;
                        return Ok(Some(chunk));
                    }
                    let delay = self.poll_delay;
                    self.poll_delay = (self.poll_delay * 2).min(POLL_MAX_DELAY);
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Drains any chunks that the daemon already has queued for this
    /// subscription. Returns an empty vec when no chunks were ready.
    ///
    /// `poll_once` performs exactly one
    /// [`PaneOutputCursorRequest`] round trip and never sleeps, which
    /// makes it the appropriate primitive for callers that want explicit
    /// control over their own backoff.
    pub async fn poll_once(&mut self) -> Result<Vec<PaneOutputChunk>> {
        let mut buffered: Vec<PaneOutputChunk> = self.pending.drain(..).collect();
        if self.inner.closed {
            return Ok(buffered);
        }

        match self.refill_once().await? {
            RefillOutcome::Closed => {
                self.inner.closed = true;
            }
            RefillOutcome::Filled => {
                buffered.extend(self.pending.drain(..));
            }
        }
        Ok(buffered)
    }

    async fn refill_once(&mut self) -> Result<RefillOutcome> {
        let request = Request::PaneOutputCursor(PaneOutputCursorRequest {
            subscription_id: self.inner.subscription_id,
            max_events: Some(PANE_OUTPUT_BATCH_SIZE),
        });

        match self.inner.transport.request(request).await {
            Ok(Response::PaneOutputCursor(cursor)) => {
                self.inner
                    .validate_response_subscription("pane-output-cursor", cursor.subscription_id)?;
                ingest_cursor(&mut self.pending, cursor.events);
                Ok(RefillOutcome::Filled)
            }
            Ok(Response::PaneOutputLag(lag)) => {
                self.inner
                    .validate_response_subscription("pane-output-lag", lag.subscription_id)?;
                self.pending
                    .push_back(PaneOutputChunk::Lag(PaneLagNotice::from_proto(lag.lag)));
                Ok(RefillOutcome::Filled)
            }
            Ok(response) => Err(unexpected_response("pane-output-cursor", response)),
            Err(error) if is_subscription_gone(&error) => Ok(RefillOutcome::Closed),
            Err(error) => Err(error),
        }
    }
}

impl PaneSubscription {
    fn validate_response_subscription(
        &self,
        command: &'static str,
        response_id: PaneOutputSubscriptionId,
    ) -> Result<()> {
        if response_id == self.subscription_id {
            return Ok(());
        }
        Err(subscription_mismatch_error(
            command,
            self.subscription_id,
            response_id,
        ))
    }
}

fn subscription_mismatch_error(
    command: &'static str,
    expected: PaneOutputSubscriptionId,
    got: PaneOutputSubscriptionId,
) -> RmuxError {
    RmuxError::protocol(rmux_proto::RmuxError::Server(format!(
        "rmux daemon sent subscription id {} in `{command}` response for subscription {}",
        got.as_u64(),
        expected.as_u64()
    )))
}

fn ingest_cursor(target: &mut VecDeque<PaneOutputChunk>, events: Vec<PaneOutputEvent>) {
    target.reserve(events.len());
    for event in events {
        target.push_back(PaneOutputChunk::Bytes {
            sequence: event.sequence,
            bytes: event.bytes,
        });
    }
}

enum RefillOutcome {
    Filled,
    Closed,
}

fn is_subscription_gone(error: &RmuxError) -> bool {
    match error {
        RmuxError::Protocol {
            source: rmux_proto::RmuxError::Server(message),
        } => message == "subscription not found" || message == "subscription receiver not found",
        _ => false,
    }
}

impl PaneLineStream {
    pub(crate) fn wrap(inner: PaneOutputStream) -> Self {
        Self {
            inner,
            line_buffer: Vec::new(),
            pending: VecDeque::new(),
        }
    }

    /// Returns the next line or lag notice, awaiting daemon output if
    /// necessary.
    ///
    /// Returns `Ok(None)` when the underlying subscription is gone. Any
    /// trailing partial-line bytes that were never terminated by `\n`
    /// are dropped at end-of-stream because the daemon never delivered a
    /// terminator — they did not represent a complete line.
    pub async fn next(&mut self) -> Result<Option<PaneLineItem>> {
        loop {
            if let Some(item) = self.pending.pop_front() {
                return Ok(Some(item));
            }
            match self.inner.next().await? {
                Some(PaneOutputChunk::Bytes { bytes, .. }) => {
                    split_lines(&mut self.line_buffer, &bytes, &mut self.pending);
                }
                Some(PaneOutputChunk::Lag(notice)) => {
                    // Drop partial-line buffer because the byte stream is
                    // discontinuous after a lag — the next bytes may not
                    // begin at a line boundary, so concatenating them
                    // would produce a synthetic line.
                    self.line_buffer.clear();
                    self.pending.push_back(PaneLineItem::Lag(notice));
                }
                None => return Ok(None),
            }
        }
    }
}

fn split_lines(buffer: &mut Vec<u8>, bytes: &[u8], out: &mut VecDeque<PaneLineItem>) {
    for byte in bytes {
        if *byte == b'\n' {
            let line_bytes = std::mem::take(buffer);
            out.push_back(PaneLineItem::Line {
                text: String::from_utf8_lossy(&line_bytes).into_owned(),
            });
        } else {
            buffer.push(*byte);
        }
    }
}

impl std::fmt::Debug for PaneOutputStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PaneOutputStream")
            .field("closed", &self.inner.closed)
            .field("buffered_chunks", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for PaneLineStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PaneLineStream")
            .field("buffered_bytes", &self.line_buffer.len())
            .field("pending_items", &self.pending.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[path = "streams_contract_tests.rs"]
mod streams_contract_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_lines_buffers_partial_input_and_drops_trailing_newlines() {
        let mut buffer = Vec::new();
        let mut out: VecDeque<PaneLineItem> = VecDeque::new();
        split_lines(&mut buffer, b"alpha\nbet", &mut out);
        assert_eq!(buffer, b"bet");
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0],
            PaneLineItem::Line { text } if text == "alpha"
        ));

        split_lines(&mut buffer, b"a\n", &mut out);
        assert!(buffer.is_empty());
        assert_eq!(out.len(), 2);
        assert!(matches!(
            &out[1],
            PaneLineItem::Line { text } if text == "beta"
        ));
    }

    #[test]
    fn split_lines_emits_empty_line_on_consecutive_newlines() {
        let mut buffer = Vec::new();
        let mut out: VecDeque<PaneLineItem> = VecDeque::new();
        split_lines(&mut buffer, b"\n\n", &mut out);
        assert!(buffer.is_empty());
        assert_eq!(out.len(), 2);
        for item in out {
            assert!(matches!(item, PaneLineItem::Line { text } if text.is_empty()));
        }
    }

    #[test]
    fn split_lines_replaces_invalid_utf8_with_replacement_character() {
        let mut buffer = Vec::new();
        let mut out: VecDeque<PaneLineItem> = VecDeque::new();
        split_lines(&mut buffer, b"\xffhello\n", &mut out);
        assert_eq!(out.len(), 1);
        let PaneLineItem::Line { text } = out.into_iter().next().unwrap() else {
            panic!("expected line item");
        };
        assert!(
            text.contains('\u{FFFD}'),
            "lossy UTF-8 must replace invalid bytes with U+FFFD; got `{text}`"
        );
        assert!(text.ends_with("hello"));
    }

    #[test]
    fn split_lines_keeps_carriage_return_inside_line() {
        let mut buffer = Vec::new();
        let mut out: VecDeque<PaneLineItem> = VecDeque::new();
        split_lines(&mut buffer, b"alpha\r\n", &mut out);
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out.front().unwrap(),
            PaneLineItem::Line { text } if text == "alpha\r"
        ));
    }

    #[test]
    fn pane_output_start_maps_to_proto_variants() {
        assert_eq!(
            PaneOutputStart::Now.into_proto(),
            PaneOutputSubscriptionStart::Now
        );
        assert_eq!(
            PaneOutputStart::Oldest.into_proto(),
            PaneOutputSubscriptionStart::Oldest
        );
    }

    #[test]
    fn is_subscription_gone_matches_known_server_strings() {
        let gone = RmuxError::protocol(rmux_proto::RmuxError::Server(
            "subscription not found".to_owned(),
        ));
        let receiver_gone = RmuxError::protocol(rmux_proto::RmuxError::Server(
            "subscription receiver not found".to_owned(),
        ));
        let other = RmuxError::protocol(rmux_proto::RmuxError::Server(
            "different daemon error".to_owned(),
        ));
        assert!(is_subscription_gone(&gone));
        assert!(is_subscription_gone(&receiver_gone));
        assert!(!is_subscription_gone(&other));
    }

    #[test]
    fn is_subscription_gone_does_not_match_ownership_or_invalid_target_errors() {
        // The daemon emits a separate "not owned by this connection"
        // error when a cursor is driven from the wrong transport. That
        // is a real protocol violation, not a subscription-gone signal,
        // so it must propagate as an SDK error rather than silently
        // ending the stream.
        let owned_elsewhere = RmuxError::protocol(rmux_proto::RmuxError::Server(
            "subscription is not owned by this connection".to_owned(),
        ));
        let invalid_target = RmuxError::protocol(rmux_proto::RmuxError::InvalidTarget {
            value: "alpha:0.0".to_owned(),
            reason: "pane index does not exist in session".to_owned(),
        });
        let session_not_found =
            RmuxError::protocol(rmux_proto::RmuxError::SessionNotFound("alpha".to_owned()));
        assert!(!is_subscription_gone(&owned_elsewhere));
        assert!(!is_subscription_gone(&invalid_target));
        assert!(!is_subscription_gone(&session_not_found));
    }

    #[test]
    fn split_lines_preserves_nul_byte_inside_rendered_text() {
        // NUL is valid UTF-8 (`U+0000`) and must round-trip through the
        // lossy decode that the line stream applies — only invalid byte
        // sequences are allowed to collapse to U+FFFD.
        let mut buffer = Vec::new();
        let mut out: VecDeque<PaneLineItem> = VecDeque::new();
        split_lines(&mut buffer, b"a\0b\n", &mut out);
        assert_eq!(out.len(), 1);
        let PaneLineItem::Line { text } = out.into_iter().next().unwrap() else {
            panic!("expected line item");
        };
        assert_eq!(text, "a\0b");
        assert!(!text.contains('\u{FFFD}'));
    }

    #[test]
    fn split_lines_reassembles_multibyte_codepoint_across_chunk_boundary() {
        // The daemon may chunk arbitrary byte boundaries. A two-byte
        // UTF-8 codepoint split across two cursor batches must NOT
        // produce a U+FFFD default_value when the LF arrives — the line
        // stream lossy-decodes the *complete* line, not each chunk.
        let mut buffer = Vec::new();
        let mut out: VecDeque<PaneLineItem> = VecDeque::new();
        split_lines(&mut buffer, &[0xc3], &mut out); // first half of `é`
        assert!(out.is_empty(), "no LF yet, no line yielded");
        split_lines(&mut buffer, &[0xa9, b'\n'], &mut out);
        let PaneLineItem::Line { text } = out.into_iter().next().unwrap() else {
            panic!("expected line item");
        };
        assert_eq!(text, "é");
        assert!(!text.contains('\u{FFFD}'));
    }

    #[test]
    fn split_lines_yields_many_lines_in_order_from_one_chunk() {
        // Multiple LFs in a single chunk must yield lines in protocol
        // order without reordering.
        let mut buffer = Vec::new();
        let mut out: VecDeque<PaneLineItem> = VecDeque::new();
        split_lines(&mut buffer, b"one\ntwo\nthree\n", &mut out);
        let texts: Vec<String> = out
            .into_iter()
            .map(|item| match item {
                PaneLineItem::Line { text } => text,
                other => panic!("expected line item, got {other:?}"),
            })
            .collect();
        assert_eq!(texts, vec!["one", "two", "three"]);
        assert!(buffer.is_empty(), "trailing LF flushes the buffer");
    }

    #[test]
    fn ingest_cursor_preserves_event_order_and_payload_bytes() {
        let mut pending: VecDeque<PaneOutputChunk> = VecDeque::new();
        ingest_cursor(
            &mut pending,
            vec![
                PaneOutputEvent {
                    sequence: 5,
                    bytes: vec![0xff, 0x00, b'a'],
                },
                PaneOutputEvent {
                    sequence: 6,
                    bytes: b"b\n".to_vec(),
                },
            ],
        );
        let chunk = pending.pop_front().expect("first event");
        match chunk {
            PaneOutputChunk::Bytes { sequence, bytes } => {
                assert_eq!(sequence, 5);
                assert_eq!(bytes, vec![0xff, 0x00, b'a']);
            }
            other => panic!("expected bytes chunk, got {other:?}"),
        }
        let chunk = pending.pop_front().expect("second event");
        match chunk {
            PaneOutputChunk::Bytes { sequence, bytes } => {
                assert_eq!(sequence, 6);
                assert_eq!(bytes, b"b\n");
            }
            other => panic!("expected bytes chunk, got {other:?}"),
        }
    }

    #[test]
    fn pane_output_start_default_is_now() {
        // The default is `Now` — replaying the entire retained backlog
        // by accident on every stream open would be a surprising
        // performance footgun, so the default deliberately starts at
        // the live tail.
        assert_eq!(PaneOutputStart::default(), PaneOutputStart::Now);
    }

    #[test]
    fn pane_recent_output_from_proto_round_trips_payload_and_sequences() {
        let proto = ProtoRecentOutput {
            bytes: vec![0xff, 0xfe, 0x00, b'!'],
            oldest_sequence: Some(11),
            newest_sequence: Some(13),
        };
        let recent = PaneRecentOutput::from_proto(proto);
        assert_eq!(recent.bytes, vec![0xff, 0xfe, 0x00, b'!']);
        assert_eq!(recent.oldest_sequence, Some(11));
        assert_eq!(recent.newest_sequence, Some(13));
    }

    #[test]
    fn pane_lag_notice_from_proto_round_trips_all_fields() {
        let proto = ProtoLagNotice {
            expected_sequence: 3,
            resume_sequence: 9,
            missed_events: 6,
            newest_sequence: 12,
            recent: ProtoRecentOutput {
                bytes: b"abc".to_vec(),
                oldest_sequence: Some(8),
                newest_sequence: Some(9),
            },
        };
        let notice = PaneLagNotice::from_proto(proto);
        assert_eq!(notice.expected_sequence, 3);
        assert_eq!(notice.resume_sequence, 9);
        assert_eq!(notice.missed_events, 6);
        assert_eq!(notice.newest_sequence, 12);
        assert_eq!(notice.recent.bytes, b"abc");
        assert_eq!(notice.recent.oldest_sequence, Some(8));
        assert_eq!(notice.recent.newest_sequence, Some(9));
    }
}
