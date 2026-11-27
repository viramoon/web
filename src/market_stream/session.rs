//! Browser market stream commands and delivery over an authorized F10a WAL.

use super::{MarketPartition, MarketReplay, ReplayError, ResetReason, replay::cursor};
use crate::{auth::WriteAccess, finance::Finance, market_preferences::MarketPreferences};
use axum::http::{HeaderMap, HeaderName, header};
use finance_client::models::{Event, SchemaVersion};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::OwnedSemaphorePermit;
use utoipa::ToSchema;

/// Browser command text is bounded before parsing or taking a subscription.
pub const MAX_COMMAND_BYTES: usize = 4096;

#[derive(Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MarketStreamCommand {
    Subscribe {
        source: String,
        product: String,
        channel: String,
        instrument_id: Option<String>,
        /// Select the saved depth focus in place of an explicit instrument.
        #[serde(default)]
        use_depth_focus: bool,
        /// Exclusive decimal-string WAL offset. Omit to start at zero.
        after: Option<String>,
    },
    Unsubscribe {},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MarketStreamError {
    AuthorizationRevoked,
    InvalidSubscription,
    SubscriptionQuotaExceeded,
    ProductUnavailable,
    FinanceUnavailable,
}

/// Web controls are distinct from the producer's F1 event envelopes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MarketStreamControl {
    Checkpoint { after: String },
    Reset { reason: ResetReason },
    Error { code: MarketStreamError },
    Unsubscribed,
}

#[derive(Debug)]
pub enum MarketStreamFrame {
    Event(Arc<Event>),
    Control(MarketStreamControl),
}

impl MarketStreamFrame {
    pub fn to_text(&self) -> Result<String, serde_json::Error> {
        match self {
            Self::Event(event) => serde_json::to_string(event.as_ref()),
            Self::Control(control) => serde_json::to_string(control),
        }
    }
}

struct Subscription {
    replay: MarketReplay,
    _permit: OwnedSemaphorePermit,
    checkpoint_sent: bool,
}

/// A connection may replace its selection using the same principal's quota slot.
pub struct MarketStreamSession {
    access: WriteAccess,
    headers: HeaderMap,
    preferences: Arc<MarketPreferences>,
    subscription: Option<Subscription>,
}

impl MarketStreamSession {
    pub fn open(
        access: WriteAccess,
        headers: &HeaderMap,
        preferences: Arc<MarketPreferences>,
    ) -> Result<Self, MarketStreamError> {
        access
            .authorize_stream(headers, true)
            .map_err(|_| MarketStreamError::AuthorizationRevoked)?;
        let mut retained = HeaderMap::new();
        for name in [
            header::AUTHORIZATION,
            header::COOKIE,
            header::ORIGIN,
            HeaderName::from_static("sec-fetch-site"),
        ] {
            for value in headers.get_all(&name) {
                let mut value = value.clone();
                value.set_sensitive(true);
                retained.append(name.clone(), value);
            }
        }
        Ok(Self {
            access,
            headers: retained,
            preferences,
            subscription: None,
        })
    }

    pub fn authorize(&mut self) -> Result<(), MarketStreamError> {
        if self.access.authorize_stream(&self.headers, true).is_err() {
            self.subscription = None;
            return Err(MarketStreamError::AuthorizationRevoked);
        }
        Ok(())
    }

    pub fn is_subscribed(&self) -> bool {
        self.subscription.is_some()
    }

    pub fn command(
        &mut self,
        text: &str,
    ) -> Result<Option<MarketStreamControl>, MarketStreamError> {
        self.authorize()?;
        if text.len() > MAX_COMMAND_BYTES {
            return Err(MarketStreamError::InvalidSubscription);
        }
        let command: MarketStreamCommand =
            serde_json::from_str(text).map_err(|_| MarketStreamError::InvalidSubscription)?;
        let MarketStreamCommand::Subscribe {
            source,
            product,
            channel,
            instrument_id,
            use_depth_focus,
            after,
        } = command
        else {
            self.subscription = None;
            return Ok(Some(MarketStreamControl::Unsubscribed));
        };
        let instrument_id = match (instrument_id, use_depth_focus) {
            (Some(instrument_id), false) => Some(instrument_id),
            (None, true) => self
                .preferences
                .focus()
                .map(|focus| focus.as_str().to_owned()),
            _ => None,
        }
        .ok_or(MarketStreamError::InvalidSubscription)?;
        let partition = MarketPartition::new(&source, &product, &channel, &instrument_id)
            .ok_or(MarketStreamError::InvalidSubscription)?;
        let after = cursor(after.as_deref().unwrap_or("0"))
            .map_err(|_| MarketStreamError::InvalidSubscription)?;
        let replay = MarketReplay::new(partition, after);
        if let Some(subscription) = &mut self.subscription {
            subscription.replay = replay;
            subscription.checkpoint_sent = false;
        } else {
            let permit = self
                .preferences
                .acquire()
                .ok_or(MarketStreamError::SubscriptionQuotaExceeded)?;
            self.subscription = Some(Subscription {
                replay,
                _permit: permit,
                checkpoint_sent: false,
            });
        }
        // A checkpoint acknowledges only a selection finance has authorized.
        Ok(None)
    }

    pub async fn poll(&mut self, finance: &Finance) -> Vec<MarketStreamFrame> {
        let Some(subscription) = &mut self.subscription else {
            return Vec::new();
        };
        let previous = subscription.replay.position();
        let result = subscription
            .replay
            .poll(finance, &self.access, &self.headers)
            .await;
        self.finish_poll(previous, result)
    }

    #[cfg(test)]
    pub(super) async fn poll_with(
        &mut self,
        fetch: impl std::ops::AsyncFnOnce(
            &MarketPartition,
            u64,
            Option<u64>,
        )
            -> Result<finance_client::models::MarketWalPage, ReplayError>,
    ) -> Vec<MarketStreamFrame> {
        let Some(subscription) = &mut self.subscription else {
            return Vec::new();
        };
        let previous = subscription.replay.position();
        let result = subscription
            .replay
            .poll_with(&self.access, &self.headers, fetch)
            .await;
        self.finish_poll(previous, result)
    }

    fn finish_poll(
        &mut self,
        previous: u64,
        result: Result<Vec<Arc<Event>>, ReplayError>,
    ) -> Vec<MarketStreamFrame> {
        let controls = match result {
            Ok(events) => {
                if events
                    .iter()
                    .any(|event| event.schema_version != SchemaVersion::Variant10)
                {
                    MarketStreamControl::Reset {
                        reason: ResetReason::UnsupportedVersion,
                    }
                } else {
                    let subscription = self.subscription.as_mut().expect("active subscription");
                    let position = subscription.replay.position();
                    let mut frames: Vec<_> =
                        events.into_iter().map(MarketStreamFrame::Event).collect();
                    if !subscription.checkpoint_sent || position != previous {
                        subscription.checkpoint_sent = true;
                        frames.push(MarketStreamFrame::Control(
                            MarketStreamControl::Checkpoint {
                                after: position.to_string(),
                            },
                        ));
                    }
                    return frames;
                }
            }
            Err(ReplayError::ResetRequired(reason)) => MarketStreamControl::Reset { reason },
            Err(ReplayError::Unauthorized) => MarketStreamControl::Error {
                code: MarketStreamError::AuthorizationRevoked,
            },
            Err(ReplayError::ProductUnavailable) => MarketStreamControl::Error {
                code: MarketStreamError::ProductUnavailable,
            },
            Err(ReplayError::UpstreamUnavailable) => MarketStreamControl::Error {
                code: MarketStreamError::FinanceUnavailable,
            },
        };
        self.subscription = None;
        vec![MarketStreamFrame::Control(controls)]
    }
}
