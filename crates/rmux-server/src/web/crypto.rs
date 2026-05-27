use std::io;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine;
use hkdf::Hkdf;
use serde::Deserialize;
use sha2::Sha256;

use super::secrets::SecretHash;
use super::websocket::{WebSocketMessage, WebSocketReader, WebSocketWriter};

pub(super) const E2EE_CAPABILITY: &str = "e2ee-token-auth";

const ENCRYPTED_FRAME: u8 = 0xE0;
const PLAINTEXT_TEXT: u8 = 0x00;
const PLAINTEXT_BINARY: u8 = 0x01;
const CLIENT_DIRECTION: &[u8] = b"c2s";
const SERVER_DIRECTION: &[u8] = b"s2c";
const KEY_INFO_PREFIX: &[u8] = b"rmux web-share e2ee v1 key ";
const NONCE_INFO_PREFIX: &[u8] = b"rmux web-share e2ee v1 nonce ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClientHello {
    pub(super) token_id: String,
    pub(super) client_nonce: String,
}

pub(super) struct EncryptedWebSocketReader {
    reader: WebSocketReader,
    opener: FrameOpener,
}

pub(super) struct EncryptedWebSocketWriter {
    writer: WebSocketWriter,
    sealer: FrameSealer,
}

pub(super) struct FrameOpener {
    cipher: Aes256Gcm,
    nonce_prefix: [u8; 4],
    next_seq: u64,
}

pub(super) struct FrameSealer {
    cipher: Aes256Gcm,
    nonce_prefix: [u8; 4],
    next_seq: u64,
}

pub(super) fn random_handshake_nonce() -> io::Result<String> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|error| {
        io::Error::other(format!("failed to create web-share e2ee nonce: {error}"))
    })?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce))
}

pub(super) fn derive_server_crypto(
    secret: SecretHash,
    token_id: &str,
    client_nonce: &str,
    server_nonce: &str,
) -> io::Result<(FrameOpener, FrameSealer)> {
    let material = SessionMaterial::derive(secret, token_id, client_nonce, server_nonce)?;
    Ok((
        material.opener(CLIENT_DIRECTION)?,
        material.sealer(SERVER_DIRECTION)?,
    ))
}

pub(super) fn parse_client_hello(text: &str, protocol_version: u16) -> Result<ClientHello, ()> {
    let hello = serde_json::from_str::<ClientHelloWire>(text).map_err(|_| ())?;
    if hello.kind != "hello" || hello.protocol_version != protocol_version {
        return Err(());
    }
    if !hello
        .capabilities
        .iter()
        .any(|capability| capability == E2EE_CAPABILITY)
    {
        return Err(());
    }
    if !super::secrets::valid_token_id_shape(&hello.token_id)
        || decode_nonce(&hello.client_nonce).is_err()
    {
        return Err(());
    }
    Ok(ClientHello {
        token_id: hello.token_id,
        client_nonce: hello.client_nonce,
    })
}

impl EncryptedWebSocketReader {
    pub(super) fn new(reader: WebSocketReader, opener: FrameOpener) -> Self {
        Self { reader, opener }
    }

    pub(super) async fn read_message(&mut self) -> io::Result<WebSocketMessage> {
        let message = self.reader.read_message().await?;
        match message {
            WebSocketMessage::Binary(bytes) => self.opener.open_message(&bytes),
            WebSocketMessage::Ping(payload) => Ok(WebSocketMessage::Ping(payload)),
            WebSocketMessage::Pong => Ok(WebSocketMessage::Pong),
            WebSocketMessage::Close => Ok(WebSocketMessage::Close),
            WebSocketMessage::Text(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "plaintext websocket text after e2ee handshake",
            )),
        }
    }
}

impl EncryptedWebSocketWriter {
    pub(super) fn new(writer: WebSocketWriter, sealer: FrameSealer) -> Self {
        Self { writer, sealer }
    }

    pub(super) async fn write_text(&mut self, text: &str) -> io::Result<()> {
        let frame = self.sealer.seal_text(text.as_bytes())?;
        self.writer.write_binary(&frame).await
    }

    pub(super) async fn write_binary(&mut self, payload: &[u8]) -> io::Result<()> {
        let frame = self.sealer.seal_binary(payload)?;
        self.writer.write_binary(&frame).await
    }

    pub(super) async fn write_close(&mut self) -> io::Result<()> {
        self.writer.write_close().await
    }

    pub(super) async fn write_close_code(&mut self, code: u16, reason: &str) -> io::Result<()> {
        self.writer.write_close_code(code, reason).await
    }

    pub(super) async fn write_pong(&mut self, payload: &[u8]) -> io::Result<()> {
        self.writer.write_pong(payload).await
    }
}

impl FrameOpener {
    pub(super) fn open_message(&mut self, frame: &[u8]) -> io::Result<WebSocketMessage> {
        let plain = self.open(frame)?;
        let Some((&kind, body)) = plain.split_first() else {
            return Err(invalid_data("empty e2ee plaintext"));
        };
        match kind {
            PLAINTEXT_TEXT => {
                let text = String::from_utf8(body.to_vec())
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                Ok(WebSocketMessage::Text(text))
            }
            PLAINTEXT_BINARY => Ok(WebSocketMessage::Binary(body.to_vec())),
            _ => Err(invalid_data("unknown e2ee plaintext kind")),
        }
    }

    fn open(&mut self, frame: &[u8]) -> io::Result<Vec<u8>> {
        if frame.len() < 1 + 8 + 16 || frame[0] != ENCRYPTED_FRAME {
            return Err(invalid_data("invalid e2ee frame"));
        }
        let mut seq_bytes = [0u8; 8];
        seq_bytes.copy_from_slice(&frame[1..9]);
        let seq = u64::from_be_bytes(seq_bytes);
        if seq != self.next_seq {
            return Err(invalid_data("out-of-order e2ee frame"));
        }
        let nonce = nonce_from_parts(self.nonce_prefix, seq);
        let aad = &frame[..9];
        let plain = self
            .cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &frame[9..],
                    aad,
                },
            )
            .map_err(|_| invalid_data("e2ee decrypt failed"))?;
        self.next_seq = self.next_seq.saturating_add(1);
        Ok(plain)
    }
}

impl FrameSealer {
    pub(super) fn seal_text(&mut self, text: &[u8]) -> io::Result<Vec<u8>> {
        self.seal(PLAINTEXT_TEXT, text)
    }

    pub(super) fn seal_binary(&mut self, payload: &[u8]) -> io::Result<Vec<u8>> {
        self.seal(PLAINTEXT_BINARY, payload)
    }

    fn seal(&mut self, kind: u8, payload: &[u8]) -> io::Result<Vec<u8>> {
        let seq = self.next_seq;
        let mut plain = Vec::with_capacity(1 + payload.len());
        plain.push(kind);
        plain.extend_from_slice(payload);
        let mut out = Vec::with_capacity(1 + 8 + plain.len() + 16);
        out.push(ENCRYPTED_FRAME);
        out.extend_from_slice(&seq.to_be_bytes());
        let nonce = nonce_from_parts(self.nonce_prefix, seq);
        let ciphertext = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &plain,
                    aad: &out,
                },
            )
            .map_err(|_| io::Error::other("e2ee encrypt failed"))?;
        out.extend_from_slice(&ciphertext);
        self.next_seq = self.next_seq.saturating_add(1);
        Ok(out)
    }
}

#[cfg(test)]
pub(super) fn derive_client_crypto_for_test(
    secret: SecretHash,
    token_id: &str,
    client_nonce: &str,
    server_nonce: &str,
) -> io::Result<(FrameOpener, FrameSealer)> {
    let material = SessionMaterial::derive(secret, token_id, client_nonce, server_nonce)?;
    Ok((
        material.opener(SERVER_DIRECTION)?,
        material.sealer(CLIENT_DIRECTION)?,
    ))
}

struct SessionMaterial {
    secret: [u8; 32],
    salt: Vec<u8>,
}

impl SessionMaterial {
    fn derive(
        secret: SecretHash,
        token_id: &str,
        client_nonce: &str,
        server_nonce: &str,
    ) -> io::Result<Self> {
        let mut salt = Vec::with_capacity(token_id.len() + 32);
        salt.extend_from_slice(token_id.as_bytes());
        salt.extend_from_slice(&decode_nonce(client_nonce)?);
        salt.extend_from_slice(&decode_nonce(server_nonce)?);
        Ok(Self {
            secret: secret.as_bytes(),
            salt,
        })
    }

    fn sealer(&self, direction: &[u8]) -> io::Result<FrameSealer> {
        let (cipher, nonce_prefix) = self.cipher(direction)?;
        Ok(FrameSealer {
            cipher,
            nonce_prefix,
            next_seq: 0,
        })
    }

    fn opener(&self, direction: &[u8]) -> io::Result<FrameOpener> {
        let (cipher, nonce_prefix) = self.cipher(direction)?;
        Ok(FrameOpener {
            cipher,
            nonce_prefix,
            next_seq: 0,
        })
    }

    fn cipher(&self, direction: &[u8]) -> io::Result<(Aes256Gcm, [u8; 4])> {
        let (key, nonce_prefix) = self.derive_direction(direction)?;
        Ok((
            Aes256Gcm::new_from_slice(&key).map_err(|_| io::Error::other("invalid e2ee key"))?,
            nonce_prefix,
        ))
    }

    fn derive_direction(&self, direction: &[u8]) -> io::Result<([u8; 32], [u8; 4])> {
        let hk = Hkdf::<Sha256>::new(Some(&self.salt), &self.secret);
        let mut key = [0u8; 32];
        let mut nonce_prefix = [0u8; 4];
        hk.expand(&direction_info(KEY_INFO_PREFIX, direction), &mut key)
            .map_err(|_| io::Error::other("failed to derive e2ee key"))?;
        hk.expand(
            &direction_info(NONCE_INFO_PREFIX, direction),
            &mut nonce_prefix,
        )
        .map_err(|_| io::Error::other("failed to derive e2ee nonce"))?;
        Ok((key, nonce_prefix))
    }
}

fn direction_info(prefix: &[u8], direction: &[u8]) -> Vec<u8> {
    let mut info = Vec::with_capacity(prefix.len() + direction.len());
    info.extend_from_slice(prefix);
    info.extend_from_slice(direction);
    info
}

fn decode_nonce(nonce: &str) -> io::Result<Vec<u8>> {
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(nonce)
        .map_err(|_| invalid_data("invalid e2ee nonce"))?;
    if decoded.len() != 16 {
        return Err(invalid_data("invalid e2ee nonce length"));
    }
    Ok(decoded)
}

fn nonce_from_parts(prefix: [u8; 4], seq: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&prefix);
    nonce[4..].copy_from_slice(&seq.to_be_bytes());
    nonce
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientHelloWire {
    #[serde(rename = "type")]
    kind: String,
    protocol_version: u16,
    capabilities: Vec<String>,
    token_id: String,
    client_nonce: String,
}

#[cfg(test)]
mod tests {
    use super::{derive_server_crypto, WebSocketMessage};
    use crate::web::secrets::SecretHash;
    use base64::Engine;

    #[test]
    fn token_id_is_stable_and_not_the_secret_hash() {
        let secret = SecretHash::from_secret("token");

        assert_eq!(
            secret.token_id(),
            SecretHash::from_secret("token").token_id()
        );
        assert_ne!(
            secret.token_id(),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret.as_bytes())
        );
    }

    #[test]
    fn e2ee_round_trips_text_and_binary_in_order() {
        let secret = SecretHash::from_secret("token");
        let token_id = secret.token_id();
        let client_nonce = "AQIDBAUGBwgJCgsMDQ4PEA";
        let server_nonce = "EDEODQwLCgkIBwYFBAMCAQ";
        let (mut server_open, mut server_seal) =
            derive_server_crypto(secret, &token_id, client_nonce, server_nonce).expect("derive");
        let (mut client_open, mut client_seal) =
            super::derive_client_crypto_for_test(secret, &token_id, client_nonce, server_nonce)
                .expect("derive client");

        let frame = client_seal.seal_text(br#"{"type":"auth"}"#).expect("seal");
        assert_eq!(
            server_open.open_message(&frame).expect("open"),
            WebSocketMessage::Text(r#"{"type":"auth"}"#.to_owned())
        );

        let frame = server_seal.seal_binary(&[0x10, b'o', b'k']).expect("seal");
        assert_eq!(
            client_open.open_message(&frame).expect("open"),
            WebSocketMessage::Binary(vec![0x10, b'o', b'k'])
        );
    }

    #[test]
    fn client_hello_rejects_missing_e2ee_capability() {
        let text = r#"{"type":"hello","protocol_version":3,"capabilities":["token-auth"],"token_id":"aaaaaaaaaaaaaaaaaaaaaa","client_nonce":"AQIDBAUGBwgJCgsMDQ4PEA"}"#;

        assert!(super::parse_client_hello(text, 3).is_err());
    }
}
