use std::{collections::HashSet, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use utoipa::{OpenApi, ToSchema};

use crate::{
    AppState, ErrorBody, api_error, database::Database, market_preferences::MarketPreferences,
    market_types::InstrumentId,
};

pub(crate) const WATCHLIST: &str = "/api/v1/watchlists/{id}";
const MAX_ITEMS: usize = 64;

#[derive(Serialize, ToSchema)]
struct Watchlist {
    #[schema(pattern = "^[a-z0-9][a-z0-9_-]{0,63}$")]
    id: String,
    #[schema(minimum = 0)]
    revision: i64,
    #[schema(max_items = 64)]
    instrument_ids: Vec<InstrumentId>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateWatchlist {
    #[schema(minimum = 0)]
    revision: i64,
    #[schema(max_items = 64)]
    /// Unique F1 identifiers, in the order to store and display.
    instrument_ids: Vec<InstrumentId>,
}

fn invalid() -> Response {
    api_error(
        StatusCode::BAD_REQUEST,
        "invalid_watchlist",
        "Invalid watchlist identifier, revision or instrument list",
    )
}

fn valid_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    let lower_digit = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    id.len() <= 64
        && bytes.next().is_some_and(lower_digit)
        && bytes.all(|byte| lower_digit(byte) || matches!(byte, b'-' | b'_'))
}

fn unavailable() -> Response {
    api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "watchlist_store_unavailable",
        "Watchlist storage is unavailable",
    )
}

pub(crate) async fn demand_ids(db: &Database) -> Result<Vec<String>, Response> {
    let mut connection = db.connection().await.map_err(|_| unavailable())?;
    sqlx::query_scalar(
        "SELECT DISTINCT instrument_id COLLATE \"C\"
         FROM app.market_watchlists CROSS JOIN LATERAL unnest(instrument_ids) AS ids(instrument_id)
         ORDER BY 1",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(|_| unavailable())
}

#[utoipa::path(get, path = "/api/v1/watchlists/{id}", operation_id = "getWatchlist",
    params(("id" = String, Path, pattern = "^[a-z0-9][a-z0-9_-]{0,63}$")),
    security(("adminSession" = []), ("adminBearer" = [])),
    responses((status = 200, body = Watchlist), (status = 400, body = ErrorBody),
        (status = 401, body = ErrorBody), (status = 403, body = ErrorBody),
        (status = 429, body = ErrorBody), (status = 503, body = ErrorBody)))]
async fn read(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Watchlist>, Response> {
    if !valid_id(&id) {
        return Err(invalid());
    }
    let mut connection = state.db.connection().await.map_err(|_| unavailable())?;
    let (revision, ids) = sqlx::query_as::<_, (i64, Vec<String>)>(
        "SELECT revision, instrument_ids FROM app.market_watchlists WHERE id = $1",
    )
    .bind(&id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(|_| unavailable())?
    .unwrap_or_default();
    let instrument_ids = ids
        .into_iter()
        .map(InstrumentId::parse)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(unavailable)?;
    Ok(Json(Watchlist {
        id,
        revision,
        instrument_ids,
    }))
}

#[utoipa::path(put, path = "/api/v1/watchlists/{id}", operation_id = "putWatchlist",
    description = "Saves the new watchlist revision before submitting the complete subscription demand to finance. Forwarding errors leave the saved revision in place.",
    params(("id" = String, Path, pattern = "^[a-z0-9][a-z0-9_-]{0,63}$"),
        ("X-CSRF-Token" = Option<String>, Header, description = "Required for browser sessions")),
    request_body = UpdateWatchlist, security(("adminSession" = []), ("adminBearer" = [])),
    responses((status = 200, body = Watchlist,
        headers(("X-Finance-Subscription-State" = String, description = "Finance collection state: enabled or disabled"))),
        (status = 400, body = ErrorBody),
        (status = 401, body = ErrorBody), (status = 403, body = ErrorBody),
        (status = 409, body = ErrorBody), (status = 413, body = ErrorBody),
        (status = 429, body = ErrorBody),
        (status = 502, body = ErrorBody), (status = 503, body = ErrorBody),
        (status = 504, body = ErrorBody)))]
async fn update(
    State(state): State<AppState>,
    Extension(preferences): Extension<Arc<MarketPreferences>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateWatchlist>, JsonRejection>,
) -> Result<impl IntoResponse, Response> {
    if !valid_id(&id) {
        return Err(invalid());
    }
    let Json(request) = body.map_err(|error| {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            api_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "Request body is too large",
            )
        } else {
            invalid()
        }
    })?;
    let revision = request
        .revision
        .checked_add(1)
        .filter(|_| request.revision >= 0)
        .ok_or_else(invalid)?;
    let ids: Vec<_> = request
        .instrument_ids
        .iter()
        .map(InstrumentId::as_str)
        .collect();
    if ids.len() > MAX_ITEMS || ids.iter().collect::<HashSet<_>>().len() != ids.len() {
        return Err(invalid());
    }
    let _update = preferences.demand_updates.lock().await;
    // The v1 site has one administrator principal. Each compare-and-swap is
    // a single PG statement and can be committed at most once per revision.
    let mut connection = state
        .db
        .write_connection()
        .await
        .map_err(|_| unavailable())?;
    let updated = if request.revision == 0 {
        sqlx::query(
            "INSERT INTO app.market_watchlists (id, revision, instrument_ids)
             VALUES ($1, $2, $3) ON CONFLICT (id) DO NOTHING",
        )
        .bind(&id)
        .bind(revision)
        .bind(&ids)
        .execute(&mut *connection)
        .await
    } else {
        sqlx::query(
            "UPDATE app.market_watchlists SET revision = $2, instrument_ids = $3
             WHERE id = $1 AND revision = $4",
        )
        .bind(&id)
        .bind(revision)
        .bind(&ids)
        .bind(request.revision)
        .execute(&mut *connection)
        .await
    }
    .map_err(|_| unavailable())?;
    drop(connection);
    if updated.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::CONFLICT,
            "watchlist_revision_conflict",
            "Read the current watchlist revision before retrying",
        ));
    }
    let headers = preferences.forward_demand(&state).await?;
    Ok((
        headers,
        Json(Watchlist {
            id,
            revision,
            instrument_ids: request.instrument_ids,
        }),
    ))
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route(WATCHLIST, get(read).put(update))
}

#[derive(OpenApi)]
#[openapi(
    paths(read, update),
    components(schemas(Watchlist, UpdateWatchlist, InstrumentId))
)]
pub(crate) struct WatchlistApi;
