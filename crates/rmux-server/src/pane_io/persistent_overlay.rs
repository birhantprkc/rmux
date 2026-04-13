use std::collections::VecDeque;

use tokio::sync::mpsc;

use super::types::{AttachControl, AttachTarget, OpenAttachTarget, OverlayFrame};

pub(super) fn discard_stale_persistent_overlays(
    attach_controls: Option<&mut mpsc::UnboundedReceiver<AttachControl>>,
    deferred_controls: &mut VecDeque<AttachControl>,
    barrier_state_id: u64,
) {
    let mut retained_controls = VecDeque::with_capacity(deferred_controls.len());
    while let Some(control) = deferred_controls.pop_front() {
        match control {
            AttachControl::Switch(next_target)
                if is_stale_persistent_switch(Some(barrier_state_id), next_target.as_ref()) => {}
            AttachControl::Overlay(overlay)
                if overlay
                    .persistent_state_id
                    .is_some_and(|state_id| state_id < barrier_state_id) => {}
            other => retained_controls.push_back(other),
        }
    }
    *deferred_controls = retained_controls;

    let Some(control_rx) = attach_controls else {
        return;
    };
    while let Ok(control) = control_rx.try_recv() {
        match control {
            AttachControl::Switch(next_target)
                if is_stale_persistent_switch(Some(barrier_state_id), next_target.as_ref()) => {}
            AttachControl::Overlay(overlay)
                if overlay
                    .persistent_state_id
                    .is_some_and(|state_id| state_id < barrier_state_id) => {}
            other => deferred_controls.push_back(other),
        }
    }
}

pub(super) fn advance_persistent_overlay_state(
    current_state_id: &mut Option<u64>,
    attach_controls: Option<&mut mpsc::UnboundedReceiver<AttachControl>>,
    deferred_controls: &mut VecDeque<AttachControl>,
    barrier_state_id: u64,
) {
    if current_state_id.is_some_and(|current| barrier_state_id < current) {
        return;
    }
    *current_state_id = Some(barrier_state_id);
    discard_stale_persistent_overlays(attach_controls, deferred_controls, barrier_state_id);
}

pub(super) fn prime_persistent_overlay_barriers(
    current_state_id: &mut Option<u64>,
    attach_controls: Option<&mut mpsc::UnboundedReceiver<AttachControl>>,
    deferred_controls: &mut VecDeque<AttachControl>,
) {
    let Some(control_rx) = attach_controls else {
        return;
    };

    while let Ok(control) = control_rx.try_recv() {
        deferred_controls.push_back(control);
    }

    let mut latest_barrier = None::<u64>;
    let mut retained_controls = VecDeque::with_capacity(deferred_controls.len());
    while let Some(control) = deferred_controls.pop_front() {
        match control {
            AttachControl::AdvancePersistentOverlayState(state_id) => {
                latest_barrier =
                    Some(latest_barrier.map_or(state_id, |current| current.max(state_id)));
            }
            other => retained_controls.push_back(other),
        }
    }
    *deferred_controls = retained_controls;

    if let Some(barrier_state_id) = latest_barrier {
        advance_persistent_overlay_state(
            current_state_id,
            Some(control_rx),
            deferred_controls,
            barrier_state_id,
        );
    }
}

pub(super) fn is_stale_persistent_switch(
    current_state_id: Option<u64>,
    next_target: &AttachTarget,
) -> bool {
    match (current_state_id, next_target.persistent_overlay_state_id) {
        (Some(current_state_id), Some(incoming_state_id)) => incoming_state_id < current_state_id,
        _ => false,
    }
}

pub(super) fn accept_persistent_overlay_state(
    current_state_id: &mut Option<u64>,
    overlay: &OverlayFrame,
) -> bool {
    let Some(incoming_state_id) = overlay.persistent_state_id else {
        return true;
    };
    if current_state_id.is_some_and(|current| incoming_state_id < current) {
        return false;
    }
    *current_state_id = Some(incoming_state_id);
    true
}

pub(super) fn update_persistent_overlay_cache(
    cache: &mut Option<Vec<u8>>,
    visible: &mut bool,
    overlay: &OverlayFrame,
) {
    if !overlay.persistent {
        return;
    }
    if overlay.frame.is_empty() {
        *cache = None;
        *visible = false;
    } else {
        *cache = Some(overlay.frame.clone());
        *visible = true;
    }
}

pub(super) fn clear_then_base_frame(current_target: &OpenAttachTarget) -> Vec<u8> {
    let mut frame = Vec::with_capacity(current_target.render_frame.len() + 10);
    frame.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
    frame.extend_from_slice(&current_target.render_frame);
    frame
}

pub(super) fn replacement_persistent_overlay_frame(
    cache: &Option<Vec<u8>>,
    visible: bool,
    next_target: &AttachTarget,
) -> Option<Vec<u8>> {
    if !visible || next_target.persistent_overlay_state_id.is_none() {
        return None;
    }
    cache.clone()
}

pub(super) fn persistent_overlay_replacement_pending(
    controls: &VecDeque<AttachControl>,
    current_state_id: Option<u64>,
) -> bool {
    let Some(current_state_id) = current_state_id else {
        return false;
    };
    controls.iter().any(|control| match control {
        AttachControl::Switch(target) => target
            .persistent_overlay_state_id
            .is_some_and(|state_id| state_id >= current_state_id),
        AttachControl::Overlay(overlay) => {
            overlay.persistent
                && !overlay.frame.is_empty()
                && overlay
                    .persistent_state_id
                    .is_none_or(|state_id| state_id >= current_state_id)
        }
        _ => false,
    })
}
