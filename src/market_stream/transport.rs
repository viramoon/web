use super::{
    MAX_COMMAND_BYTES, MarketStreamControl, MarketStreamError, MarketStreamFrame,
    MarketStreamSession, ResetReason,
};
use crate::{
    AppState, ErrorBody, StreamShutdown, api_error, market_preferences::MarketPreferences,
};
use axum::{
    Extension,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use std::{future::pending, sync::Arc, time::Duration};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{MissedTickBehavior, interval, timeout},
};

const MAX_CONNECTIONS: usize = 16;
const MAX_WRITE_BYTES: usize = 256 * 1024;
const MAX_EVENT_BYTES: usize = MAX_WRITE_BYTES - 256;
const SEND_TIMEOUT: Duration = Duration::from_secs(1);
const AUTH_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub(super) struct Connections(Arc<Semaphore>);

impl Connections {
    pub(super) fn new() -> Self {
        Self(Arc::new(Semaphore::new(MAX_CONNECTIONS)))
    }
}

#[utoipa::path(get, path = "/api/v1/stream/market", operation_id = "marketStream",
    description = "RFC 6455 upgrade for authorized F10a WAL replay. A session cookie requires the configured HTTPS origin or, when unset, an HTTP loopback Origin matching Host. See MarketStreamCommand and MarketStreamControl.",
    security(("adminSession" = []), ("adminBearer" = [])),
    responses((status = 101, description = "Market WebSocket opened"),
        (status = 400, body = ErrorBody), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 429, body = ErrorBody),
        (status = 503, body = ErrorBody, description = "Finance is unreachable")))]
pub(super) async fn upgrade(
    State(state): State<AppState>,
    Extension(preferences): Extension<Arc<MarketPreferences>>,
    Extension(connections): Extension<Connections>,
    shutdown: Option<Extension<StreamShutdown>>,
    headers: HeaderMap,
    websocket: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    if let Err(denied) = state.write_access.authorize_stream(&headers, true) {
        return denied.into_response();
    }
    let Ok(websocket) = websocket else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_market_websocket_upgrade",
            "A valid WebSocket upgrade is required",
        );
    };
    let Some(permit) = connections.0.try_acquire_owned().ok() else {
        return api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "market_connection_limit",
            "Market connection limit reached",
        );
    };
    let (_, reachable) = state.finance.probe().await;
    let Ok(session) = MarketStreamSession::open(state.write_access.clone(), &headers, preferences)
    else {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "A valid administrator session is required",
        );
    };
    if !reachable {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "finance_unavailable",
            "Finance is unavailable",
        );
    }
    websocket
        .max_frame_size(MAX_COMMAND_BYTES)
        .max_message_size(MAX_COMMAND_BYTES)
        .read_buffer_size(MAX_COMMAND_BYTES)
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_WRITE_BYTES)
        .on_upgrade(move |peer| serve(peer, session, state, shutdown.map(|s| s.0), permit))
}

async fn stopping(shutdown: &mut Option<StreamShutdown>) {
    if let Some(shutdown) = shutdown {
        let _ = shutdown.0.wait_for(|value| *value).await;
    } else {
        pending::<()>().await;
    }
}

#[derive(PartialEq, Eq)]
enum SendOutcome {
    Sent,
    Interrupted,
    Lagged,
}

async fn send(
    peer: &mut WebSocket,
    frame: MarketStreamFrame,
    shutdown: &mut Option<StreamShutdown>,
) -> SendOutcome {
    let Ok(text) = frame.to_text() else {
        return SendOutcome::Interrupted;
    };
    // Leave room for frame headers and control traffic within the buffer.
    if text.len() > MAX_EVENT_BYTES {
        return SendOutcome::Lagged;
    }
    tokio::select! {
        biased;
        () = stopping(shutdown) => SendOutcome::Interrupted,
        result = timeout(SEND_TIMEOUT, peer.send(Message::Text(text.into()))) => match result {
            Ok(Ok(())) => SendOutcome::Sent,
            Ok(Err(_)) => SendOutcome::Interrupted,
            Err(_) => SendOutcome::Lagged,
        }
    }
}

async fn send_control(
    peer: &mut WebSocket,
    control: MarketStreamControl,
    shutdown: &mut Option<StreamShutdown>,
) -> SendOutcome {
    send(peer, MarketStreamFrame::Control(control), shutdown).await
}

async fn serve(
    mut peer: WebSocket,
    mut session: MarketStreamSession,
    state: AppState,
    mut shutdown: Option<StreamShutdown>,
    _permit: OwnedSemaphorePermit,
) {
    let mut idle_auth = interval(AUTH_INTERVAL);
    idle_auth.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let send_lagged = 'connection: loop {
        let subscribed = session.is_subscribed();
        tokio::select! {
            () = stopping(&mut shutdown) => break false,
            _ = idle_auth.tick(), if !subscribed => {
                if let Err(code) = session.authorize() {
                    send_control(&mut peer, MarketStreamControl::Error { code }, &mut shutdown).await;
                    break false;
                }
            }
            input = peer.recv() => {
                let control = match input {
                    Some(Ok(Message::Text(text))) => match session.command(text.as_str()) {
                        Ok(control) => control,
                        Err(code) => Some(MarketStreamControl::Error { code }),
                    },
                    Some(Ok(Message::Binary(_))) => Some(MarketStreamControl::Error {
                        code: MarketStreamError::InvalidSubscription,
                    }),
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => None,
                    _ => break false,
                };
                if let Some(control) = control {
                    let revoked = control == (MarketStreamControl::Error {
                        code: MarketStreamError::AuthorizationRevoked,
                    });
                    let sent = send_control(&mut peer, control, &mut shutdown).await;
                    if sent != SendOutcome::Sent || revoked {
                        break !revoked && sent == SendOutcome::Lagged;
                    }
                }
            }
            frames = session.poll(&state.finance), if subscribed => {
                for frame in frames {
                    if let Err(code) = session.authorize() {
                        send_control(&mut peer, MarketStreamControl::Error { code }, &mut shutdown).await;
                        break 'connection false;
                    }
                    let sent = send(&mut peer, frame, &mut shutdown).await;
                    if sent != SendOutcome::Sent {
                        break 'connection sent == SendOutcome::Lagged;
                    }
                }
            }
        }
    };
    let can_send_reset = send_lagged && session.authorize().is_ok();
    drop(session);
    if can_send_reset {
        send_control(
            &mut peer,
            MarketStreamControl::Reset {
                reason: ResetReason::ConsumerLagged,
            },
            &mut shutdown,
        )
        .await;
    }
    let _ = timeout(SEND_TIMEOUT, peer.send(Message::Close(None))).await;
}
