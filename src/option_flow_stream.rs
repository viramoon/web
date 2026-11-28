//! Authorized delivery and snapshot recovery for the finance F16 observation feed.

mod sse;
mod websocket;
mod wire;

use crate::{
    AppState, ErrorBody, StreamShutdown, api_error,
    auth::{WriteAccess, WriteDenied, consume_quota, stream_auth_headers},
    finance::Fault,
    market_options::{OptionReadError, parsed, positive_id, timestamp},
};
use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{Query, State, rejection::QueryRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, Utc};
use finance_client::apis::option_flow_stream_api as upstream;
use futures_util::stream;
use serde::Deserialize;
use serde_json::value::RawValue;
use std::{
    collections::VecDeque,
    convert::Infallible,
    future::pending,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{MissedTickBehavior, interval, timeout},
};
use utoipa::{IntoParams, OpenApi};

pub(crate) const WS: &str = "/api/v1/options/flow/ws";
pub(crate) const SSE: &str = "/api/v1/options/flow/sse";
pub(crate) const SNAPSHOT: &str = "/api/v1/options/flow/snapshot";
const MAX_CONNECTIONS: usize = 16;
const MAX_OPENS_PER_MINUTE: usize = 120;
const MAX_FRAME_BYTES: usize = 256 * 1024;
const MAX_CURSOR_BYTES: usize = 512;
const UPSTREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

pub(crate) fn routes() -> Router<AppState> {
    let limits = Arc::new(Limits {
        connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        opens: Mutex::new(VecDeque::new()),
    });
    Router::new()
        .route(WS, get(websocket::upgrade))
        .route(SSE, get(events))
        .route(SNAPSHOT, get(snapshot))
        .layer(Extension(limits))
}

#[derive(OpenApi)]
#[openapi(paths(websocket::upgrade, events))]
pub(crate) struct OptionFlowStreamApi;

struct Limits {
    connections: Arc<Semaphore>,
    opens: Mutex<VecDeque<Instant>>,
}

#[derive(Debug)]
enum LimitError {
    Connections,
    Rate(u64),
}

impl IntoResponse for LimitError {
    fn into_response(self) -> Response {
        match self {
            Self::Connections => api_error(
                StatusCode::TOO_MANY_REQUESTS,
                "option_flow_connection_limit",
                "Option flow connection limit reached",
            ),
            Self::Rate(seconds) => {
                let mut response = api_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate_limited",
                    "Option flow stream opening rate limit reached",
                );
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
                response
            }
        }
    }
}

impl Limits {
    fn acquire(&self) -> Result<OwnedSemaphorePermit, LimitError> {
        let permit = self
            .connections
            .clone()
            .try_acquire_owned()
            .map_err(|_| LimitError::Connections)?;
        consume_quota(&self.opens, MAX_OPENS_PER_MINUTE, Duration::from_secs(60))
            .map_err(LimitError::Rate)?;
        Ok(permit)
    }
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
struct StreamQuery {
    /// Confirmed broker option contract.
    #[param(minimum = 1)]
    con_id: i32,
    /// Opaque cursor from the same contract's stream or recovery snapshot.
    #[param(max_length = 512)]
    cursor: Option<String>,
}

impl StreamQuery {
    fn check(&self) -> Result<u32, OptionReadError> {
        if self
            .cursor
            .as_ref()
            .is_some_and(|id| id.len() > MAX_CURSOR_BYTES)
        {
            return Err(OptionReadError::InvalidQuery);
        }
        positive_id(self.con_id)
    }
}

fn last_event_id(headers: &HeaderMap) -> Result<Option<&str>, OptionReadError> {
    let mut ids = headers.get_all("last-event-id").iter();
    let id = ids.next();
    if ids.next().is_some() {
        return Err(OptionReadError::InvalidQuery);
    }
    id.map(|id| {
        id.to_str()
            .ok()
            .filter(|id| id.len() <= MAX_CURSOR_BYTES)
            .ok_or(OptionReadError::InvalidQuery)
    })
    .transpose()
}

struct Access {
    policy: WriteAccess,
    headers: HeaderMap,
    websocket: bool,
    shutdown: Option<StreamShutdown>,
    _permit: OwnedSemaphorePermit,
}

impl Access {
    fn new(
        state: &AppState,
        headers: &HeaderMap,
        websocket: bool,
        shutdown: Option<Extension<StreamShutdown>>,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        Self {
            policy: state.write_access.clone(),
            headers: stream_auth_headers(headers),
            websocket,
            shutdown: shutdown.map(|Extension(shutdown)| shutdown),
            _permit: permit,
        }
    }

    fn check(&self) -> Result<(), WriteDenied> {
        self.policy.authorize_stream(&self.headers, self.websocket)
    }

    async fn stopped(&mut self) {
        let mut tick = interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = async {
                    match &mut self.shutdown {
                        Some(shutdown) => { let _ = shutdown.0.wait_for(|stop| *stop).await; }
                        None => pending().await,
                    }
                } => return,
                _ = tick.tick() => if self.check().is_err() { return; },
            }
        }
    }
}

#[utoipa::path(get, path = "/api/v1/options/flow/sse", operation_id = "optionsFlowSse",
    description = "F16 option-flow 1.0. See finance.option-flow-sse.schema.json for status, observation and reset_required. Reconnect with fresh authorization and resume with the last applied ID.",
    params(StreamQuery, ("Last-Event-ID" = Option<String>, Header, description = "Nonempty opaque resume cursor. Takes precedence over the query cursor.")),
    security(("adminSession" = []), ("adminBearer" = [])),
    responses(
        (status = 200, description = "Option-flow SSE 1.0", content_type = "text/event-stream", body = String),
        (status = 400, body = ErrorBody), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 429, body = ErrorBody),
        (status = 502, body = ErrorBody), (status = 503, body = ErrorBody),
        (status = 504, body = ErrorBody)
    ))]
async fn events(
    State(state): State<AppState>,
    Extension(limits): Extension<Arc<Limits>>,
    shutdown: Option<Extension<StreamShutdown>>,
    headers: HeaderMap,
    query: Result<Query<StreamQuery>, QueryRejection>,
) -> Result<Response, Response> {
    let query = parsed(query).map_err(IntoResponse::into_response)?;
    let con_id = query.check().map_err(IntoResponse::into_response)?;
    let last_id = last_event_id(&headers).map_err(IntoResponse::into_response)?;
    let permit = limits.acquire().map_err(IntoResponse::into_response)?;
    let mut access = Access::new(&state, &headers, false, shutdown, permit);
    let response = tokio::select! {
        biased;
        () = access.stopped() => return Err(closed_during_open(&access)),
        response = state.finance.open_stream(async |cfg| {
            let response = upstream::sse(cfg, con_id, query.cursor.as_deref(), last_id)
                .send().await.map_err(Fault::stream_transport)?;
            if response.status() != StatusCode::OK {
                return Err(Fault::stream_status(response.status()));
            }
            let is_sse = response.headers().get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(';').next())
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"));
            if !is_sse {
                return Err(Fault::invalid_response());
            }
            Ok(response)
        }) => response.map_err(IntoResponse::into_response)?,
    };
    access.check().map_err(IntoResponse::into_response)?;
    let frames = stream::unfold(
        (sse::SseReader::new(response), access, false),
        move |(mut reader, mut access, done)| async move {
            if done {
                return None;
            }
            let frame = tokio::select! {
                biased;
                () = access.stopped() => return None,
                frame = timeout(UPSTREAM_IDLE_TIMEOUT, reader.next()) => frame.ok()?.ok()??,
            };
            access.check().ok()?;
            let reset = if let Some(dispatch) = frame.dispatch() {
                wire::check(dispatch.event, dispatch.id, dispatch.data, query.con_id).ok()?
            } else {
                false
            };
            Some((
                Ok::<_, Infallible>(frame.into_bytes()),
                (reader, access, reset),
            ))
        },
    );
    Ok((
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Body::from_stream(frames),
    )
        .into_response())
}

fn closed_during_open(access: &Access) -> Response {
    match access.check() {
        Err(denied) => denied.into_response(),
        Ok(()) => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "option_flow_unavailable",
            "Option flow stream is shutting down",
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotQuery {
    con_id: i32,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

async fn snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<SnapshotQuery>, QueryRejection>,
) -> Result<Json<Box<RawValue>>, Response> {
    let query = parsed(query).map_err(IntoResponse::into_response)?;
    let con_id = positive_id(query.con_id).map_err(IntoResponse::into_response)?;
    if query.start >= query.end {
        return Err(OptionReadError::InvalidQuery.into_response());
    }
    let raw: Box<RawValue> = state
        .finance
        .read(|cfg| async move {
            upstream::snapshot(&cfg, con_id, timestamp(query.start), timestamp(query.end)).await
        })
        .await
        .map_err(IntoResponse::into_response)?;
    state
        .write_access
        .authorize_stream(&headers, false)
        .map_err(IntoResponse::into_response)?;
    wire::check_snapshot(raw.get(), &query).map_err(IntoResponse::into_response)?;
    Ok(Json(raw))
}

#[cfg(test)]
mod tests;
