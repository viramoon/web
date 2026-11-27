//! Authenticated operational events with bounded, process-local replay.
mod sources;

use crate::{
    AppState, ErrorBody, StreamShutdown, api_error,
    auth::{Product, WriteAccess, stream_auth_headers},
    ops::BackupUpdate,
};
use axum::{
    Extension, Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::get,
};
use futures_util::stream;
use serde::Serialize;
use sources::{AlertUpdate, JobUpdate, Sources};
use std::{
    collections::VecDeque,
    convert::Infallible,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    time::{Instant, Interval, MissedTickBehavior},
};
use utoipa::{OpenApi, ToSchema};
use uuid::Uuid;

pub(crate) const EVENTS: &str = "/api/v1/events";
const LAST_EVENT_ID: &str = "last-event-id";
const RETAINED_EVENTS: usize = 128;
const MAX_CONNECTIONS: usize = 16;
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const AUTH_INTERVAL: Duration = Duration::from_secs(1);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Serialize, ToSchema)]
enum EventSchemaVersion {
    #[serde(rename = "1.0")]
    V1,
}

#[derive(Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum PermissionsState {
    Authorized,
    Revoked,
}

#[derive(Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum EventResetReason {
    SnapshotRequired,
    CursorUnavailable,
    AlertWindowOverrun,
}

#[derive(Clone, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EventPayload {
    Job {
        job: JobUpdate,
    },
    Alert {
        alert: AlertUpdate,
    },
    Backup {
        backup: BackupUpdate,
    },
    Permissions {
        state: PermissionsState,
        products: Vec<Product>,
    },
    Reset {
        reason: EventResetReason,
    },
}

impl EventPayload {
    fn event_name(&self) -> &'static str {
        match self {
            Self::Job { .. } => "job",
            Self::Alert { .. } => "alert",
            Self::Backup { .. } => "backup",
            Self::Permissions { .. } => "permissions",
            Self::Reset { .. } => "reset",
        }
    }
}

#[derive(Serialize, ToSchema)]
struct EventData {
    schema_version: EventSchemaVersion,
    #[serde(flatten)]
    payload: EventPayload,
}

fn frame(payload: EventPayload, id: Option<Cursor>) -> Event {
    let mut event = Event::default()
        .event(payload.event_name())
        .json_data(EventData {
            schema_version: EventSchemaVersion::V1,
            payload,
        })
        .expect("fixed event data is serializable");
    if let Some(id) = id {
        event = event.id(id.to_string());
    }
    event
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Cursor {
    epoch: Uuid,
    sequence: u64,
}

impl fmt::Display for Cursor {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}:{}", self.epoch, self.sequence)
    }
}

enum Start {
    Fresh,
    Resume(Cursor),
    Invalid,
}

impl Start {
    fn read(headers: &HeaderMap) -> Self {
        let mut values = headers.get_all(LAST_EVENT_ID).iter();
        let Some(value) = values.next() else {
            return Self::Fresh;
        };
        if values.next().is_some() {
            return Self::Invalid;
        }
        if value.is_empty() {
            return Self::Fresh;
        }
        match value.to_str().ok().and_then(parse_cursor) {
            Some(cursor) => Self::Resume(cursor),
            None => Self::Invalid,
        }
    }
}

fn parse_cursor(value: &str) -> Option<Cursor> {
    if !(38..=57).contains(&value.len()) {
        return None;
    }
    let (epoch, sequence) = value.split_once(':')?;
    let cursor = Cursor {
        epoch: epoch.parse().ok()?,
        sequence: sequence.parse().ok()?,
    };
    (value == cursor.to_string()).then_some(cursor)
}

struct Journal {
    head: Cursor,
    entries: VecDeque<EventPayload>,
    poller_running: bool,
    next_poll: Instant,
}

enum Replay {
    Idle,
    Send(Cursor, EventPayload),
}

impl Journal {
    fn push(&mut self, event: EventPayload) {
        if self.head.sequence == u64::MAX {
            self.head.epoch = Uuid::new_v4();
            self.head.sequence = 0;
            self.entries.clear();
        }
        self.head.sequence += 1;
        if self.entries.len() == RETAINED_EVENTS {
            self.entries.pop_front();
        }
        self.entries.push_back(event);
    }

    fn after(&self, last: Cursor) -> Replay {
        let oldest_predecessor = self.head.sequence - self.entries.len() as u64;
        if self.head.epoch != last.epoch
            || !(oldest_predecessor..=self.head.sequence).contains(&last.sequence)
        {
            return Replay::Send(
                self.head,
                EventPayload::Reset {
                    reason: EventResetReason::CursorUnavailable,
                },
            );
        }
        let offset = (last.sequence - oldest_predecessor) as usize;
        match self.entries.get(offset) {
            Some(entry) => Replay::Send(
                Cursor {
                    sequence: last.sequence + 1,
                    ..last
                },
                entry.clone(),
            ),
            None => Replay::Idle,
        }
    }
}

struct Hub {
    changed: watch::Sender<()>,
    connections: Arc<Semaphore>,
    journal: Mutex<Journal>,
    sources: tokio::sync::Mutex<Sources>,
}

impl Hub {
    fn new() -> Self {
        Self {
            changed: watch::channel(()).0,
            connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            journal: Mutex::new(Journal {
                head: Cursor {
                    epoch: Uuid::new_v4(),
                    sequence: 0,
                },
                entries: VecDeque::with_capacity(RETAINED_EVENTS),
                poller_running: false,
                next_poll: Instant::now(),
            }),
            sources: tokio::sync::Mutex::new(Sources::default()),
        }
    }

    fn journal(&self) -> std::sync::MutexGuard<'_, Journal> {
        self.journal.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self, payloads: Vec<EventPayload>) {
        if payloads.is_empty() {
            return;
        }
        let mut journal = self.journal();
        for payload in payloads {
            journal.push(payload);
        }
        drop(journal);
        self.changed.send_replace(());
    }

    fn start_poller(self: &Arc<Self>, state: AppState, shutdown: Option<StreamShutdown>) {
        let mut journal = self.journal();
        if !journal.poller_running {
            journal.poller_running = true;
            let hub = Arc::clone(self);
            tokio::spawn(hub.run_poller(state, shutdown));
        }
    }

    async fn run_poller(self: Arc<Self>, state: AppState, shutdown: Option<StreamShutdown>) {
        let mut sources = self.sources.lock().await;
        self.poll_readers(&mut sources.readers(&state), shutdown)
            .await;
    }

    async fn poll_readers(
        &self,
        readers: &mut [sources::Updates<'_>],
        mut shutdown: Option<StreamShutdown>,
    ) {
        loop {
            let next_poll = self.journal().next_poll;
            tokio::select! {
                biased;
                () = wait_for_shutdown(&mut shutdown) => {
                    self.journal().poller_running = false;
                    return;
                }
                () = self.changed.closed() => {
                    let mut journal = self.journal();
                    if self.changed.receiver_count() == 0 {
                        journal.poller_running = false;
                        return;
                    }
                }
                () = tokio::time::sleep_until(next_poll) => {
                    let deadline = Instant::now() + POLL_INTERVAL;
                    let payloads = tokio::select! {
                        biased;
                        () = wait_for_shutdown(&mut shutdown) => {
                            self.journal().poller_running = false;
                            return;
                        }
                        payloads = sources::ready_batch(readers, deadline) => payloads,
                    };
                    self.publish(payloads);
                    self.journal().next_poll = deadline;
                }
            }
        }
    }
}

async fn wait_for_shutdown(shutdown: &mut Option<StreamShutdown>) {
    match shutdown {
        Some(StreamShutdown(receiver)) => {
            let _ = receiver.wait_for(|stopping| *stopping).await;
        }
        None => std::future::pending().await,
    }
}

fn timers() -> (Interval, Instant) {
    let now = Instant::now();
    let mut interval = tokio::time::interval_at(now + AUTH_INTERVAL, AUTH_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    (interval, now + HEARTBEAT_INTERVAL)
}

struct Connection {
    hub: Arc<Hub>,
    _permit: OwnedSemaphorePermit,
    changed: watch::Receiver<()>,
    start: Option<Start>,
    permissions_sent: bool,
    last: Cursor,
    access: WriteAccess,
    auth_headers: HeaderMap,
    auth_tick: Interval,
    heartbeat_due: Instant,
    shutdown: Option<StreamShutdown>,
    closed: bool,
}

impl Connection {
    async fn next(&mut self) -> Option<Event> {
        if self.closed {
            return None;
        }
        loop {
            if self.shutdown.as_ref().is_some_and(|stop| *stop.0.borrow()) {
                return None;
            }
            if self
                .access
                .authorize_stream(&self.auth_headers, false)
                .is_err()
            {
                self.closed = true;
                return Some(frame(
                    EventPayload::Permissions {
                        state: PermissionsState::Revoked,
                        products: Vec::new(),
                    },
                    None,
                ));
            }
            if !self.permissions_sent {
                self.permissions_sent = true;
                return Some(
                    frame(
                        EventPayload::Permissions {
                            state: PermissionsState::Authorized,
                            products: Product::ALL.into(),
                        },
                        None,
                    )
                    .retry(POLL_INTERVAL),
                );
            }
            let next = match self.start.take() {
                Some(Start::Fresh) => Replay::Send(
                    self.last,
                    EventPayload::Reset {
                        reason: EventResetReason::SnapshotRequired,
                    },
                ),
                Some(Start::Invalid) => Replay::Send(
                    self.last,
                    EventPayload::Reset {
                        reason: EventResetReason::CursorUnavailable,
                    },
                ),
                Some(Start::Resume(_)) | None => self.hub.journal().after(self.last),
            };
            if let Replay::Send(id, payload) = next {
                self.last = id;
                return Some(frame(payload, Some(id)));
            }
            let now = Instant::now();
            if now >= self.heartbeat_due {
                self.heartbeat_due = now + HEARTBEAT_INTERVAL;
                return Some(Event::default().comment("keep-alive"));
            }
            tokio::select! {
                biased;
                () = wait_for_shutdown(&mut self.shutdown) => return None,
                _ = self.changed.changed() => {},
                _ = self.auth_tick.tick() => {},
            };
        }
    }
}

pub(crate) fn routes() -> Router<AppState> {
    routes_with_hub(Arc::new(Hub::new()))
}

fn routes_with_hub(hub: Arc<Hub>) -> Router<AppState> {
    Router::new()
        .route(EVENTS, get(events).head(head))
        .layer(Extension(hub))
}

#[derive(OpenApi)]
#[openapi(
    paths(events, head),
    components(schemas(
        EventData,
        EventSchemaVersion,
        EventPayload,
        PermissionsState,
        EventResetReason,
        JobUpdate,
        AlertUpdate,
        BackupUpdate,
        crate::ops::BackupState
    ))
)]
pub(crate) struct EventsApi;

#[utoipa::path(get, path = "/api/v1/events", operation_id = "readEvents",
    description = "Without a configured HTTPS origin, the session cookie requires a matching HTTP loopback Origin, or a loopback Host and Sec-Fetch-Site: same-origin when Origin is omitted.",
    security(("adminBearer" = []), ("adminSession" = [])),
    params(("Last-Event-ID" = Option<String>, Header,
        description = "Opaque text ID from the last successfully handled event")),
    responses(
        (status = 200, body = String, content_type = "text/event-stream",
            description = "Operational SSE. Each data line is JSON EventData. On reset, reload authorized REST snapshots."),
        (status = 401, body = ErrorBody), (status = 403, body = ErrorBody),
        (status = 429, body = ErrorBody), (status = 503, body = ErrorBody)))]
async fn events(
    State(state): State<AppState>,
    Extension(hub): Extension<Arc<Hub>>,
    shutdown: Option<Extension<StreamShutdown>>,
    headers: HeaderMap,
) -> Response {
    if let Err(denied) = state.write_access.authorize_stream(&headers, false) {
        return denied.into_response();
    }
    let shutdown = shutdown.map(|Extension(signal)| signal);
    if shutdown.as_ref().is_some_and(|stop| *stop.0.borrow()) {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "stream_stopping",
            "Event stream is shutting down",
        );
    }
    let Ok(permit) = Arc::clone(&hub.connections).try_acquire_owned() else {
        let mut response = api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "stream_limit",
            "Too many event connections",
        );
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("3"));
        return response;
    };
    let changed = hub.changed.subscribe();
    let start = Start::read(&headers);
    let last = match start {
        Start::Resume(cursor) => cursor,
        _ => hub.journal().head,
    };
    let (auth_tick, heartbeat_due) = timers();
    let connection = Connection {
        hub: Arc::clone(&hub),
        changed,
        _permit: permit,
        start: Some(start),
        permissions_sent: false,
        last,
        access: state.write_access.clone(),
        auth_headers: stream_auth_headers(&headers),
        auth_tick,
        heartbeat_due,
        shutdown: shutdown.clone(),
        closed: false,
    };
    hub.start_poller(state, shutdown);
    (
        [
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (
                header::HeaderName::from_static("x-accel-buffering"),
                HeaderValue::from_static("no"),
            ),
        ],
        Sse::new(stream::unfold(connection, |mut connection| async {
            connection
                .next()
                .await
                .map(|frame| (Ok::<_, Infallible>(frame), connection))
        })),
    )
        .into_response()
}

#[utoipa::path(head, path = "/api/v1/events", operation_id = "headEvents",
    security(("adminBearer" = []), ("adminSession" = [])),
    responses((status = 200, description = "Authenticated event stream metadata"),
        (status = 401, body = ErrorBody), (status = 403, body = ErrorBody)))]
async fn head(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(denied) = state.write_access.authorize_stream(&headers, false) {
        return denied.into_response();
    }
    (
        [
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            ),
        ],
        "",
    )
        .into_response()
}

#[cfg(test)]
mod tests;
