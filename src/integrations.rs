mod store;

use crate::{AppState, ErrorBody, api_error, auth::Product};
use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{
        Path, RawQuery, State,
        rejection::{BytesRejection, JsonRejection, PathRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, patch, post, put},
};
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin, sync::Arc};
use store::{Change, CredentialRef, Error, Integration, Outcome, TestInput, TestView};
use utoipa::ToSchema;
use uuid::Uuid;
use zeroize::Zeroize;

#[derive(Clone, Copy, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum Connector {
    BinanceSpot,
    BinanceUsdM,
    HyperliquidPerp,
    LicensedOptions,
}

#[derive(Clone, Copy, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "UPPERCASE")]
enum Symbol {
    Btcusdt,
    Ethusdt,
    Btc,
    Eth,
    Spx,
}

#[derive(Clone, Copy, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum AuthKind {
    None,
    ApiToken,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum TestMethod {
    PublicMarketSample,
    AuthorizedRead,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum FeeClass {
    Free,
    Unavailable,
}

#[derive(Serialize, ToSchema)]
struct CatalogEntry {
    connector: Connector,
    title: &'static str,
    product: Product,
    auth_kind: AuthKind,
    settings_fields: Vec<&'static str>,
    credential_fields: Vec<&'static str>,
    file_types: Vec<&'static str>,
    symbols: Vec<Symbol>,
    test_method: TestMethod,
    test_available: bool,
    fee_class: FeeClass,
    auto_backfill: bool,
}

impl Connector {
    const ALL: [Self; 4] = [
        Self::BinanceSpot,
        Self::BinanceUsdM,
        Self::HyperliquidPerp,
        Self::LicensedOptions,
    ];

    fn id(self) -> &'static str {
        match self {
            Self::BinanceSpot => "binance_spot",
            Self::BinanceUsdM => "binance_usd_m",
            Self::HyperliquidPerp => "hyperliquid_perp",
            Self::LicensedOptions => "licensed_options",
        }
    }

    fn symbols(self) -> &'static [Symbol] {
        match self {
            Self::BinanceSpot | Self::BinanceUsdM => &[Symbol::Btcusdt, Symbol::Ethusdt],
            Self::HyperliquidPerp => &[Symbol::Btc, Symbol::Eth],
            Self::LicensedOptions => &[Symbol::Spx],
        }
    }

    fn auth_kind(self) -> AuthKind {
        match self {
            Self::LicensedOptions => AuthKind::ApiToken,
            _ => AuthKind::None,
        }
    }

    fn catalog(self) -> CatalogEntry {
        let title = match self {
            Self::BinanceSpot => "Binance Spot",
            Self::BinanceUsdM => "Binance USDⓈ-M",
            Self::HyperliquidPerp => "Hyperliquid Perp",
            Self::LicensedOptions => "Licensed Options",
        };
        let requires_token = self.auth_kind() == AuthKind::ApiToken;
        let (product, test_method, fee_class, credential_fields) = if requires_token {
            (
                Product::RestrictedMarketData,
                TestMethod::AuthorizedRead,
                FeeClass::Unavailable,
                vec!["api_token"],
            )
        } else {
            (
                Product::MarketStatus,
                TestMethod::PublicMarketSample,
                FeeClass::Free,
                Vec::new(),
            )
        };
        CatalogEntry {
            connector: self,
            title,
            product,
            auth_kind: self.auth_kind(),
            settings_fields: vec!["symbol"],
            credential_fields,
            file_types: Vec::new(),
            symbols: self.symbols().to_vec(),
            test_method,
            test_available: false,
            fee_class,
            auto_backfill: false,
        }
    }
}

#[derive(Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    symbol: Symbol,
}

impl Settings {
    fn validate(self, connector: Connector) -> Result<serde_json::Value, ApiError> {
        if !connector.symbols().contains(&self.symbol) {
            return Err(ApiError::Invalid);
        }
        serde_json::to_value(self).map_err(|_| ApiError::Store(Error::Internal))
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Create {
    connector: Connector,
    settings: Settings,
}

#[derive(Deserialize, ToSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Update {
    Configure { settings: Settings },
    Disable,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credential {
    #[schema(write_only, min_length = 16, max_length = 4096)]
    api_token: String,
}

impl Credential {
    fn validate(&self) -> Result<(), ApiError> {
        let token = self.api_token.trim_end_matches('=');
        if !(16..=4096).contains(&self.api_token.len())
            || self.api_token.len() - token.len() > 2
            || !token.starts_with(|c: char| c.is_ascii_alphanumeric())
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~+".contains(&b))
        {
            return Err(ApiError::Invalid);
        }
        Ok(())
    }
}

impl Drop for Credential {
    fn drop(&mut self) {
        self.api_token.zeroize();
    }
}

pub(crate) trait ConnectorTester: Send + Sync {
    fn test(&self, input: TestInput) -> Pin<Box<dyn Future<Output = Outcome> + Send + '_>>;
}

struct DeferredTests;

impl ConnectorTester for DeferredTests {
    fn test(&self, input: TestInput) -> Pin<Box<dyn Future<Output = Outcome> + Send + '_>> {
        Box::pin(async move {
            // W2 wires authorized connector test jobs through the generated finance client.
            // W1 has no connection target or credential reader.
            let TestInput {
                connector,
                settings,
                credential,
                ..
            } = input;
            let Some(connector) = Connector::ALL.into_iter().find(|c| c.id() == connector) else {
                return Outcome::Failed;
            };
            if serde_json::from_value::<Settings>(settings)
                .ok()
                .is_none_or(|settings| !connector.symbols().contains(&settings.symbol))
                || (connector.auth_kind() == AuthKind::ApiToken && credential.is_none())
            {
                return Outcome::Failed;
            }
            Outcome::Unavailable
        })
    }
}

pub(crate) fn routes() -> Router<AppState> {
    let tester: Arc<dyn ConnectorTester> = Arc::new(DeferredTests);
    Router::new()
        .route("/api/v1/integrations/catalog", get(catalog))
        .route("/api/v1/integrations", get(list).post(create))
        .route("/api/v1/integrations/{id}", patch(update))
        .route(
            "/api/v1/integrations/{id}/credential",
            put(put_credential).delete(delete_credential),
        )
        .route(
            "/api/v1/integrations/{id}/tests",
            post(start_test).layer(Extension(tester)),
        )
        .route("/api/v1/integration-tests/{id}", get(get_test))
}

pub(crate) enum ApiError {
    Invalid,
    RevisionRequired,
    PayloadTooLarge,
    CredentialsUnavailable,
    Store(Error),
}

impl From<Error> for ApiError {
    fn from(value: Error) -> Self {
        Self::Store(value)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::Invalid => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_request",
                "Invalid integration request",
            ),
            Self::RevisionRequired => (
                StatusCode::PRECONDITION_REQUIRED,
                "revision_required",
                "If-Match with a quoted integration revision is required",
            ),
            Self::PayloadTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "Request body is too large",
            ),
            Self::CredentialsUnavailable | Self::Store(Error::CredentialUnavailable) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "credential_store_unavailable",
                "Credential storage is unavailable",
            ),
            Self::Store(Error::Missing) => (
                StatusCode::NOT_FOUND,
                "integration_not_found",
                "Integration or test was not found",
            ),
            Self::Store(Error::Conflict) => (
                StatusCode::CONFLICT,
                "integration_conflict",
                "Integration revision or state changed",
            ),
            Self::Store(Error::Unavailable) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "database_unavailable",
                "Integration storage is unavailable",
            ),
            Self::Store(Error::Internal) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Internal server error",
            ),
        };
        api_error(status, code, message)
    }
}

fn request_json<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    match body {
        Ok(Json(body)) => Ok(body),
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => Err(ApiError::PayloadTooLarge),
        Err(_) => Err(ApiError::Invalid),
    }
}

fn request_id(path: Result<Path<String>, PathRejection>) -> Result<Uuid, ApiError> {
    let Path(id) = path.map_err(|_| ApiError::Invalid)?;
    Uuid::parse_str(&id).map_err(|_| ApiError::Invalid)
}

fn revision(headers: &HeaderMap) -> Result<i64, ApiError> {
    let mut values = headers.get_all(header::IF_MATCH).iter();
    let value = values.next().ok_or(ApiError::RevisionRequired)?;
    let revision = value
        .to_str()
        .ok()
        .and_then(|value| value.strip_prefix('"')?.strip_suffix('"'))
        .and_then(|value| value.parse::<i64>().ok().filter(|n| n.to_string() == value))
        .filter(|n| *n > 0 && values.next().is_none());
    revision.ok_or(ApiError::Invalid)
}

fn empty_body(body: Result<Bytes, BytesRejection>) -> Result<(), ApiError> {
    match body {
        Ok(body) if body.is_empty() => Ok(()),
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => Err(ApiError::PayloadTooLarge),
        _ => Err(ApiError::Invalid),
    }
}

fn integration_response(status: StatusCode, integration: Integration) -> Response {
    let tag = format!("\"{}\"", integration.revision);
    let etag = HeaderValue::from_str(&tag).expect("revision is valid ASCII");
    (status, [(header::ETAG, etag)], Json(integration)).into_response()
}

fn stored_connector(integration: &Integration) -> Result<Connector, ApiError> {
    Connector::ALL
        .into_iter()
        .find(|c| c.id() == integration.connector)
        .ok_or(ApiError::Store(Error::Internal))
}

#[utoipa::path(get, path="/api/v1/integrations/catalog",
    security(("adminBearer" = []), ("adminSession" = [])),
    responses((status=200, body=[CatalogEntry]),(status=401,body=ErrorBody),(status=403,body=ErrorBody)))]
pub(crate) async fn catalog() -> impl IntoResponse {
    Json(
        Connector::ALL
            .into_iter()
            .map(Connector::catalog)
            .collect::<Vec<_>>(),
    )
}

#[utoipa::path(get, path="/api/v1/integrations",
    security(("adminBearer" = []), ("adminSession" = [])),
    responses((status=200,body=[Integration]),(status=401,body=ErrorBody),
        (status=403,body=ErrorBody),(status=503,body=ErrorBody)))]
pub(crate) async fn list(State(s): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(store::list(&s.db).await?))
}

#[utoipa::path(post, path="/api/v1/integrations", request_body=Create,
    security(("adminBearer" = []), ("adminSession" = [])),
    params(("Origin" = Option<String>, Header, description="Matches the configured HTTPS origin when sent and is required with the session cookie"),
        ("X-CSRF-Token" = Option<String>, Header, description="Required with the session cookie")),
    responses((status=201,body=Integration,headers(("ETag" = String))),
        (status=401,body=ErrorBody),(status=403,body=ErrorBody),(status=413,body=ErrorBody),
        (status=422,body=ErrorBody),(status=429,body=ErrorBody),(status=503,body=ErrorBody)))]
pub(crate) async fn create(
    State(s): State<AppState>,
    body: Result<Json<Create>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Create {
        connector,
        settings,
    } = request_json(body)?;
    let settings = settings.validate(connector)?;
    let integration = store::create(&s.db, connector.id(), settings).await?;
    Ok(integration_response(StatusCode::CREATED, integration))
}

#[utoipa::path(patch, path="/api/v1/integrations/{id}", request_body=Update,
    security(("adminBearer" = []), ("adminSession" = [])),
    params(("id" = String, Path, format = "uuid"),
        ("If-Match" = String, Header, description="Quoted integration revision"),
        ("Origin" = Option<String>, Header, description="Matches the configured HTTPS origin when sent and is required with the session cookie"),
        ("X-CSRF-Token" = Option<String>, Header, description="Required with the session cookie")),
    responses((status=200,body=Integration,headers(("ETag" = String))),
        (status=401,body=ErrorBody),(status=403,body=ErrorBody),(status=404,body=ErrorBody),
        (status=409,body=ErrorBody),(status=413,body=ErrorBody),(status=422,body=ErrorBody),
        (status=428,body=ErrorBody),(status=429,body=ErrorBody),(status=503,body=ErrorBody)))]
pub(crate) async fn update(
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<Update>, JsonRejection>,
) -> Result<Response, ApiError> {
    let id = request_id(path)?;
    let revision = revision(&headers)?;
    let change = match request_json(body)? {
        Update::Configure { settings } => {
            let connector = stored_connector(&store::get(&s.db, id).await?)?;
            Change::Settings(settings.validate(connector)?)
        }
        Update::Disable => Change::Disable,
    };
    let integration = store::change(&s.db, id, revision, change).await?;
    Ok(integration_response(StatusCode::OK, integration))
}

#[utoipa::path(put, path="/api/v1/integrations/{id}/credential", request_body=Credential,
    security(("adminBearer" = []), ("adminSession" = [])),
    params(("id" = String, Path, format = "uuid"),
        ("If-Match" = String, Header, description="Quoted integration revision"),
        ("Origin" = Option<String>, Header, description="Matches the configured HTTPS origin when sent and is required with the session cookie"),
        ("X-CSRF-Token" = Option<String>, Header, description="Required with the session cookie")),
    responses((status=200,body=Integration,headers(("ETag" = String))),
        (status=401,body=ErrorBody),(status=403,body=ErrorBody),(status=404,body=ErrorBody),
        (status=409,body=ErrorBody),(status=413,body=ErrorBody),(status=422,body=ErrorBody),
        (status=428,body=ErrorBody),(status=429,body=ErrorBody),(status=503,body=ErrorBody)))]
pub(crate) async fn put_credential(
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<Credential>, JsonRejection>,
) -> Result<Response, ApiError> {
    let id = request_id(path)?;
    let revision = revision(&headers)?;
    let credential = request_json(body)?;
    credential.validate()?;
    let store = s
        .credential_store
        .as_ref()
        .ok_or(ApiError::CredentialsUnavailable)?;
    let connector = stored_connector(&store::get(&s.db, id).await?)?;
    if connector.auth_kind() != AuthKind::ApiToken {
        return Err(ApiError::Invalid);
    }
    let reference = Uuid::new_v4();
    let integration = store::set_credential(
        &s.db,
        id,
        revision,
        CredentialRef {
            reference,
            mask: "********".into(),
            format: "jwe".into(),
        },
        async {
            store
                .seal(reference, credential.api_token.as_bytes().to_vec())
                .await
                .map_err(|_| Error::CredentialUnavailable)
        },
    )
    .await?;
    Ok(integration_response(StatusCode::OK, integration))
}

#[utoipa::path(delete, path="/api/v1/integrations/{id}/credential",
    security(("adminBearer" = []), ("adminSession" = [])),
    params(("id" = String, Path, format = "uuid"),
        ("If-Match" = String, Header, description="Quoted integration revision"),
        ("Origin" = Option<String>, Header, description="Matches the configured HTTPS origin when sent and is required with the session cookie"),
        ("X-CSRF-Token" = Option<String>, Header, description="Required with the session cookie")),
    responses((status=200,body=Integration,headers(("ETag" = String))),
        (status=401,body=ErrorBody),(status=403,body=ErrorBody),(status=404,body=ErrorBody),
        (status=409,body=ErrorBody),(status=413,body=ErrorBody),(status=422,body=ErrorBody),
        (status=428,body=ErrorBody),(status=429,body=ErrorBody),(status=503,body=ErrorBody)))]
pub(crate) async fn delete_credential(
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let id = request_id(path)?;
    let revision = revision(&headers)?;
    empty_body(body)?;
    let integration = store::change(&s.db, id, revision, Change::RevokeCredential).await?;
    Ok(integration_response(StatusCode::OK, integration))
}

#[utoipa::path(post, path="/api/v1/integrations/{id}/tests",
    security(("adminBearer" = []), ("adminSession" = [])),
    params(("id" = String, Path, format = "uuid"),
        ("If-Match" = String, Header, description="Quoted integration revision"),
        ("Origin" = Option<String>, Header, description="Matches the configured HTTPS origin when sent and is required with the session cookie"),
        ("X-CSRF-Token" = Option<String>, Header, description="Required with the session cookie")),
    responses((status=202,body=TestView),(status=401,body=ErrorBody),(status=403,body=ErrorBody),
        (status=404,body=ErrorBody),(status=409,body=ErrorBody),(status=413,body=ErrorBody),
        (status=422,body=ErrorBody),(status=428,body=ErrorBody),(status=429,body=ErrorBody),
        (status=503,body=ErrorBody)))]
pub(crate) async fn start_test(
    State(s): State<AppState>,
    Extension(tester): Extension<Arc<dyn ConnectorTester>>,
    path: Result<Path<String>, PathRejection>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let id = request_id(path)?;
    let revision = revision(&headers)?;
    if query.is_some() {
        return Err(ApiError::Invalid);
    }
    empty_body(body)?;
    let input = store::begin_test(&s.db, id, revision).await?;
    let test_id = Uuid::parse_str(&input.test.id).map_err(|_| Error::Internal)?;
    let outcome = tester.test(input).await;
    let test = store::finish_test(&s.db, id, test_id, outcome).await?;
    Ok((StatusCode::ACCEPTED, Json(test)))
}

#[utoipa::path(get, path="/api/v1/integration-tests/{id}",
    security(("adminBearer" = []), ("adminSession" = [])),
    params(("id" = String, Path, format = "uuid")),
    responses((status=200,body=TestView),(status=401,body=ErrorBody),(status=403,body=ErrorBody),
        (status=404,body=ErrorBody),(status=422,body=ErrorBody),(status=503,body=ErrorBody)))]
pub(crate) async fn get_test(
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(store::get_test(&s.db, request_id(path)?).await?))
}
