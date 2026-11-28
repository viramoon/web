use super::{MAX_CURSOR_BYTES, SNAPSHOT, SnapshotQuery};
use crate::{
    market_options::{OptionReadError, require_display},
    market_reads::MarketReadError,
    market_types::parse_u64,
};
use chrono::{DateTime, Utc};
use finance_client::models::ProductPolicy;
use serde::Deserialize;
use serde_json::value::RawValue;

#[derive(Deserialize)]
pub(super) struct Envelope<'a> {
    pub(super) event: &'a str,
    pub(super) id: Option<&'a str>,
    pub(super) data: &'a RawValue,
}

#[derive(Deserialize)]
struct Observation {
    clue: Clue,
    coverage: Option<Coverage>,
}

#[derive(Deserialize)]
struct Clue {
    con_id: i32,
}

#[derive(Deserialize)]
struct Coverage {
    con_id: i32,
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
    access: ProductPolicy,
    observation: Observation,
}

#[derive(Deserialize)]
struct StatusData<'a> {
    schema_version: &'a str,
    con_id: i32,
    state: &'a str,
}

#[derive(Deserialize)]
struct ResetData<'a> {
    schema_version: &'a str,
    con_id: i32,
    snapshot: &'a str,
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
            let data: ObservationData = serde_json::from_str(data).map_err(|_| invalid())?;
            if data.schema_version != "1.0" || id.is_none() || parse_u64(data.seq).is_none() {
                return Err(invalid());
            }
            require_display(&data.access, "opra")?;
            data.observation.check(con_id)?;
        }
        "status" => {
            let data: StatusData = serde_json::from_str(data).map_err(|_| invalid())?;
            if data.schema_version != "1.0"
                || data.con_id != con_id
                || (data.state == "live" && id.is_none())
                || (data.state == "disabled" && id.is_some())
            {
                return Err(invalid());
            }
        }
        "reset_required" => {
            let data: ResetData = serde_json::from_str(data).map_err(|_| invalid())?;
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
    snapshot: Page,
}

#[derive(Deserialize)]
struct Page {
    access: ProductPolicy,
    con_id: i32,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    items: Vec<Observation>,
}

pub(super) fn check_snapshot(raw: &str, query: &SnapshotQuery) -> Result<(), OptionReadError> {
    let Snapshot { cursor, snapshot } = serde_json::from_str(raw).map_err(|_| invalid())?;
    if !valid_id(cursor)
        || snapshot.con_id != query.con_id
        || snapshot.start != query.start
        || snapshot.end != query.end
    {
        return Err(invalid());
    }
    require_display(&snapshot.access, "opra")?;
    for item in snapshot.items {
        item.check(query.con_id)?;
    }
    Ok(())
}
