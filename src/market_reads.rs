use crate::{
    AppState, api_error,
    finance::{Fault, Finance},
};
use axum::{
    Json, Router,
    extract::{Query, State, rejection::QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use finance_client::{apis::market_read_api, models};
use serde::Deserialize;
use serde_json::{Value, json};

mod views;

const PRODUCTS: &str = "/api/v1/market/products";
const WAL: &str = "/api/v1/market/wal";
const PROJECTION: &str = "/api/v1/market/projection/latest";
const WATERMARKS: &str = "/api/v1/market/watermarks";
const FINANCE_GAPS: &str = "/api/v1/quality/finance-gaps";
const UPSTREAM_GAPS: &str = "/api/v1/quality/gaps";

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route(PRODUCTS, get(products))
        .route(WAL, get(wal))
        .route(PROJECTION, get(projected))
        .route(WATERMARKS, get(watermarks))
        .route(FINANCE_GAPS, get(gaps))
        .merge(views::routes())
}

pub(crate) fn is_private(path: &str) -> bool {
    matches!(
        path,
        PRODUCTS | WAL | PROJECTION | WATERMARKS | FINANCE_GAPS
    ) || views::is_private(path)
}

pub(crate) enum MarketReadError {
    InvalidQuery,
    DepthFocusRequired,
    IndicatorNotFound,
    DisplayDenied,
    InvalidResponse,
    Upstream(Fault),
}

impl MarketReadError {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::InvalidQuery | Self::DepthFocusRequired => StatusCode::BAD_REQUEST,
            Self::IndicatorNotFound => StatusCode::NOT_FOUND,
            Self::DisplayDenied => StatusCode::FORBIDDEN,
            Self::InvalidResponse => StatusCode::BAD_GATEWAY,
            Self::Upstream(fault) => fault.status,
        }
    }
}

impl IntoResponse for MarketReadError {
    fn into_response(self) -> Response {
        match self {
            Self::InvalidQuery => api_error(
                StatusCode::BAD_REQUEST,
                "market_query_invalid",
                "Missing or invalid market query parameters",
            ),
            Self::DepthFocusRequired => api_error(
                StatusCode::BAD_REQUEST,
                "depth_focus_required",
                "Select a depth focus or supply an instrument identifier",
            ),
            Self::IndicatorNotFound => api_error(
                StatusCode::NOT_FOUND,
                "market_indicator_not_found",
                "No indicator has this identifier",
            ),
            Self::DisplayDenied => api_error(
                StatusCode::FORBIDDEN,
                "market_display_denied",
                "The product does not permit display",
            ),
            Self::InvalidResponse => api_error(
                StatusCode::BAD_GATEWAY,
                "finance_invalid_response",
                "Invalid finance response",
            ),
            Self::Upstream(fault) => match fault.status {
                StatusCode::BAD_REQUEST => Self::InvalidQuery.into_response(),
                StatusCode::NOT_FOUND => api_error(
                    StatusCode::NOT_FOUND,
                    "market_data_not_found",
                    "No recorded market data matches the query",
                ),
                status if status.is_client_error() => api_error(
                    status,
                    "finance_request_rejected",
                    "Finance rejected the request",
                ),
                _ => fault.into_response(),
            },
        }
    }
}

impl From<Fault> for MarketReadError {
    fn from(fault: Fault) -> Self {
        Self::Upstream(fault)
    }
}

fn parsed<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, MarketReadError> {
    query
        .map(|Query(query)| query)
        .map_err(|_| MarketReadError::InvalidQuery)
}

fn check_product(
    access: &models::ProductPolicy,
    source: &str,
    product: &str,
) -> Result<(), MarketReadError> {
    if access.source != source || access.product != product || !access.record_local {
        return Err(MarketReadError::InvalidResponse);
    }
    Ok(())
}

fn check_display(access: &models::ProductPolicy) -> Result<(), MarketReadError> {
    if !access.display_live {
        return Err(MarketReadError::DisplayDenied);
    }
    Ok(())
}

fn check_record(
    access: &models::ProductPolicy,
    record: &models::WalRecord,
) -> Result<(), MarketReadError> {
    let event = &record.event;
    if event.source != access.source
        || event.product != access.product
        || event.license != access.license
        || !event_version_matches(event)
    {
        return Err(MarketReadError::InvalidResponse);
    }
    Ok(())
}

pub(crate) fn event_version_matches(event: &models::Event) -> bool {
    use models::Message;

    let message = event.message.as_ref();
    let perpetual_context = matches!(
        message,
        Message::MessageOneOf(_) | Message::MessageOneOf1(_) | Message::MessageOneOf7(_)
    );
    let option_history = matches!(
        message,
        Message::MessageOneOf8(_) | Message::MessageOneOf9(_)
    );
    match event.schema_version {
        models::SchemaVersion::Variant10 => !perpetual_context && !option_history,
        models::SchemaVersion::Variant20 => {
            event.asset_kind == models::AssetKind::Perpetual
                && event.revision == "0"
                && (perpetual_context
                    || matches!(
                        message,
                        Message::MessageOneOf3(_)
                            | Message::MessageOneOf4(_)
                            | Message::MessageOneOf5(_)
                            | Message::MessageOneOf6(_)
                    ))
        }
        models::SchemaVersion::Variant30 => {
            option_history && event.asset_kind == models::AssetKind::Option && event.revision == "0"
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProductQuery {
    source: Option<String>,
    offset: Option<u64>,
    limit: Option<u64>,
}

async fn products(
    State(state): State<AppState>,
    query: Result<Query<ProductQuery>, QueryRejection>,
) -> Result<Json<models::MarketProducts>, MarketReadError> {
    let query = &parsed(query)?;
    state
        .finance
        .read(|cfg| async move {
            market_read_api::products(&cfg, query.source.as_deref(), query.offset, query.limit)
                .await
        })
        .await
        .map(Json)
        .map_err(Into::into)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WalQuery {
    source: String,
    product: String,
    after: Option<String>,
    through: Option<String>,
    limit: Option<u64>,
}

pub(crate) async fn wal_page(
    finance: &Finance,
    source: &str,
    product: &str,
    after: Option<&str>,
    through: Option<&str>,
    limit: Option<u64>,
) -> Result<models::MarketWalPage, MarketReadError> {
    let page = finance
        .read(|cfg| async move {
            market_read_api::wal(&cfg, source, product, after, through, limit).await
        })
        .await?;
    check_product(&page.access, source, product)?;
    check_display(&page.access)?;
    for record in &page.records {
        check_record(&page.access, record)?;
    }
    Ok(page)
}

async fn wal(
    State(state): State<AppState>,
    query: Result<Query<WalQuery>, QueryRejection>,
) -> Result<Json<models::MarketWalPage>, MarketReadError> {
    let query = parsed(query)?;
    wal_page(
        &state.finance,
        &query.source,
        &query.product,
        query.after.as_deref(),
        query.through.as_deref(),
        query.limit,
    )
    .await
    .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectionQuery {
    source: String,
    product: String,
    channel: String,
    instrument_id: String,
    epoch: String,
    seq: String,
}

async fn projected(
    State(state): State<AppState>,
    query: Result<Query<ProjectionQuery>, QueryRejection>,
) -> Result<Json<models::ProjectedMarketEvent>, MarketReadError> {
    let query = &parsed(query)?;
    let projected = state
        .finance
        .read(|cfg| async move {
            market_read_api::projected(
                &cfg,
                &query.source,
                &query.product,
                &query.channel,
                &query.instrument_id,
                &query.epoch,
                &query.seq,
            )
            .await
        })
        .await?;
    check_product(&projected.access, &query.source, &query.product)?;
    check_display(&projected.access)?;
    check_record(&projected.access, &projected.record)?;
    Ok(Json(projected))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedQuery {
    source: String,
    product: String,
}

async fn watermarks(
    State(state): State<AppState>,
    query: Result<Query<FeedQuery>, QueryRejection>,
) -> Result<Json<models::MarketWatermarks>, MarketReadError> {
    let query = parsed(query)?;
    product_watermarks(&state.finance, &query.source, &query.product)
        .await
        .map(Json)
}

async fn product_watermarks(
    finance: &Finance,
    source: &str,
    product: &str,
) -> Result<models::MarketWatermarks, MarketReadError> {
    let result = finance
        .read(|cfg| async move { market_read_api::watermarks(&cfg, source, product).await })
        .await?;
    check_product(&result.access, source, product)?;
    Ok(result)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GapQuery {
    source: String,
    product: String,
    channel: Option<String>,
    instrument_id: Option<String>,
    from_ts: Option<String>,
    to_ts: Option<String>,
    offset: Option<u64>,
    limit: Option<u64>,
}

async fn gaps(
    State(state): State<AppState>,
    query: Result<Query<GapQuery>, QueryRejection>,
) -> Result<Json<models::MarketQualityPage>, MarketReadError> {
    let query = &parsed(query)?;
    let result = state
        .finance
        .read(|cfg| async move {
            market_read_api::gaps(
                &cfg,
                &query.source,
                &query.product,
                query.channel.as_deref(),
                query.instrument_id.as_deref(),
                query.from_ts.as_deref(),
                query.to_ts.as_deref(),
                query.offset,
                query.limit,
            )
            .await
        })
        .await?;
    check_product(&result.access, &query.source, &query.product)?;
    Ok(Json(result))
}

pub(crate) fn register_openapi(spec: &mut Value, upstream: &Value) {
    views::register_openapi(spec);
    for (path, source, operation_id) in [
        (PRODUCTS, PRODUCTS, "marketProducts"),
        (WAL, WAL, "marketWal"),
        (PROJECTION, PROJECTION, "marketProjectedEvent"),
        (WATERMARKS, WATERMARKS, "marketWatermarks"),
        (FINANCE_GAPS, UPSTREAM_GAPS, "marketFinanceGaps"),
    ] {
        register_read_operation(spec, upstream, path, source, operation_id);
    }

    let upstream_schemas = upstream["components"]["schemas"]
        .as_object()
        .expect("finance components declare schemas");
    let schemas = &mut spec["components"]["schemas"];
    for name in [
        "AssetKind",
        "BookDelta",
        "BookLevel",
        "BookSide",
        "BookSnapshot",
        "BookUpdate",
        "ClosedBar",
        "ConnectionState",
        "Correction",
        "DecimalText",
        "Event",
        "FundingRate",
        "Gap",
        "GapCause",
        "Heartbeat",
        "InstrumentId",
        "MarketMessage",
        "MarketProducts",
        "MarketQualityPage",
        "MarketWalPage",
        "MarketWatermarks",
        "Message",
        "NonEmptyText",
        "OpenInterest",
        "OptionHistoryBatch",
        "OptionHistoryQuote",
        "OptionHistoryTrade",
        "PerpetualContext",
        "ProductPolicy",
        "ProjectedMarketEvent",
        "QualityReport",
        "RecoveryOutcome",
        "Reset",
        "SchemaVersion",
        "SourceObservation",
        "Status",
        "Token",
        "Trade",
        "TradeSide",
        "U64Text",
        "Venue",
        "WalRecord",
        "Watermarks",
    ] {
        schemas[name] = upstream_schemas[name].clone();
    }
}

pub(crate) fn register_read_operation(
    spec: &mut Value,
    upstream: &Value,
    path: &str,
    source: &str,
    operation_id: &str,
) {
    let mut item = upstream["paths"][source].clone();
    let operation = &mut item["get"];
    operation["operationId"] = json!(operation_id);
    operation["security"] = json!([{"adminBearer": []}, {"adminSession": []}]);
    let responses = operation["responses"]
        .as_object_mut()
        .expect("finance read is a GET operation with responses");
    responses.remove("500");
    for (status, description) in [
        ("401", "Authentication required"),
        ("403", "Product access denied"),
        ("429", "Read rate limit exceeded"),
        ("502", "Finance response error"),
        ("503", "Finance unavailable"),
        ("504", "Finance read timed out"),
    ] {
        responses.insert(
            status.into(),
            json!({
                "description": description,
                "content": {
                    "application/json": {
                        "schema": {"$ref": "#/components/schemas/ErrorBody"}
                    }
                }
            }),
        );
    }
    spec["paths"][path] = item;
}

#[cfg(test)]
mod tests;
