use serde::{Deserialize, Deserializer, Serialize, de};
use utoipa::{
    PartialSchema, ToSchema,
    openapi::{
        RefOr,
        schema::{ObjectBuilder, Schema, Type},
    },
};

// Identifier grammar from finance protocol/v1. Registry membership is a separate
// check that requires a finance instrument discovery contract.
pub(crate) const INSTRUMENT_ID_PATTERN: &str = concat!(
    r"^(?=[A-Z0-9_.-]{1,96}\.[A-Z0-9][A-Z0-9_-]{0,31}(?![\s\S]))",
    r"[A-Z0-9][A-Z0-9_-]*(?:\.[A-Z0-9][A-Z0-9_-]*)*\.[A-Z0-9][A-Z0-9_-]{0,31}(?![\s\S])"
);

pub(crate) fn valid_instrument_id(value: &str) -> bool {
    fn upper_digit(byte: u8) -> bool {
        byte.is_ascii_uppercase() || byte.is_ascii_digit()
    }
    fn segment(value: &str) -> bool {
        let mut bytes = value.bytes();
        bytes.next().is_some_and(upper_digit)
            && bytes.all(|byte| upper_digit(byte) || matches!(byte, b'_' | b'-'))
    }
    value.rsplit_once('.').is_some_and(|(local, venue)| {
        local.len() <= 96 && venue.len() <= 32 && local.split('.').all(segment) && segment(venue)
    })
}

pub(crate) fn valid_token(value: &str) -> bool {
    fn lower_digit(byte: u8) -> bool {
        byte.is_ascii_lowercase() || byte.is_ascii_digit()
    }
    let mut bytes = value.bytes();
    value.len() <= 64
        && bytes.next().is_some_and(lower_digit)
        && bytes.all(|byte| lower_digit(byte) || matches!(byte, b'_' | b'-' | b'.'))
}

pub(crate) fn parse_u64(value: &str) -> Option<u64> {
    let number: u64 = value.parse().ok()?;
    (number.to_string() == value).then_some(number)
}

#[derive(Clone, Serialize, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String, pattern = r"^[a-z0-9][a-z0-9_.-]{0,63}(?![\s\S])")]
pub(crate) struct ProductToken(String);

impl ProductToken {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ProductToken {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if valid_token(&value) {
            Ok(Self(value))
        } else {
            Err(de::Error::custom("invalid market token"))
        }
    }
}

#[derive(Clone, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct InstrumentId(String);

impl PartialSchema for InstrumentId {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new()
            .schema_type(Type::String)
            .pattern(Some(INSTRUMENT_ID_PATTERN))
            .into()
    }
}

impl ToSchema for InstrumentId {}

impl InstrumentId {
    pub(crate) fn parse(value: String) -> Option<Self> {
        valid_instrument_id(&value).then_some(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for InstrumentId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(deserializer)?)
            .ok_or_else(|| de::Error::custom("invalid instrument identifier"))
    }
}
