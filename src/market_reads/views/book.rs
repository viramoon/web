//! Reconstructs only a single F1 book partition rooted at a committed snapshot.

use super::{MarketReadError, MarketViewMetadata, WindowQuery, parse_u64, parsed, read_window};
use crate::{AppState, ErrorBody, market_preferences::MarketPreferences};
use axum::{
    Extension, Json,
    extract::{Query, State, rejection::QueryRejection},
};
use finance_client::models::{
    BookLevel, BookSide, ConnectionState, MarketMessage, Message, WalRecord,
};
use rust_decimal::Decimal;
use serde::Serialize;
use std::{collections::BTreeMap, sync::Arc};
use utoipa::{
    ToSchema,
    openapi::{
        Ref, RefOr,
        schema::{Array, Schema},
    },
};

const MAX_BOOK_LEVELS: usize = 1024;

#[derive(Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum BookState {
    Observed,
    SnapshotRequired,
    Unknown,
}

#[derive(Serialize, ToSchema)]
pub(super) struct Observation {
    channel: String,
    venue: String,
    position: String,
    epoch: String,
    seq: String,
    exchange_ts: Option<String>,
    receive_ts: String,
}

impl From<&WalRecord> for Observation {
    fn from(record: &WalRecord) -> Self {
        Self {
            channel: record.event.channel.clone(),
            venue: record.event.venue.clone(),
            position: record.position.clone(),
            epoch: record.event.epoch.clone(),
            seq: record.event.seq.clone(),
            exchange_ts: record.event.exchange_ts.clone(),
            receive_ts: record.event.receive_ts.clone(),
        }
    }
}

#[derive(Default)]
struct Levels {
    bids: BTreeMap<Decimal, BookLevel>,
    asks: BTreeMap<Decimal, BookLevel>,
}

fn decimal(value: &str) -> Result<Decimal, MarketReadError> {
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let canonical = !whole.is_empty()
        && (whole == "0" || !whole.starts_with('0'))
        && whole.len() + fraction.len() <= 28
        && whole
            .bytes()
            .chain(fraction.bytes())
            .all(|ch| ch.is_ascii_digit())
        && (!unsigned.contains('.') || !fraction.is_empty());
    if !canonical {
        return Err(MarketReadError::InvalidResponse);
    }
    Decimal::from_str_exact(value).map_err(|_| MarketReadError::InvalidResponse)
}

impl Levels {
    fn update(
        &mut self,
        side: BookSide,
        price: &str,
        quantity: &str,
        snapshot: bool,
    ) -> Result<(), MarketReadError> {
        let number = decimal(price)?;
        let amount = decimal(quantity)?;
        let levels = match side {
            BookSide::Bid => &mut self.bids,
            BookSide::Ask => &mut self.asks,
        };
        if amount < Decimal::ZERO
            || (snapshot && (amount.is_zero() || levels.contains_key(&number)))
        {
            return Err(MarketReadError::InvalidResponse);
        }
        if amount.is_zero() {
            levels.remove(&number);
        } else {
            levels.insert(number, BookLevel::new(price.into(), quantity.into()));
        }
        if levels.len() > MAX_BOOK_LEVELS {
            return Err(MarketReadError::InvalidResponse);
        }
        Ok(())
    }
}

struct FoldedBook {
    levels: Option<Levels>,
    observation: Option<Observation>,
}

impl FoldedBook {
    fn state(&self) -> BookState {
        match (&self.levels, &self.observation) {
            (Some(_), _) => BookState::Observed,
            (None, Some(_)) => BookState::SnapshotRequired,
            (None, None) => BookState::Unknown,
        }
    }

    fn read(query: &WindowQuery, records: &[WalRecord]) -> Result<Self, MarketReadError> {
        let mut levels: Option<Levels> = None;
        let mut observation = None;
        let mut prior: Option<(u64, u64, &Message)> = None;
        for record in records
            .iter()
            .filter(|record| query.includes(&record.event, None))
        {
            let event = &record.event;
            if !event
                .instrument_id
                .rsplit_once('.')
                .is_some_and(|(_, venue)| venue == event.venue)
            {
                return Err(MarketReadError::InvalidResponse);
            }
            let epoch = parse_u64(&event.epoch)
                .filter(|epoch| *epoch > 0)
                .ok_or(MarketReadError::InvalidResponse)?;
            let seq = parse_u64(&event.seq).ok_or(MarketReadError::InvalidResponse)?;
            let message = event.message.as_ref();
            if let Some((old_epoch, old_seq, old_message)) = prior {
                if epoch == old_epoch && seq == old_seq && message == old_message {
                    continue;
                }
                if (epoch, seq) <= (old_epoch, old_seq) {
                    // A regressed position or conflicting repeat cannot establish a new baseline.
                    levels = None;
                    observation = Some(record.into());
                    continue;
                }
                if epoch != old_epoch || old_seq.checked_add(1) != Some(seq) {
                    levels = None;
                    observation = Some(record.into());
                }
            }
            prior = Some((epoch, seq, message));
            match message {
                Message::MarketMessageOneOf2(snapshot) => {
                    let mut next = Levels::default();
                    for (side, entries) in [
                        (BookSide::Bid, &snapshot.payload.bids),
                        (BookSide::Ask, &snapshot.payload.asks),
                    ] {
                        for entry in entries {
                            next.update(side, &entry.price, &entry.quantity, true)?;
                        }
                    }
                    levels = Some(next);
                    observation = Some(record.into());
                }
                Message::MarketMessageOneOf3(delta) => {
                    if let Some(current) = &mut levels {
                        for update in &delta.payload.updates {
                            current.update(update.side, &update.price, &update.quantity, false)?;
                        }
                    }
                    observation = Some(record.into());
                }
                Message::MessageOneOf1(_) | Message::MessageOneOf2(_) => {
                    levels = None;
                    observation = Some(record.into());
                }
                Message::MessageOneOf(correction)
                    if matches!(
                        correction.payload.replacement.as_ref(),
                        MarketMessage::MarketMessageOneOf2(_)
                            | MarketMessage::MarketMessageOneOf3(_),
                    ) =>
                {
                    levels = None;
                    observation = Some(record.into());
                }
                Message::MessageOneOf4(status)
                    if status.payload.state == ConnectionState::Disconnected =>
                {
                    levels = None;
                    observation = Some(record.into());
                }
                _ => {}
            }
        }
        Ok(Self {
            levels,
            observation,
        })
    }
}

#[derive(Serialize, ToSchema)]
pub(super) struct MarketBook {
    #[serde(flatten)]
    metadata: MarketViewMetadata,
    state: BookState,
    observation: Option<Observation>,
    #[schema(schema_with = book_level_array)]
    bids: Option<Vec<BookLevel>>,
    #[schema(schema_with = book_level_array)]
    asks: Option<Vec<BookLevel>>,
}

#[derive(Serialize, ToSchema)]
pub(super) struct MarketQuote {
    #[serde(flatten)]
    metadata: MarketViewMetadata,
    state: BookState,
    observation: Option<Observation>,
    #[schema(schema_with = book_level)]
    bid: Option<BookLevel>,
    #[schema(schema_with = book_level)]
    ask: Option<BookLevel>,
}

fn book_level() -> RefOr<Schema> {
    utoipa::openapi::schema::OneOfBuilder::new()
        .item(Ref::from_schema_name("BookLevel"))
        .item(utoipa::openapi::schema::Object::with_type(
            utoipa::openapi::schema::Type::Null,
        ))
        .into()
}

fn book_level_array() -> RefOr<Schema> {
    utoipa::openapi::schema::OneOfBuilder::new()
        .item(Array::new(Ref::from_schema_name("BookLevel")))
        .item(utoipa::openapi::schema::Object::with_type(
            utoipa::openapi::schema::Type::Null,
        ))
        .into()
}

async fn depth_observation(
    state: AppState,
    query: WindowQuery,
) -> Result<(MarketViewMetadata, FoldedBook), MarketReadError> {
    if query.channel.is_none()
        || query.from.is_some()
        || query.until.is_some()
        || query.indicator.is_some()
    {
        return Err(MarketReadError::InvalidQuery);
    }
    let (metadata, records) = read_window(&state, &query).await?;
    let book = FoldedBook::read(&query, &records)?;
    Ok((metadata, book))
}

#[utoipa::path(get, path = "/api/v1/market/book", operation_id = "marketBook", params(WindowQuery),
    security(("adminSession" = []), ("adminBearer" = [])),
    responses((status = 200, body = MarketBook), (status = 400, body = ErrorBody),
        (status = 401, body = ErrorBody), (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
        (status = 429, body = ErrorBody), (status = 502, body = ErrorBody),
        (status = 503, body = ErrorBody), (status = 504, body = ErrorBody)))]
pub(super) async fn book(
    State(state): State<AppState>,
    Extension(preferences): Extension<Arc<MarketPreferences>>,
    query: Result<Query<WindowQuery>, QueryRejection>,
) -> Result<Json<MarketBook>, MarketReadError> {
    let mut query = parsed(query)?;
    if query.instrument_id.is_none() {
        query.instrument_id = Some(
            preferences
                .focus()
                .ok_or(MarketReadError::DepthFocusRequired)?,
        );
    }
    let (metadata, book) = depth_observation(state, query).await?;
    let state = book.state();
    let (bids, asks) = match book.levels {
        Some(levels) => (
            Some(levels.bids.into_values().rev().collect()),
            Some(levels.asks.into_values().collect()),
        ),
        None => (None, None),
    };
    Ok(Json(MarketBook {
        metadata,
        state,
        observation: book.observation,
        bids,
        asks,
    }))
}

#[utoipa::path(get, path = "/api/v1/market/quotes", operation_id = "marketQuotes", params(WindowQuery),
    security(("adminSession" = []), ("adminBearer" = [])),
    responses((status = 200, body = MarketQuote), (status = 400, body = ErrorBody),
        (status = 401, body = ErrorBody), (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
        (status = 429, body = ErrorBody), (status = 502, body = ErrorBody),
        (status = 503, body = ErrorBody), (status = 504, body = ErrorBody)))]
pub(super) async fn quotes(
    State(state): State<AppState>,
    query: Result<Query<WindowQuery>, QueryRejection>,
) -> Result<Json<MarketQuote>, MarketReadError> {
    let (metadata, book) = depth_observation(state, parsed(query)?).await?;
    let state = book.state();
    let (bid, ask) = match book.levels {
        Some(levels) => (
            levels.bids.into_values().next_back(),
            levels.asks.into_values().next(),
        ),
        None => (None, None),
    };
    Ok(Json(MarketQuote {
        metadata,
        state,
        observation: book.observation,
        bid,
        ask,
    }))
}

#[cfg(test)]
mod tests;
