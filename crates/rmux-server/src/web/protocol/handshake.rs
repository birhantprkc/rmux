//! Pre-ready handshake: client hello, server challenge, and the encrypted auth
//! frame. Failures before token/PIN authentication collapse to
//! [`super::HANDSHAKE_REJECTED`] on the wire (no PIN/identity oracle).

use std::io;

use serde::Serialize;
use tokio::time::timeout;

use super::{
    AuthMessage, AuthWireMessage, AUTH_FRAME_TIMEOUT, CAPACITY_REACHED, HANDSHAKE_REJECTED,
    PRE_AUTH_TIMEOUT, SERVER_CAPABILITIES, WEB_SHARE_PROTOCOL_VERSION,
};
use crate::web::crypto::{parse_client_hello, ClientHello, FrameOpener, E2EE_CAPABILITY};
use crate::web::websocket::{WebSocket, WebSocketMessage};

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerHandshakeMessage<'a> {
    Challenge {
        protocol_version: u16,
        capabilities: &'static [&'static str],
        server_nonce: &'a str,
        server_public: &'a str,
        server_ml_kem_ct: &'a str,
    },
}

/// Reads and parses the v4 client hello.
///
/// On failure the `Err` carries the PRECISE internal reason for server-side
/// logging only; the caller collapses every pre-ready failure to
/// [`super::HANDSHAKE_REJECTED`] on the wire.
pub(crate) async fn read_client_hello(socket: &mut WebSocket) -> Result<ClientHello, &'static str> {
    let message = timeout(PRE_AUTH_TIMEOUT, socket.read_message())
        .await
        .map_err(|_| "hello_timeout")?
        .map_err(|_| "invalid_hello_frame")?;
    let WebSocketMessage::Text(text) = message else {
        return Err("hello_must_be_text");
    };
    parse_client_hello(&text, WEB_SHARE_PROTOCOL_VERSION).map_err(|_| "invalid_hello")
}

/// Serializes the v4 challenge to its EXACT wire text.
///
/// Split from sending so the caller can bind the same bytes it transmits into
/// the session key schedule (handshake transcript binding).
pub(crate) fn build_challenge(
    server_nonce: &str,
    server_public_b64: &str,
    server_ml_kem_ct_b64: &str,
) -> io::Result<String> {
    let payload = ServerHandshakeMessage::Challenge {
        protocol_version: WEB_SHARE_PROTOCOL_VERSION,
        capabilities: SERVER_CAPABILITIES,
        server_nonce,
        server_public: server_public_b64,
        server_ml_kem_ct: server_ml_kem_ct_b64,
    };
    serde_json::to_string(&payload).map_err(|error| io::Error::other(error.to_string()))
}

/// Sends a pre-built handshake text message on the raw socket.
pub(crate) async fn send_text(socket: &mut WebSocket, text: &str) -> io::Result<()> {
    socket.write_text(text).await
}

/// Reads and decrypts the first (auth) frame.
///
/// On failure the `Err` carries the PRECISE internal reason for server-side
/// logging only; the caller collapses to [`super::HANDSHAKE_REJECTED`] on the wire.
pub(crate) async fn read_auth_message(
    socket: &mut WebSocket,
    opener: &mut FrameOpener,
) -> Result<AuthMessage, &'static str> {
    let message = timeout(AUTH_FRAME_TIMEOUT, socket.read_message())
        .await
        .map_err(|_| "auth_timeout")?
        .map_err(|_| "invalid_auth_frame")?;
    let WebSocketMessage::Binary(frame) = message else {
        return Err("auth_must_be_encrypted");
    };
    let WebSocketMessage::Text(text) = opener
        .open_message(&frame)
        .map_err(|_| "invalid_encrypted_auth")?
    else {
        return Err("auth_must_be_text");
    };
    let wire = serde_json::from_str::<AuthWireMessage>(&text).map_err(|_| "invalid_auth_json")?;
    if wire.kind != "auth" {
        return Err("first_frame_must_auth");
    }
    if wire.protocol_version != WEB_SHARE_PROTOCOL_VERSION {
        return Err("protocol_version_mismatch");
    }
    if !wire
        .capabilities
        .iter()
        .any(|capability| capability == E2EE_CAPABILITY)
    {
        return Err("missing_e2ee_capability");
    }
    Ok(AuthMessage { pin: wire.pin })
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AuthClose {
    pub(crate) reason: &'static str,
    pub(crate) wire_close: (u16, &'static str),
}

/// Maps an open-token error to a precise internal reason and safe wire close.
///
/// Role capacity is only reached after the token-authenticated channel and PIN
/// check succeed, so it can use a distinct close code without becoming a
/// token/PIN oracle. All other auth errors stay collapsed.
pub(crate) fn close_for_auth_error(error: &str) -> AuthClose {
    if error.contains("spectator limit") {
        return AuthClose {
            reason: "spectator_cap_reached",
            wire_close: CAPACITY_REACHED,
        };
    }
    if error.contains("operator limit") {
        return AuthClose {
            reason: "operator_cap_reached",
            wire_close: CAPACITY_REACHED,
        };
    }
    if error.contains("missing web-share pairing code") {
        return AuthClose {
            reason: "pin_required",
            wire_close: HANDSHAKE_REJECTED,
        };
    }
    if error.contains("no operator") {
        return AuthClose {
            reason: "operator_not_available",
            wire_close: HANDSHAKE_REJECTED,
        };
    }
    AuthClose {
        reason: "invalid_auth",
        wire_close: HANDSHAKE_REJECTED,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn challenge_serialization_is_wire_stable() {
        let challenge = super::build_challenge("nonce", "server-public", "ml-kem-ct")
            .expect("challenge serialization");

        assert_eq!(
            challenge,
            r#"{"type":"challenge","protocol_version":1,"capabilities":["e2ee-token-auth","terminal-palette-v1"],"server_nonce":"nonce","server_public":"server-public","server_ml_kem_ct":"ml-kem-ct"}"#
        );
    }

    #[test]
    fn role_capacity_auth_errors_use_capacity_close_code() {
        for error in [
            "web-share spectator limit reached",
            "web-share operator limit reached",
        ] {
            let close = super::close_for_auth_error(error);
            assert_eq!(close.wire_close, super::CAPACITY_REACHED);
        }
    }

    #[test]
    fn non_capacity_auth_errors_remain_collapsed() {
        for error in [
            "invalid web-share pairing code",
            "missing web-share pairing code",
            "web-share connection limit reached",
            "web-share does not exist or has expired",
            "web-share has no operator access",
        ] {
            let close = super::close_for_auth_error(error);
            assert_eq!(close.wire_close, super::HANDSHAKE_REJECTED);
        }
    }
}
