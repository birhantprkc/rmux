use base64::Engine;
use rmux_proto::RmuxError;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub(super) fn random_share_id() -> Result<String, RmuxError> {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut bytes = [0u8; 5];
    getrandom::fill(&mut bytes).map_err(random_error)?;
    let value = u64::from_be_bytes([0, 0, 0, bytes[0], bytes[1], bytes[2], bytes[3], bytes[4]]);
    let mut out = String::with_capacity(8);
    for shift in (0..40).step_by(5).rev() {
        let index = ((value >> shift) & 0x1f) as usize;
        out.push(ALPHABET[index] as char);
    }
    Ok(out)
}

pub(super) fn random_pairing_code() -> Result<String, RmuxError> {
    loop {
        let mut bytes = [0u8; 3];
        getrandom::fill(&mut bytes).map_err(random_error)?;
        let value = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
        if value < 16_000_000 {
            return Ok(format!("{:06}", value % 1_000_000));
        }
    }
}

pub(super) fn random_token() -> Result<String, RmuxError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(random_error)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SecretHash([u8; 32]);

impl SecretHash {
    pub(crate) fn from_secret(secret: &str) -> Self {
        let digest = Sha256::digest(secret.as_bytes());
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        Self(out)
    }

    pub(super) const fn as_bytes(self) -> [u8; 32] {
        self.0
    }

    pub(crate) fn token_id(self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"rmux-token-id-v1");
        hasher.update(self.0);
        let digest = hasher.finalize();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&digest[..16])
    }
}

pub(super) fn valid_token_id_shape(token_id: &str) -> bool {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token_id)
        .is_ok_and(|bytes| bytes.len() == 16)
}

pub(super) fn secret_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    left.len() == right.len() && bool::from(left.ct_eq(right))
}

fn random_error(error: getrandom::Error) -> RmuxError {
    RmuxError::Server(format!("failed to create web-share secret: {error}"))
}
