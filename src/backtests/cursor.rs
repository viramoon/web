use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use openssl::{
    error::ErrorStack,
    rand::rand_bytes,
    symm::{Cipher, decrypt_aead, encrypt_aead},
};
use std::sync::LazyLock;
use zeroize::Zeroizing;

const PREFIX: &str = "v1.";
pub(super) const PATTERN: &str = r"^v1\.[A-Za-z0-9_-]{48}$";
const CONTEXT: &[u8] = b"web.backtests.public-cursor.v1";
const NONCE_LEN: usize = 12;
const POSITION_LEN: usize = 8;
const TAG_LEN: usize = 16;
const SEALED_LEN: usize = NONCE_LEN + POSITION_LEN + TAG_LEN;

// Public continuations are scoped to this web process. It holds no cursor table.
static CODEC: LazyLock<Result<Codec, ErrorStack>> = LazyLock::new(Codec::generate);

pub(super) fn seal(position: i64) -> Result<String, ()> {
    CODEC
        .as_ref()
        .map_err(|_| ())?
        .seal(position)
        .map_err(|_| ())
}

pub(super) fn parse(value: &str) -> Option<i64> {
    if value.starts_with(PREFIX) {
        return CODEC.as_ref().ok()?.open(value);
    }
    value.parse::<i64>().ok().filter(|id| *id >= 0)
}

struct Codec(Zeroizing<[u8; 32]>);

impl Codec {
    fn generate() -> Result<Self, ErrorStack> {
        let mut key = Zeroizing::new([0; 32]);
        rand_bytes(&mut *key)?;
        Ok(Self(key))
    }

    fn seal(&self, position: i64) -> Result<String, ErrorStack> {
        let mut nonce = [0; NONCE_LEN];
        let mut tag = [0; TAG_LEN];
        rand_bytes(&mut nonce)?;
        let position = encrypt_aead(
            Cipher::aes_256_gcm(),
            &*self.0,
            Some(&nonce),
            CONTEXT,
            &position.to_be_bytes(),
            &mut tag,
        )?;
        let mut sealed = Vec::with_capacity(SEALED_LEN);
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&position);
        sealed.extend_from_slice(&tag);
        Ok(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(sealed)))
    }

    fn open(&self, value: &str) -> Option<i64> {
        let encoded = value.strip_prefix(PREFIX)?;
        if encoded.len() != SEALED_LEN * 4 / 3 {
            return None;
        }
        let sealed: [u8; SEALED_LEN] = URL_SAFE_NO_PAD.decode(encoded).ok()?.try_into().ok()?;
        let (nonce, rest) = sealed.split_at(NONCE_LEN);
        let (position, tag) = rest.split_at(POSITION_LEN);
        let position = decrypt_aead(
            Cipher::aes_256_gcm(),
            &*self.0,
            Some(nonce),
            CONTEXT,
            position,
            tag,
        )
        .ok()?;
        let position = i64::from_be_bytes(position.try_into().ok()?);
        (position > 0).then_some(position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_cursor_is_randomized_and_authenticated() {
        let codec = Codec::generate().unwrap();
        let token = codec.seal(208).unwrap();
        assert_eq!(codec.open(&token), Some(208));
        assert_ne!(token, codec.seal(208).unwrap());
        assert_eq!(Codec::generate().unwrap().open(&token), None);
        assert_eq!(codec.open(&format!("{token}A")), None);

        let wire = token.strip_prefix(PREFIX).unwrap();
        let mut bytes = URL_SAFE_NO_PAD.decode(wire).unwrap();
        assert_eq!(bytes.len(), SEALED_LEN);
        for index in [0, NONCE_LEN, NONCE_LEN + POSITION_LEN] {
            bytes[index] ^= 1;
            assert_eq!(
                codec.open(&format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(&bytes))),
                None
            );
            bytes[index] ^= 1;
        }
    }

    #[test]
    fn cursor_parser_keeps_legacy_nonnegative_decimal_input() {
        assert_eq!(parse("0"), Some(0));
        assert_eq!(parse("7"), Some(7));
        assert_eq!(parse("9223372036854775807"), Some(i64::MAX));
        assert_eq!(parse("-1"), None);
        assert_eq!(parse("9223372036854775808"), None);
        assert_eq!(parse("v1."), None);
        assert_eq!(parse("v1.invalid"), None);
        assert_eq!(parse(&seal(208).unwrap()), Some(208));
    }
}
