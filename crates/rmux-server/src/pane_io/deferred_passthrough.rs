#[cfg(any(unix, windows))]
use super::passthrough::render_passthroughs;
#[cfg(any(unix, windows))]
use super::types::OpenAttachTarget;
#[cfg(any(unix, windows))]
use super::wire::emit_attach_bytes;
#[cfg(any(unix, windows))]
use rmux_core::TerminalPassthrough;
#[cfg(any(unix, windows))]
use rmux_ipc::LocalStream;
#[cfg(any(unix, windows))]
use std::io;

#[cfg(any(unix, windows))]
const DEFERRED_PASSTHROUGH_LIMIT: usize = 16;

#[cfg(any(unix, windows))]
pub(super) fn defer_passthroughs(
    deferred_passthroughs: &mut Vec<TerminalPassthrough>,
    passthroughs: Vec<TerminalPassthrough>,
) {
    if passthroughs.is_empty() {
        return;
    }
    deferred_passthroughs.extend(passthroughs);
    let overflow = deferred_passthroughs
        .len()
        .saturating_sub(DEFERRED_PASSTHROUGH_LIMIT);
    if overflow > 0 {
        deferred_passthroughs.drain(..overflow);
    }
}

#[cfg(any(unix, windows))]
pub(super) fn take_passthrough_frame(
    current_target: &OpenAttachTarget,
    deferred_passthroughs: &mut Vec<TerminalPassthrough>,
) -> Vec<u8> {
    if deferred_passthroughs.is_empty() {
        return Vec::new();
    }
    let passthroughs = std::mem::take(deferred_passthroughs);
    render_passthroughs(current_target, &passthroughs)
}

#[cfg(any(unix, windows))]
pub(super) async fn flush_deferred_passthroughs(
    stream: &LocalStream,
    current_target: &OpenAttachTarget,
    deferred_passthroughs: &mut Vec<TerminalPassthrough>,
    persistent_overlay_visible: bool,
    persistent_overlay_cached: bool,
) -> io::Result<()> {
    if persistent_overlay_visible || persistent_overlay_cached {
        return Ok(());
    }
    let frame = take_passthrough_frame(current_target, deferred_passthroughs);
    if frame.is_empty() {
        return Ok(());
    }
    emit_attach_bytes(stream, &frame).await
}
