use super::{
    Access, ErrorBody, Fault, Limits, MAX_FRAME_BYTES, StreamQuery, UPSTREAM_IDLE_TIMEOUT,
    api_error, closed_during_open, parsed, upstream, wire,
};
use crate::{AppState, StreamShutdown};
use axum::{
    Extension,
    extract::{
        Query, State,
        rejection::QueryRejection,
        ws::{
            CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code,
            rejection::WebSocketUpgradeRejection,
        },
    },
    http::{HeaderMap, StatusCode, Version, header},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use std::{sync::Arc, time::Duration};
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message as UpstreamMessage,
        handshake::{client::generate_key, derive_accept_key},
        protocol::{Role, WebSocketConfig},
    },
};

const WRITE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_BUFFER_BYTES: usize = MAX_FRAME_BYTES + 256;
type UpstreamSocket = WebSocketStream<reqwest::Upgraded>;

#[utoipa::path(get, path = "/api/v1/options/flow/ws", operation_id = "optionsFlowWs",
    description = "RFC 6455 upgrade for F16 option-flow 1.0. See finance.option-flow-ws.schema.json for status, observation and reset_required. Clients receive data and reconnect with fresh authorization. Session cookies require an explicit same-origin Origin.",
    params(StreamQuery), security(("adminSession" = []), ("adminBearer" = [])),
    responses(
        (status = 101, description = "Option-flow WebSocket opened"),
        (status = 400, body = ErrorBody), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 429, body = ErrorBody),
        (status = 502, body = ErrorBody), (status = 503, body = ErrorBody),
        (status = 504, body = ErrorBody)
    ))]
pub(super) async fn upgrade(
    State(state): State<AppState>,
    Extension(limits): Extension<Arc<Limits>>,
    shutdown: Option<Extension<StreamShutdown>>,
    headers: HeaderMap,
    query: Result<Query<StreamQuery>, QueryRejection>,
    websocket: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, Response> {
    let query = parsed(query).map_err(IntoResponse::into_response)?;
    let con_id = query.check().map_err(IntoResponse::into_response)?;
    let websocket = websocket.map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_option_flow_websocket_upgrade",
            "A valid WebSocket upgrade is required",
        )
    })?;
    let permit = limits.acquire().map_err(IntoResponse::into_response)?;
    let mut access = Access::new(&state, &headers, true, shutdown, permit);
    let upstream = tokio::select! {
        biased;
        () = access.stopped() => return Err(closed_during_open(&access)),
        upstream = state.finance.open_stream(async |cfg| {
            connect(upstream::ws(cfg, con_id, query.cursor.as_deref())).await
        }) => upstream.map_err(IntoResponse::into_response)?,
    };
    access.check().map_err(IntoResponse::into_response)?;
    Ok(websocket
        .max_frame_size(1024)
        .max_message_size(1024)
        .read_buffer_size(1024)
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_BUFFER_BYTES)
        .on_upgrade(move |peer| serve(peer, upstream, access, query.con_id)))
}

async fn connect(request: reqwest::RequestBuilder) -> Result<UpstreamSocket, Fault> {
    let key = generate_key();
    let response = request
        .version(Version::HTTP_11)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_VERSION, "13")
        .header(header::SEC_WEBSOCKET_KEY, &key)
        .send()
        .await
        .map_err(Fault::stream_transport)?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Err(Fault::stream_status(response.status()));
    }
    let headers = response.headers();
    if !has_token(headers, header::CONNECTION, "upgrade")
        || !has_token(headers, header::UPGRADE, "websocket")
        || headers.get_all(header::SEC_WEBSOCKET_ACCEPT).iter().count() != 1
        || headers
            .get(header::SEC_WEBSOCKET_ACCEPT)
            .and_then(|value| value.to_str().ok())
            != Some(derive_accept_key(key.as_bytes()).as_str())
        || headers.contains_key(header::SEC_WEBSOCKET_PROTOCOL)
        || headers.contains_key(header::SEC_WEBSOCKET_EXTENSIONS)
    {
        return Err(Fault::invalid_response());
    }
    let stream = response.upgrade().await.map_err(Fault::stream_transport)?;
    let config = WebSocketConfig::default()
        .read_buffer_size(8192)
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_BUFFER_BYTES)
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES));
    Ok(WebSocketStream::from_raw_socket(stream, Role::Client, Some(config)).await)
}

fn has_token(headers: &HeaderMap, name: header::HeaderName, token: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(token))
        })
    })
}

async fn serve(mut peer: WebSocket, mut upstream: UpstreamSocket, mut access: Access, con_id: i32) {
    let idle = sleep(UPSTREAM_IDLE_TIMEOUT);
    tokio::pin!(idle);
    let code = loop {
        let frame = tokio::select! {
            biased;
            () = access.stopped() => break close_code::POLICY,
            () = &mut idle => break close_code::AGAIN,
            frame = peer.recv() => match frame {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(Message::Text(_) | Message::Binary(_))) => break close_code::POLICY,
                _ => return,
            },
            frame = upstream.next() => frame,
        };
        idle.as_mut().reset(Instant::now() + UPSTREAM_IDLE_TIMEOUT);
        let (message, reset) = match frame {
            Some(Ok(UpstreamMessage::Text(text))) => {
                let checked = serde_json::from_str::<wire::Envelope>(text.as_str())
                    .ok()
                    .and_then(|frame| {
                        wire::check(frame.event, frame.id, frame.data.get(), con_id).ok()
                    });
                let Some(reset) = checked else {
                    break close_code::POLICY;
                };
                (Message::Text(text.as_str().into()), reset)
            }
            Some(Ok(UpstreamMessage::Ping(_))) => {
                // Tungstenite queues the upstream pong on receipt.
                if !matches!(timeout(WRITE_TIMEOUT, upstream.flush()).await, Ok(Ok(()))) {
                    break close_code::AGAIN;
                }
                (Message::Ping(Default::default()), false)
            }
            Some(Ok(UpstreamMessage::Pong(_))) => continue,
            Some(Ok(UpstreamMessage::Close(_))) | None => break close_code::NORMAL,
            _ => break close_code::AGAIN,
        };
        if access.check().is_err() {
            break close_code::POLICY;
        }
        tokio::select! {
            biased;
            // The sink may still hold part of the interrupted frame. Drop it
            // instead of letting a subsequent close flush buffered data.
            () = access.stopped() => return,
            sent = timeout(WRITE_TIMEOUT, peer.send(message)) => match sent {
                Ok(Ok(())) => {}
                _ => return,
            }
        }
        if reset {
            break close_code::NORMAL;
        }
    };
    drop(upstream);
    let _ = timeout(
        WRITE_TIMEOUT,
        peer.send(Message::Close(Some(CloseFrame {
            code,
            reason: "".into(),
        }))),
    )
    .await;
}
