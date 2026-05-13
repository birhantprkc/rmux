//! Daemon-backed pane handle.
//!
//! The handle never caches a `PaneId`. Every operation re-reads the
//! daemon's current view of the addressed `(session, window, pane)` slot,
//! which is what keeps linked windows and grouped sessions returning the
//! same stable `%N` identity through every sibling view, and what makes
//! stale handles behave the same way as stale [`Window`](super::Window)
//! handles: the typed empty/`None` results carry the original target
//! verbatim instead of erroring out.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::events::streams::{PaneLineStream, PaneOutputStart, PaneOutputStream};
use crate::handles::split::SplitDirection;
use crate::transport::TransportClient;
use crate::{
    ArmedWait, CollectedPaneOutput, InfoSnapshot, PaneExitState, PaneId, PaneRef, PaneSnapshot,
    PaneTextMatch, ProcessSpec, Result, RmuxEndpoint, TerminalSizeSpec,
};

#[path = "pane/info.rs"]
mod info;
#[path = "pane/input.rs"]
mod input;
#[path = "pane/lifecycle.rs"]
mod lifecycle;
#[path = "pane/snapshot.rs"]
mod snapshot;
#[path = "pane/split.rs"]
mod split;
#[path = "pane/target.rs"]
mod target;

use info::{current_pane_entry, pane_info_snapshot};
use input::{resize_to_size, send_key, send_text};
use lifecycle::{close_pane, respawn_pane};
use snapshot::pane_snapshot;
use split::split_pane;
pub(crate) use target::is_already_closed_pane_error;

/// Result of consuming a [`Pane`] handle with [`Pane::close`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PaneCloseOutcome {
    /// The daemon killed the addressed pane.
    Closed {
        /// The pane target consumed by the close call.
        target: PaneRef,
        /// Whether the pane removal also destroyed its window.
        window_destroyed: bool,
    },
    /// The addressed pane was already absent by the time close ran.
    AlreadyClosed {
        /// The stale target consumed by the close call.
        target: PaneRef,
    },
}

/// Process and policy fields for [`Pane::respawn`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PaneRespawnOptions {
    /// Whether a running pane should be killed before respawning.
    pub kill: bool,
    /// Optional working-directory override for the new process.
    pub start_directory: Option<PathBuf>,
    /// Process argv and per-spawn environment overrides.
    pub process: ProcessSpec,
}

/// Opaque handle for one daemon pane slot.
///
/// A pane handle addresses a `(session, window, pane)` triple rather than
/// caching a `PaneId`. Every operation resolves that slot against the
/// daemon's current state, so:
///
/// * linked windows and grouped sessions keep returning the same stable
///   `%N` identity through every sibling view, and
/// * stale handles for an already-closed pane resolve to typed
///   `None`/empty results — never to a panic and never to a `PaneId` from
///   a prior epoch.
///
/// The handle deliberately exposes no `current_revision()` accessor.
/// Revision values are only observable through
/// [`PaneSnapshot::revision`] on a freshly captured snapshot, or through
/// the revision-carrying [`PaneEvent`](crate::PaneEvent) variants emitted
/// over a control-mode subscription.
#[derive(Clone)]
pub struct Pane {
    target: PaneRef,
    endpoint: RmuxEndpoint,
    default_timeout: Option<Duration>,
    transport: TransportClient,
}

impl Pane {
    pub(crate) fn new(
        target: PaneRef,
        endpoint: RmuxEndpoint,
        default_timeout: Option<Duration>,
        transport: TransportClient,
    ) -> Self {
        Self {
            target,
            endpoint,
            default_timeout,
            transport,
        }
    }

    /// Returns the exact protocol-owned pane target addressed by this
    /// handle.
    #[must_use]
    pub const fn target(&self) -> &PaneRef {
        &self.target
    }

    /// Returns the endpoint that was resolved when this handle was created.
    #[must_use]
    pub const fn endpoint(&self) -> &RmuxEndpoint {
        &self.endpoint
    }

    /// Returns the default timeout configured on the parent facade.
    #[must_use]
    pub const fn configured_default_timeout(&self) -> Option<Duration> {
        self.default_timeout
    }

    pub(crate) const fn transport(&self) -> &TransportClient {
        &self.transport
    }

    /// Waits until the pane emits the requested raw byte sequence.
    ///
    /// Dropping the returned future before it completes sends a best-effort
    /// daemon cancellation request. Drop cleanup only removes the wait record;
    /// it never closes panes, sessions, processes, or the daemon.
    pub async fn wait_for(&self, bytes: impl AsRef<[u8]>) -> Result<()> {
        crate::wait::wait_for_bytes(self, bytes.as_ref().to_vec()).await
    }

    /// Arms a daemon-backed wait for future raw pane output bytes.
    ///
    /// The returned [`ArmedWait`] is created only after the SDK has sent the
    /// daemon wait request with a live-tail cursor, so it cannot match retained
    /// history from before this call. Await the handle after triggering the
    /// output that should satisfy the wait.
    pub async fn wait_for_next(&self, bytes: impl AsRef<[u8]>) -> Result<ArmedWait> {
        crate::wait::wait_for_next_bytes(self, bytes.as_ref().to_vec()).await
    }

    /// Waits until the pane's rendered snapshot text contains non-empty `text`.
    ///
    /// This is a client-side text wait over fresh [`Self::snapshot`]
    /// captures. It observes the rendered grid text already present at the
    /// time of the first snapshot and keeps polling until the configured SDK
    /// operation timeout expires. Unlike [`Self::wait_for`], this method does
    /// not subscribe to raw pane output and does not send SDK byte-wait
    /// cancellation requests.
    pub async fn wait_for_text(&self, text: impl AsRef<str>) -> Result<()> {
        crate::wait::wait_for_text(self, text.as_ref().to_owned()).await
    }

    /// Arms a daemon-backed wait for future pane output containing `text`.
    ///
    /// This matches the UTF-8 bytes of `text` in raw output emitted after the
    /// wait is armed. It does not inspect existing snapshots or retained output
    /// history.
    pub async fn wait_for_text_next(&self, text: impl AsRef<str>) -> Result<ArmedWait> {
        crate::wait::wait_for_text_next(self, text.as_ref().to_owned()).await
    }

    /// Waits until the pane process exits or the pane slot becomes stale.
    ///
    /// The wait polls daemon sticky pane metadata through [`Self::info`].
    /// It does not subscribe to raw output and does not send SDK byte-wait
    /// cancellation requests. `Ok(None)` means the pane was already stale, or
    /// vanished before the daemon could retain exit details for this slot.
    pub async fn wait_exit(&self) -> Result<Option<PaneExitState>> {
        crate::wait::wait_exit(self).await
    }

    /// Alias for [`Self::wait_exit`].
    pub async fn wait_for_exit(&self) -> Result<Option<PaneExitState>> {
        self.wait_exit().await
    }

    /// Subscribes to the live raw pane output as a typed byte stream.
    ///
    /// Setup performs one `subscribe-pane-output` round trip and is
    /// fallible: a stale pane slot, a transport failure, or a refused
    /// daemon capability propagates as [`crate::RmuxError`].
    ///
    /// The returned [`PaneOutputStream`] preserves arbitrary bytes,
    /// pairs every chunk with the daemon's monotonic per-pane sequence,
    /// and surfaces any retained-output gaps as
    /// [`PaneOutputChunk::Lag`](crate::PaneOutputChunk::Lag) without ever
    /// converting raw bytes through `String::from_utf8_lossy`. Dropping
    /// the stream emits exactly one best-effort
    /// `unsubscribe-pane-output` request; if the unsubscribe is refused,
    /// late, or the transport is already gone the drop never closes the
    /// pane, its window/session/process, or the daemon itself.
    pub async fn output_stream(&self) -> Result<PaneOutputStream> {
        self.output_stream_starting_at(PaneOutputStart::Now).await
    }

    /// Subscribes to the live raw pane output, anchoring the cursor at
    /// the requested start position.
    ///
    /// See [`Self::output_stream`] for setup, drop, and lag semantics.
    pub async fn output_stream_starting_at(
        &self,
        start: PaneOutputStart,
    ) -> Result<PaneOutputStream> {
        PaneOutputStream::open(self.transport.clone(), self.target.to_proto(), start).await
    }

    /// Collects bounded raw pane output bytes until the pane process exits.
    ///
    /// Collection starts at the live output cursor, retains at most
    /// `max_bytes`, and keeps waiting for pane exit even after the cap is
    /// reached. Returned bytes are raw pane-output bytes; lag notices are
    /// reported on the returned [`CollectedPaneOutput`] and are not spliced
    /// into the byte buffer.
    pub async fn collect_output_until_exit(&self, max_bytes: usize) -> Result<CollectedPaneOutput> {
        crate::extract::collect_output_until_exit(self, max_bytes).await
    }

    /// Collects bounded raw pane output from the requested stream start until
    /// the pane process exits.
    ///
    /// See [`Self::collect_output_until_exit`] for cap, lag, and byte
    /// preservation semantics.
    pub async fn collect_output_until_exit_starting_at(
        &self,
        start: PaneOutputStart,
        max_bytes: usize,
    ) -> Result<CollectedPaneOutput> {
        crate::extract::collect_output_until_exit_starting_at(self, start, max_bytes).await
    }

    /// Subscribes to the live pane output rendered into UTF-8 lines.
    ///
    /// Setup is fallible (see [`Self::output_stream`]). Beyond the raw
    /// stream the line stream applies two well-isolated transformations:
    /// it splits on the LF byte `b'\n'` and runs each completed line
    /// through `String::from_utf8_lossy`, replacing every byte that is
    /// not valid UTF-8 with the Unicode replacement character `U+FFFD`.
    /// Bytes between LFs stay buffered until the next LF arrives, and a
    /// daemon-side lag drops the in-flight partial line; both
    /// transformations are documented in detail on the
    /// [`crate::events::streams`] module. Drop semantics match
    /// [`Self::output_stream`].
    pub async fn line_stream(&self) -> Result<PaneLineStream> {
        self.line_stream_starting_at(PaneOutputStart::Now).await
    }

    /// Subscribes to rendered output lines, anchoring the cursor at the
    /// requested start position.
    pub async fn line_stream_starting_at(&self, start: PaneOutputStart) -> Result<PaneLineStream> {
        let inner = self.output_stream_starting_at(start).await?;
        Ok(PaneLineStream::wrap(inner))
    }

    /// Returns the live daemon pane identity for this slot, when it is
    /// currently listed.
    ///
    /// Returns `Ok(None)` (rather than an error) for a stale slot, mirroring
    /// the [`Window`](super::Window)-handle stale-slot semantics.
    pub async fn id(&self) -> Result<Option<PaneId>> {
        Ok(current_pane_entry(&self.transport, &self.target)
            .await?
            .map(|entry| entry.pane_id))
    }

    /// Checks whether this exact pane slot is currently listed by the
    /// daemon.
    pub async fn exists(&self) -> Result<bool> {
        Ok(self.id().await?.is_some())
    }

    /// Returns a sticky info snapshot scoped to this pane's session,
    /// window, and pane.
    ///
    /// The snapshot is assembled from live `list-sessions`,
    /// `list-windows`, `list-panes`, and `display-message -p` responses so
    /// pane process state — running pid, exit state, geometry — reflects
    /// the daemon's current view rather than any handle-cached value.
    /// Stale slots return what is still observable: a session-only
    /// snapshot when the window or pane is gone, or an empty snapshot
    /// when the session itself is gone.
    pub async fn info(&self) -> Result<InfoSnapshot> {
        pane_info_snapshot(&self.transport, &self.target).await
    }

    /// Captures the live pane grid as a [`PaneSnapshot`].
    ///
    /// The captured grid is read directly from the daemon's live
    /// rmux-core screen — the same in-memory grid that the crate-private
    /// terminal parser feeds from PTY output — so dimensions, cursor
    /// state, and per-cell glyph/attribute/colour data round-trip without
    /// any `capture-pane -p` text reconstruction step. Wide-glyph padding
    /// is preserved as padding cells in the row-major layout, raw bytes
    /// that are not valid UTF-8 stay isolated to the cell text payload
    /// rather than reaching helper output, and the daemon-derived
    /// [`revision`](PaneSnapshot::revision) is non-zero for a live pane
    /// and changes whenever any observable pane field mutates — output,
    /// resize, clear, exit. Stale slots resolve to a default empty
    /// snapshot whose revision is `0`, distinct from any prior live
    /// revision.
    pub async fn snapshot(&self) -> Result<PaneSnapshot> {
        pane_snapshot(&self.transport, &self.target).await
    }

    /// Captures a fresh snapshot and searches its rendered visible text for
    /// the first literal match.
    ///
    /// This is a lossy rendered-text helper built from
    /// [`PaneSnapshot::visible_lines`]. It does not inspect raw output bytes
    /// and does not use any daemon/core regex search surface.
    pub async fn find_text(&self, text: impl AsRef<str>) -> Result<Option<PaneTextMatch>> {
        crate::extract::find_text(self, text.as_ref().to_owned()).await
    }

    /// Captures a fresh snapshot and returns every literal rendered-text
    /// match, including overlapping matches on the same visible line.
    ///
    /// See [`Self::find_text`] for rendered-text and coordinate semantics.
    pub async fn find_text_all(&self, text: impl AsRef<str>) -> Result<Vec<PaneTextMatch>> {
        crate::extract::find_text_all(self, text.as_ref().to_owned()).await
    }

    /// Sends literal UTF-8 text bytes to this pane through the daemon.
    ///
    /// The payload is not interpreted as key names, does not expand tmux
    /// formats, and does not receive an implicit trailing newline. Use
    /// [`send_key`](Self::send_key) when a tmux key token such as `Enter`
    /// should be interpreted as a key press.
    pub async fn send_text(&self, text: impl AsRef<str>) -> Result<()> {
        send_text(&self.transport, &self.target, text.as_ref()).await
    }

    /// Sends one tmux-compatible key token to this pane through the daemon.
    ///
    /// Tokens keep the daemon's existing `send-keys` semantics: known key
    /// names such as `Enter` are encoded as keys, while ordinary text tokens
    /// are forwarded as their bytes by the server.
    pub async fn send_key(&self, key: impl Into<String>) -> Result<()> {
        send_key(&self.transport, &self.target, key.into()).await
    }

    /// Requests an absolute pane size through the daemon.
    ///
    /// Only dimensions that differ from the daemon's current pane details are
    /// sent. The daemon still applies normal `resize-pane` layout rules, so
    /// linked panes, borders, and neighboring panes can constrain the final
    /// geometry. No pane identity is cached by this handle.
    pub async fn resize(&self, size: TerminalSizeSpec) -> Result<()> {
        resize_to_size(&self.transport, &self.target, size).await
    }

    /// Consumes this handle and kills the addressed pane through the daemon.
    ///
    /// A stale handle is treated as an idempotent no-op and returns
    /// [`PaneCloseOutcome::AlreadyClosed`]. Dropping a [`Pane`] handle remains
    /// inert; this consuming method is the SDK operation that explicitly
    /// closes the pane slot and its process.
    pub async fn close(self) -> Result<PaneCloseOutcome> {
        close_pane(&self.transport, self.target).await
    }

    /// Consumes this handle without sending any daemon request.
    ///
    /// Detaching an SDK handle is equivalent to dropping it: the addressed
    /// pane slot, process, subscriptions owned elsewhere, and daemon state are
    /// left untouched. Use [`Self::close`] when the pane itself should be
    /// killed.
    pub fn detach(self) {}

    /// Splits this pane and returns a handle for the freshly spawned pane.
    ///
    /// The direction names where the new pane lands relative to this one:
    /// `Right`/`Left` create a side-by-side arrangement (vertical divider),
    /// `Up`/`Down` create a stacked arrangement (horizontal divider).
    /// `Left` and `Up` map to tmux's `-b` flag — the new pane is inserted
    /// *before* this one on the chosen axis.
    pub async fn split(&self, direction: SplitDirection) -> Result<Self> {
        let new_target = split_pane(&self.transport, &self.target, direction).await?;
        Ok(Self::new(
            new_target,
            self.endpoint.clone(),
            self.default_timeout,
            self.transport.clone(),
        ))
    }

    /// Respawns the process in this pane slot through the daemon.
    ///
    /// The addressed slot and stable `%N`/[`PaneId`] are preserved by the
    /// daemon. `options.kill` mirrors `respawn-pane -k`: a running process is
    /// rejected unless that flag is set, while a dead pane can be respawned
    /// without it. The daemon resets the pane transcript, parser state,
    /// scrollback, and retained output before exposing output from the fresh
    /// lifecycle generation.
    pub async fn respawn(&self, options: PaneRespawnOptions) -> Result<PaneRef> {
        respawn_pane(&self.transport, &self.target, options).await
    }
}

impl fmt::Debug for Pane {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Pane")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[path = "pane/tests.rs"]
mod tests;
