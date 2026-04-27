use rmux_core::{key_code_lookup_bits, key_string_lookup_key};
use rmux_proto::{
    ErrorResponse, OptionName, PaneTarget, Response, RmuxError, SendKeysResponse, Target,
};

use super::display_message_context;
use crate::format_runtime::render_runtime_template;
use crate::input_keys::{encode_key, encode_mouse_event, ExtendedKeyFormat};
use crate::keys::parse_key_code;
use crate::pane_terminals::{session_not_found, HandlerState};

pub(super) fn write_bytes_to_target(
    state: &HandlerState,
    target: &PaneTarget,
    bytes: &[u8],
    key_count: usize,
) -> Response {
    match write_bytes_to_target_io(state, target, bytes) {
        Ok(()) => Response::SendKeys(SendKeysResponse { key_count }),
        Err(error) => Response::Error(ErrorResponse { error }),
    }
}

pub(super) fn write_bytes_to_target_io(
    state: &HandlerState,
    target: &PaneTarget,
    bytes: &[u8],
) -> Result<(), RmuxError> {
    let session_name = target.session_name().clone();
    let window_index = target.window_index();
    let pane_index = target.pane_index();
    let master = state.pane_master_in_window(&session_name, window_index, pane_index)?;
    if bytes.is_empty() {
        return Ok(());
    }
    master.write_all(bytes).map_err(|error| {
        RmuxError::Server(format!(
            "failed to write to pane {}:{}.{}: {}",
            session_name, window_index, pane_index, error
        ))
    })
}

pub(super) fn encode_tokens_for_target(
    state: &HandlerState,
    target: &PaneTarget,
    tokens: &[String],
) -> Result<Vec<u8>, RmuxError> {
    let mut bytes = Vec::new();
    for token in tokens {
        if let Some(key) = parse_key_code(token) {
            let Some(encoded) = encode_key_for_target(state, target, key)? else {
                return Err(RmuxError::Server(format!(
                    "key {} cannot be sent to a pane",
                    key_string_lookup_key(key_code_lookup_bits(key), false)
                )));
            };
            bytes.extend_from_slice(&encoded);
        } else {
            bytes.extend_from_slice(token.as_bytes());
        }
    }
    Ok(bytes)
}

pub(super) fn encode_key_for_target(
    state: &HandlerState,
    target: &PaneTarget,
    key: rmux_core::KeyCode,
) -> Result<Option<Vec<u8>>, RmuxError> {
    let pane_id = state
        .sessions
        .session(target.session_name())
        .and_then(|session| session.window_at(target.window_index()))
        .and_then(|window| window.pane(target.pane_index()))
        .map(|pane| pane.id())
        .ok_or_else(|| {
            RmuxError::invalid_target(target.to_string(), "pane index does not exist in session")
        })?;
    let pane_mode = state
        .pane_screen_state(target.session_name(), pane_id)
        .map(|screen_state| screen_state.mode)
        .unwrap_or_default();
    let format =
        ExtendedKeyFormat::parse(state.options.resolve(None, OptionName::ExtendedKeysFormat));
    Ok(encode_key(pane_mode, format, key))
}

pub(super) fn encode_mouse_for_target(
    state: &HandlerState,
    target: &PaneTarget,
    event: &crate::mouse::AttachedMouseEvent,
) -> Result<Vec<u8>, RmuxError> {
    let session = state
        .sessions
        .session(target.session_name())
        .ok_or_else(|| session_not_found(target.session_name()))?;
    let window = session.window_at(target.window_index()).ok_or_else(|| {
        RmuxError::invalid_target(target.to_string(), "window index does not exist in session")
    })?;
    let pane = window.pane(target.pane_index()).ok_or_else(|| {
        RmuxError::invalid_target(target.to_string(), "pane index does not exist in session")
    })?;
    if event.ignore || event.pane_id != Some(pane.id()) {
        return Ok(Vec::new());
    }

    let pane_mode = state
        .pane_screen_state(target.session_name(), pane.id())
        .map(|screen_state| screen_state.mode)
        .unwrap_or_default();
    let adjusted_y = match event.status_at {
        Some(0) if event.raw.y >= event.status_lines => event.raw.y - event.status_lines,
        _ => event.raw.y,
    };
    if event.raw.x < pane.geometry().x()
        || event.raw.x >= pane.geometry().x().saturating_add(pane.geometry().cols())
        || adjusted_y < pane.geometry().y()
        || adjusted_y >= pane.geometry().y().saturating_add(pane.geometry().rows())
    {
        return Ok(Vec::new());
    }
    let x = event.raw.x - pane.geometry().x();
    let y = adjusted_y - pane.geometry().y();
    Ok(encode_mouse_event(pane_mode, &event.raw, x, y).unwrap_or_default())
}

pub(super) fn expand_send_key_tokens(
    state: &HandlerState,
    target: &PaneTarget,
    tokens: &[String],
    expand_formats: bool,
) -> Result<Vec<String>, RmuxError> {
    if !expand_formats {
        return Ok(tokens.to_vec());
    }

    let (_, runtime) = display_message_context(state, &Target::Pane(target.clone()), 0)?;
    Ok(tokens
        .iter()
        .map(|token| render_runtime_template(token, &runtime, false))
        .collect())
}
