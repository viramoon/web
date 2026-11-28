use super::{MAX_CURSOR_BYTES, SNAPSHOT, SnapshotQuery};
use crate::{
    market_options::{OptionReadError, require_display},
    market_reads::MarketReadError,
    market_types::parse_u64,
};
use chrono::{DateTime, Utc};
use finance_client::models::ProductPolicy;
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;

#[derive(Deserialize)]
pub(super) struct Envelope<'a> {
    pub(super) event: &'a str,
    #[serde(default, deserialize_with = "present_id")]
    pub(super) id: Option<&'a str>,
    pub(super) data: &'a RawValue,
}

impl<'a> Envelope<'a> {
    pub(super) fn parse(raw: &'a str) -> serde_json::Result<Self> {
        parse_object(raw)
    }
}

fn present_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<&'de str>, D::Error> {
    <&str>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
struct Observation {
    #[serde(deserialize_with = "deserialize_object")]
    clue: Clue,
    #[serde(rename = "budget", deserialize_with = "deserialize_object")]
    _budget: Budget,
    #[serde(deserialize_with = "deserialize_nullable_object")]
    coverage: Option<Coverage>,
}

// The generated phase enum is closed. Validate the budget shape while
// accepting future phase names and forwarding the original JSON.
#[derive(Deserialize)]
struct Budget {
    #[serde(rename = "phase")]
    _phase: String,
    #[serde(rename = "as_of")]
    _as_of: DateTime<Utc>,
    #[serde(rename = "account_lines")]
    _account_lines: u64,
    #[serde(rename = "reserve_lines")]
    _reserve_lines: u64,
    #[serde(rename = "stock_lines")]
    _stock_lines: u64,
    #[serde(rename = "hot_lines")]
    _hot_lines: u64,
    #[serde(rename = "rotation_lines")]
    _rotation_lines: u64,
    #[serde(rename = "confirmed_options")]
    _confirmed_options: u64,
    #[serde(rename = "observed_options")]
    _observed_options: u64,
    #[serde(rename = "discovery_complete")]
    _discovery_complete: bool,
}

#[derive(Deserialize)]
struct Clue {
    con_id: i32,
}

#[derive(Deserialize)]
struct Coverage {
    con_id: i32,
    #[serde(deserialize_with = "deserialize_object")]
    access: ProductPolicy,
}

impl Observation {
    fn check(&self, con_id: i32) -> Result<(), OptionReadError> {
        if self.clue.con_id != con_id {
            return Err(invalid());
        }
        if let Some(coverage) = &self.coverage {
            if coverage.con_id != con_id {
                return Err(invalid());
            }
            require_display(&coverage.access, "opra")?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct ObservationData<'a> {
    schema_version: &'a str,
    seq: &'a str,
    #[serde(deserialize_with = "deserialize_object")]
    access: ProductPolicy,
    #[serde(deserialize_with = "deserialize_object")]
    observation: Observation,
}

#[derive(Deserialize)]
struct StatusData<'a> {
    schema_version: &'a str,
    con_id: i32,
    state: String,
}

#[derive(Deserialize)]
struct ResetData<'a> {
    schema_version: &'a str,
    con_id: i32,
    #[serde(rename = "reason")]
    _reason: String,
    snapshot: &'a str,
}

fn parse_object<'a, T: Deserialize<'a>>(raw: &'a str) -> serde_json::Result<T> {
    // Serde's derived struct visitor also accepts positional JSON arrays.
    if !raw.trim_start().starts_with('{') {
        return Err(serde::de::Error::custom("expected F16 JSON object"));
    }
    serde_json::from_str(raw)
}

fn deserialize_object<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    let raw = <&RawValue>::deserialize(deserializer)?;
    parse_object(raw.get()).map_err(serde::de::Error::custom)
}

fn deserialize_nullable_object<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    Option::<&RawValue>::deserialize(deserializer)?
        .map(|raw| parse_object(raw.get()).map_err(serde::de::Error::custom))
        .transpose()
}

fn valid_id(value: &str) -> bool {
    (1..=MAX_CURSOR_BYTES).contains(&value.len())
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn invalid() -> OptionReadError {
    OptionReadError::Market(MarketReadError::InvalidResponse)
}

/// Validate the permission boundary without rewriting application data.
/// Returns true for the terminal recovery frame.
pub(super) fn check(
    event: &str,
    id: Option<&str>,
    data: &str,
    con_id: i32,
) -> Result<bool, OptionReadError> {
    if id.is_some_and(|id| !valid_id(id)) {
        return Err(invalid());
    }
    match event {
        "observation" => {
            let data: ObservationData = parse_object(data).map_err(|_| invalid())?;
            if data.schema_version != "1.0" || id.is_none() || parse_u64(data.seq).is_none() {
                return Err(invalid());
            }
            require_display(&data.access, "opra")?;
            data.observation.check(con_id)?;
        }
        "status" => {
            let data: StatusData = parse_object(data).map_err(|_| invalid())?;
            if data.schema_version != "1.0"
                || data.con_id != con_id
                || (data.state == "live" && id.is_none())
                || (data.state == "disabled" && id.is_some())
            {
                return Err(invalid());
            }
        }
        "reset_required" => {
            let data: ResetData = parse_object(data).map_err(|_| invalid())?;
            if data.schema_version != "1.0"
                || data.con_id != con_id
                || id.is_some()
                || data.snapshot != SNAPSHOT
            {
                return Err(invalid());
            }
            return Ok(true);
        }
        _ => return Err(invalid()),
    }
    Ok(false)
}

#[derive(Deserialize)]
struct Snapshot<'a> {
    cursor: &'a str,
    #[serde(borrow, deserialize_with = "deserialize_object")]
    snapshot: Page<'a>,
}

#[derive(Deserialize)]
struct Page<'a> {
    #[serde(deserialize_with = "deserialize_object")]
    access: ProductPolicy,
    #[serde(rename = "as_of")]
    _as_of: DateTime<Utc>,
    con_id: i32,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    #[serde(borrow)]
    items: Vec<&'a RawValue>,
    #[serde(deserialize_with = "Option::deserialize")]
    next_offset: Option<u64>,
}

pub(super) fn check_snapshot(raw: &str, query: &SnapshotQuery) -> Result<(), OptionReadError> {
    let Snapshot { cursor, snapshot } = parse_object(raw).map_err(|_| invalid())?;
    if !valid_id(cursor)
        || snapshot.con_id != query.con_id
        || snapshot.start != query.start
        || snapshot.end != query.end
        || snapshot.next_offset.is_some()
    {
        return Err(invalid());
    }
    require_display(&snapshot.access, "opra")?;
    for raw_item in snapshot.items {
        let item: Observation = parse_object(raw_item.get()).map_err(|_| invalid())?;
        item.check(query.con_id)?;
    }
    Ok(())
}
