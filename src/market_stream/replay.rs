use crate::{
    auth::WriteAccess,
    finance::Finance,
    market_reads::{event_version_matches, wal_page},
    market_types::{valid_instrument_id, valid_token},
};
use axum::http::{HeaderMap, StatusCode};
use finance_client::models::{Event, MarketWalPage, Message};
use serde::Serialize;
use std::{ops::AsyncFnOnce, sync::Arc, time::Duration};
use tokio::time::{Instant, sleep_until};

const PAGE_LIMIT: u64 = 64;
const MAX_BACKLOG: u64 = 512;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// One F1 partition in an explicitly selected source and product.
#[derive(Clone, PartialEq, Eq)]
pub struct MarketPartition {
    source: String,
    product: String,
    channel: String,
    instrument_id: String,
}

impl MarketPartition {
    pub fn new(source: &str, product: &str, channel: &str, instrument_id: &str) -> Option<Self> {
        if ![source, product, channel].into_iter().all(valid_token)
            || !valid_instrument_id(instrument_id)
        {
            return None;
        }
        Some(Self {
            source: source.into(),
            product: product.into(),
            channel: channel.into(),
            instrument_id: instrument_id.into(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayError {
    Unauthorized,
    ProductUnavailable,
    UpstreamUnavailable,
    ResetRequired(ResetReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetReason {
    ConsumerLagged,
    CursorUnavailable,
    Discontinuity,
    SnapshotRequired,
    UnsupportedVersion,
}

/// Bounded committed WAL replay for a single, already-selected F1 partition.
/// WAL positions are exclusive product-local offsets and independent of F1 epoch/seq.
/// A reset invalidates the reader until the caller establishes a new starting position.
pub struct MarketReplay {
    partition: MarketPartition,
    after: u64,
    through: Option<u64>,
    delivery: Delivery,
    blocked: Option<ResetReason>,
    next_poll: Instant,
}

impl MarketReplay {
    pub fn new(partition: MarketPartition, after: u64) -> Self {
        Self {
            partition,
            after,
            through: None,
            delivery: Delivery::default(),
            blocked: None,
            next_poll: Instant::now(),
        }
    }

    /// Read a page of committed events, capped by the WAL backlog and poll interval.
    /// An empty page can indicate that every record this time belonged to other partitions.
    pub async fn poll(
        &mut self,
        finance: &Finance,
        access: &WriteAccess,
        headers: &HeaderMap,
    ) -> Result<Vec<Arc<Event>>, ReplayError> {
        self.poll_with(access, headers, async |partition, after, through| {
            let after = after.to_string();
            let through = through.map(|through| through.to_string());
            wal_page(
                finance,
                &partition.source,
                &partition.product,
                Some(&after),
                through.as_deref(),
                Some(PAGE_LIMIT),
            )
            .await
            .map_err(|fault| match fault.status() {
                StatusCode::BAD_REQUEST => {
                    ReplayError::ResetRequired(ResetReason::CursorUnavailable)
                }
                StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => ReplayError::ProductUnavailable,
                _ => ReplayError::UpstreamUnavailable,
            })
        })
        .await
    }

    pub(super) async fn poll_with(
        &mut self,
        access: &WriteAccess,
        headers: &HeaderMap,
        fetch: impl AsyncFnOnce(
            &MarketPartition,
            u64,
            Option<u64>,
        ) -> Result<MarketWalPage, ReplayError>,
    ) -> Result<Vec<Arc<Event>>, ReplayError> {
        authorize(access, headers)?;
        if let Some(reason) = self.blocked {
            return Err(ReplayError::ResetRequired(reason));
        }
        sleep_until(self.next_poll).await;
        authorize(access, headers)?;
        self.next_poll = Instant::now() + POLL_INTERVAL;
        let result = fetch(&self.partition, self.after, self.through).await;
        authorize(access, headers)?;
        let result = result.and_then(|page| self.accept(page));
        if let Err(ReplayError::ResetRequired(reason)) = result {
            self.blocked = Some(reason);
            self.delivery = Delivery::default();
        }
        result
    }

    fn accept(&mut self, page: MarketWalPage) -> Result<Vec<Arc<Event>>, ReplayError> {
        // wal_page has checked recording/display permission and the license of each record.
        if self
            .delivery
            .previous
            .as_ref()
            .is_some_and(|previous| previous.event.license != page.access.license)
        {
            return Err(ReplayError::ResetRequired(ResetReason::CursorUnavailable));
        }
        let durable = cursor(&page.durable_through)?;
        let through = cursor(&page.through)?;
        let backlog = durable
            .checked_sub(self.after)
            .ok_or(ReplayError::ResetRequired(ResetReason::CursorUnavailable))?;
        if backlog > MAX_BACKLOG {
            return Err(ReplayError::ResetRequired(ResetReason::ConsumerLagged));
        }
        if through > durable
            || through != self.through.unwrap_or(durable)
            || page.records.len() > PAGE_LIMIT as usize
        {
            return Err(ReplayError::UpstreamUnavailable);
        }

        // Advance the cursor and baseline only after validating the whole page.
        let mut after = self.after;
        let mut delivery = self.delivery.clone();
        let mut events = Vec::new();
        for record in page.records {
            let position = cursor(&record.position)?;
            let event = record.event;
            if after.checked_add(1) != Some(position) || position > through {
                return Err(ReplayError::UpstreamUnavailable);
            }
            after = position;
            if event.channel == self.partition.channel
                && event.instrument_id == self.partition.instrument_id
                && let Some(frame) = delivery.observe(*event)?
            {
                events.push(frame);
            }
        }

        let next = page.next_after.as_deref().map(cursor).transpose()?;
        if next != (after < through).then_some(after) || (next.is_some() && after == self.after) {
            return Err(ReplayError::UpstreamUnavailable);
        }
        self.after = after;
        self.through = next.map(|_| through);
        self.delivery = delivery;
        Ok(events)
    }
}

fn authorize(access: &WriteAccess, headers: &HeaderMap) -> Result<(), ReplayError> {
    access
        .authorize_stream(headers, true)
        .map_err(|_| ReplayError::Unauthorized)
}

fn cursor(value: &str) -> Result<u64, ReplayError> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ReplayError::UpstreamUnavailable);
    }
    value.parse().map_err(|_| ReplayError::UpstreamUnavailable)
}

#[derive(Clone, Default)]
struct Delivery {
    previous: Option<PositionedEvent>,
    has_book_snapshot: bool,
}

#[derive(Clone)]
struct PositionedEvent {
    epoch: u64,
    seq: u64,
    event: Arc<Event>,
}

impl Delivery {
    fn observe(&mut self, mut event: Event) -> Result<Option<Arc<Event>>, ReplayError> {
        if !event_version_matches(&event) {
            return Err(ReplayError::ResetRequired(ResetReason::UnsupportedVersion));
        }
        let epoch = cursor(&event.epoch)?;
        let seq = cursor(&event.seq)?;
        let reset = matches!(*event.message, Message::MessageOneOf2(_));
        let discontinuity = ReplayError::ResetRequired(ResetReason::Discontinuity);
        if epoch == 0 || (reset && seq != 0) {
            return Err(discontinuity);
        }
        if let Some(previous) = &self.previous {
            if epoch == previous.epoch && seq == previous.seq {
                // F3 can re-deliver one F1 event with an updated receive clock.
                event.receive_ts.clone_from(&previous.event.receive_ts);
                return if event == *previous.event {
                    Ok(None)
                } else {
                    Err(discontinuity)
                };
            }
            let contiguous = if reset {
                epoch > previous.epoch
            } else {
                epoch == previous.epoch && previous.seq.checked_add(1) == Some(seq)
            };
            if !contiguous {
                return Err(discontinuity);
            }
        }
        match *event.message {
            Message::MarketMessageOneOf2(_) => self.has_book_snapshot = true,
            Message::MarketMessageOneOf3(_) if !self.has_book_snapshot => {
                return Err(ReplayError::ResetRequired(ResetReason::SnapshotRequired));
            }
            Message::MessageOneOf1(_) | Message::MessageOneOf2(_) => self.has_book_snapshot = false,
            _ => {}
        }
        let event = Arc::new(event);
        self.previous = Some(PositionedEvent {
            epoch,
            seq,
            event: Arc::clone(&event),
        });
        Ok(Some(event))
    }
}

#[cfg(test)]
mod tests;
