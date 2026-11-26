pub(crate) mod local_probe;
mod market_probe;
pub(crate) mod store;

use crate::{
    AppState, ErrorBody, api_error, auth::Product, config, notifications::model::AttemptOutcome,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        Path, RawQuery, State,
        rejection::{BytesRejection, JsonRejection, PathRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use store::{Change, CredentialRef, Error, Integration, Outcome, TestInput, TestView};
use utoipa::{
    Modify, ToSchema,
    openapi::{OpenApi, RefOr, Schema, schema::AdditionalProperties},
};
use uuid::Uuid;
use zeroize::Zeroize;

#[derive(Clone, Copy, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum Connector {
    BinanceSpot,
    BinanceUsdM,
    HyperliquidPerp,
    LicensedOptions,
    IbkrPaper,
    StrategyFile,
    ResearchFile,
    Smtp,
    GoogleDrive,
}

#[derive(Clone, Copy, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "UPPERCASE")]
enum Symbol {
    Btcusdt,
    Ethusdt,
    Btc,
    Eth,
    Spx,
    Spy,
    Qqq,
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
    PaperMarketSample,
    FileValidation,
    TestEmail,
    EncryptedLocalRoundTrip,
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
    profiles: Vec<Profile>,
    test_method: TestMethod,
    test_available: bool,
    fee_class: FeeClass,
    auto_backfill: bool,
}

impl Connector {
    const ALL: [Self; 9] = [
        Self::BinanceSpot,
        Self::BinanceUsdM,
        Self::HyperliquidPerp,
        Self::LicensedOptions,
        Self::IbkrPaper,
        Self::StrategyFile,
        Self::ResearchFile,
        Self::Smtp,
        Self::GoogleDrive,
    ];

    fn id(self) -> &'static str {
        match self {
            Self::BinanceSpot => "binance_spot",
            Self::BinanceUsdM => "binance_usd_m",
            Self::HyperliquidPerp => "hyperliquid_perp",
            Self::LicensedOptions => "licensed_options",
            Self::IbkrPaper => "ibkr_paper",
            Self::StrategyFile => "strategy_file",
            Self::ResearchFile => "research_file",
            Self::Smtp => "smtp",
            Self::GoogleDrive => "google_drive",
        }
    }

    fn symbols(self) -> &'static [Symbol] {
        match self {
            Self::BinanceSpot | Self::BinanceUsdM => &[Symbol::Btcusdt, Symbol::Ethusdt],
            Self::HyperliquidPerp => &[Symbol::Btc, Symbol::Eth],
            Self::LicensedOptions => &[Symbol::Spx],
            Self::IbkrPaper => &[Symbol::Spy, Symbol::Qqq],
            _ => &[],
        }
    }

    fn auth_kind(self) -> AuthKind {
        match self {
            Self::LicensedOptions => AuthKind::ApiToken,
            _ => AuthKind::None,
        }
    }

    fn catalog(
        self,
        config: &config::Settings,
        database_ready: bool,
        market_adapter_configured: bool,
    ) -> CatalogEntry {
        use TestMethod::*;

        let (title, product, test_method, test_available) = match self {
            Self::BinanceSpot => (
                "Binance Spot",
                Product::MarketStatus,
                PublicMarketSample,
                config.binance_probes_enabled && market_adapter_configured,
            ),
            Self::BinanceUsdM => (
                "Binance USDⓈ-M",
                Product::MarketStatus,
                PublicMarketSample,
                config.binance_probes_enabled && market_adapter_configured,
            ),
            Self::HyperliquidPerp => (
                "Hyperliquid Perp",
                Product::MarketStatus,
                PublicMarketSample,
                market_adapter_configured,
            ),
            Self::LicensedOptions => (
                "Licensed Options",
                Product::RestrictedMarketData,
                AuthorizedRead,
                false,
            ),
            Self::IbkrPaper => (
                "IBKR Paper",
                Product::RestrictedMarketData,
                PaperMarketSample,
                market_adapter_configured,
            ),
            Self::StrategyFile => (
                "Strategy File",
                Product::Integrations,
                FileValidation,
                config.local_probes.available(self.id()),
            ),
            Self::ResearchFile => (
                "Research File",
                Product::Integrations,
                FileValidation,
                config.local_probes.available(self.id()),
            ),
            Self::Smtp => ("SMTP", Product::Notifications, TestEmail, true),
            Self::GoogleDrive => (
                "Google Drive",
                Product::Integrations,
                EncryptedLocalRoundTrip,
                config.local_probes.available(self.id()),
            ),
        };
        let (settings_fields, profiles) = if self.symbols().is_empty() {
            (vec!["profile"], vec![Profile::Primary, Profile::Secondary])
        } else {
            (vec!["symbol"], Vec::new())
        };
        let (fee_class, credential_fields) = match self {
            Self::LicensedOptions => (FeeClass::Unavailable, vec!["api_token"]),
            Self::IbkrPaper => (FeeClass::Unavailable, Vec::new()),
            _ => (FeeClass::Free, Vec::new()),
        };
        CatalogEntry {
            connector: self,
            title,
            product,
            auth_kind: self.auth_kind(),
            settings_fields,
            credential_fields,
            file_types: match self {
                Self::StrategyFile => vec!["yaml", "yml", "json"],
                Self::ResearchFile => vec!["dataset.json", "bars.csv"],
                _ => Vec::new(),
            },
            symbols: self.symbols().to_vec(),
            profiles,
            test_method,
            test_available: test_available && database_ready,
            fee_class,
            auto_backfill: false,
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum Profile {
    Primary,
    Secondary,
}

#[derive(Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol: Option<Symbol>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<Profile>,
}

impl Settings {
    fn validate(self, connector: Connector) -> Result<serde_json::Value, ApiError> {
        let valid = match (self.symbol, self.profile) {
            (Some(symbol), None) => connector.symbols().contains(&symbol),
            (None, Some(_)) => connector.symbols().is_empty(),
            _ => false,
        };
        if !valid {
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
    // The struct variant makes Serde enforce deny_unknown_fields for disable.
    Disable {},
}

pub(crate) struct StrictUpdateSchema;

impl Modify for StrictUpdateSchema {
    fn modify(&self, spec: &mut OpenApi) {
        let schema = spec
            .components
            .as_mut()
            .and_then(|components| components.schemas.get_mut("Update"));
        let Some(RefOr::T(Schema::OneOf(update))) = schema else {
            panic!("Update must export a oneOf request schema");
        };
        // Utoipa 5.5 does not propagate deny_unknown_fields to enum branches.
        for variant in &mut update.items {
            let RefOr::T(Schema::Object(object)) = variant else {
                panic!("Update variants must export object schemas");
            };
            object.additional_properties = Some(Box::new(AdditionalProperties::FreeForm(false)));
        }
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credential {
    #[schema(
        write_only,
        min_length = 16,
        max_length = 4096,
        pattern = r"^[A-Za-z0-9][A-Za-z0-9._~+-]*={0,2}(?![\s\S])"
    )]
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

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", content = "details", rename_all = "snake_case")]
pub(crate) enum TestEvidence {
    Market(market_probe::MarketEvidence),
    Local(local_probe::LocalEvidence),
    Smtp(AttemptOutcome),
    Adapter(AdapterResult),
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AdapterResult {
    MissingCredential,
    Unsupported,
    DeadlineExceeded,
}

async fn probe(s: &AppState, input: &TestInput) -> (Outcome, TestEvidence) {
    match input.connector.as_str() {
        "ibkr_paper" | "binance_spot" | "binance_usd_m" | "hyperliquid_perp" => {
            let (outcome, evidence) =
                market_probe::test(&s.finance, input, s.config.binance_probes_enabled).await;
            (outcome, TestEvidence::Market(evidence))
        }
        "strategy_file" | "research_file" | "google_drive" => {
            let (outcome, evidence) = local_probe::test(&s.config.local_probes, input).await;
            (outcome, TestEvidence::Local(evidence))
        }
        "licensed_options" if input.credential.is_none() => (
            Outcome::Failed,
            TestEvidence::Adapter(AdapterResult::MissingCredential),
        ),
        _ => (
            Outcome::Unavailable,
            TestEvidence::Adapter(AdapterResult::Unsupported),
        ),
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/integrations/catalog", get(catalog))
        .route("/api/v1/integrations", get(list).post(create))
        .route(
            "/api/v1/integrations/{id}",
            get(get_integration).patch(update),
        )
        .route(
            "/api/v1/integrations/{id}/credential",
            put(put_credential).delete(delete_credential),
        )
        .route("/api/v1/integrations/{id}/tests", post(start_test))
        .route(
            "/api/v1/integrations/{id}/tests/{testId}",
            get(get_scoped_test),
        )
        .route("/api/v1/integrations/{id}/activate", post(activate))
        .route("/api/v1/integrations/{id}/rollback", post(rollback))
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
pub(crate) async fn catalog(State(s): State<AppState>) -> impl IntoResponse {
    let database_ready = s.db.status().ok;
    let market_adapter_configured = s.finance.integration_probe_client().is_some();
    Json(
        Connector::ALL
            .into_iter()
            .map(|connector| {
                connector.catalog(&s.config, database_ready, market_adapter_configured)
            })
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

#[utoipa::path(get, path="/api/v1/integrations/{id}",
    params(("id" = String, Path, format = "uuid")),
    security(("adminBearer" = []), ("adminSession" = [])),
    responses((status=200,body=Integration,headers(("ETag" = String))),
        (status=401,body=ErrorBody),(status=403,body=ErrorBody),(status=404,body=ErrorBody),
        (status=422,body=ErrorBody),(status=503,body=ErrorBody)))]
pub(crate) async fn get_integration(
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let integration = store::get(&s.db, request_id(path)?).await?;
    Ok(integration_response(StatusCode::OK, integration))
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
        Update::Disable {} => Change::Disable,
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
    responses((status=202,body=TestView,headers(("Location" = String))),
        (status=401,body=ErrorBody),(status=403,body=ErrorBody),
        (status=404,body=ErrorBody),(status=409,body=ErrorBody),(status=413,body=ErrorBody),
        (status=422,body=ErrorBody),(status=428,body=ErrorBody),(status=429,body=ErrorBody),
        (status=503,body=ErrorBody)))]
pub(crate) async fn start_test(
    State(s): State<AppState>,
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
    let test = if input.connector == "smtp" {
        // begin_test publishes the SMTP job in the same transaction.
        input.test
    } else {
        // Continue settling a test if the client times out or loses its connection.
        tokio::spawn(async move {
            let result = tokio::time::timeout(Duration::from_secs(13), probe(&s, &input))
                .await
                .unwrap_or((
                    Outcome::Unavailable,
                    TestEvidence::Adapter(AdapterResult::DeadlineExceeded),
                ));
            store::finish_test(&s.db, id, test_id, result).await
        })
        .await
        .map_err(|_| Error::Internal)??
    };
    let location = format!("/api/v1/integrations/{id}/tests/{test_id}");
    Ok((
        StatusCode::ACCEPTED,
        [(header::LOCATION, location)],
        Json(test),
    ))
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

#[utoipa::path(get, path="/api/v1/integrations/{id}/tests/{testId}",
    params(("id" = String, Path, format = "uuid"), ("testId" = String, Path, format = "uuid")),
    security(("adminBearer" = []), ("adminSession" = [])),
    responses((status=200,body=TestView),(status=401,body=ErrorBody),(status=403,body=ErrorBody),
        (status=404,body=ErrorBody),(status=422,body=ErrorBody),(status=503,body=ErrorBody)))]
pub(crate) async fn get_scoped_test(
    State(s): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Path((id, test_id)) = path.map_err(|_| ApiError::Invalid)?;
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::Invalid)?;
    let test_id = Uuid::parse_str(&test_id).map_err(|_| ApiError::Invalid)?;
    let test = store::get_test(&s.db, test_id).await?;
    if test.integration_id != id.to_string() {
        return Err(Error::Missing.into());
    }
    Ok(Json(test))
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Activate {
    #[schema(format = "uuid")]
    test_id: String,
}

#[utoipa::path(post, path="/api/v1/integrations/{id}/activate", request_body=Activate,
    params(("id" = String, Path, format = "uuid"), ("If-Match" = String, Header),
        ("Origin" = Option<String>, Header), ("X-CSRF-Token" = Option<String>, Header)),
    security(("adminBearer" = []), ("adminSession" = [])),
    responses((status=200,body=Integration,headers(("ETag" = String))),
        (status=401,body=ErrorBody),(status=403,body=ErrorBody),(status=404,body=ErrorBody),
        (status=409,body=ErrorBody),(status=413,body=ErrorBody),(status=422,body=ErrorBody),
        (status=428,body=ErrorBody),(status=429,body=ErrorBody),(status=503,body=ErrorBody)))]
pub(crate) async fn activate(
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<Activate>, JsonRejection>,
) -> Result<Response, ApiError> {
    let id = request_id(path)?;
    let revision = revision(&headers)?;
    let request = request_json(body)?;
    let test_id = Uuid::parse_str(&request.test_id).map_err(|_| ApiError::Invalid)?;
    let integration = store::activate(
        &s.db,
        id,
        revision,
        test_id,
        s.config.binance_probes_enabled,
    )
    .await?;
    Ok(integration_response(StatusCode::OK, integration))
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rollback {
    #[schema(minimum = 1)]
    version: i64,
}

#[utoipa::path(post, path="/api/v1/integrations/{id}/rollback", request_body=Rollback,
    params(("id" = String, Path, format = "uuid"), ("If-Match" = String, Header),
        ("Origin" = Option<String>, Header), ("X-CSRF-Token" = Option<String>, Header)),
    security(("adminBearer" = []), ("adminSession" = [])),
    responses((status=200,body=Integration,headers(("ETag" = String))),
        (status=401,body=ErrorBody),(status=403,body=ErrorBody),(status=404,body=ErrorBody),
        (status=409,body=ErrorBody),(status=413,body=ErrorBody),(status=422,body=ErrorBody),
        (status=428,body=ErrorBody),(status=429,body=ErrorBody),(status=503,body=ErrorBody)))]
pub(crate) async fn rollback(
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<Rollback>, JsonRejection>,
) -> Result<Response, ApiError> {
    let id = request_id(path)?;
    let revision = revision(&headers)?;
    let Rollback { version } = request_json(body)?;
    if version < 1 {
        return Err(ApiError::Invalid);
    }
    let integration = store::rollback(
        &s.db,
        id,
        revision,
        version,
        s.config.binance_probes_enabled,
    )
    .await?;
    Ok(integration_response(StatusCode::OK, integration))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn integration_request_schemas_reject_disallowed_fields_and_token_syntax() {
        let spec = serde_json::to_value(crate::openapi()).unwrap();
        let schemas = &spec["components"]["schemas"];
        let credential = &schemas["Credential"];
        assert_eq!(credential["additionalProperties"], false);
        assert_eq!(credential["required"], json!(["api_token"]));
        assert_eq!(
            credential["properties"]["api_token"],
            json!({
                "type": "string",
                "writeOnly": true,
                "minLength": 16,
                "maxLength": 4096,
                "pattern": r"^[A-Za-z0-9][A-Za-z0-9._~+-]*={0,2}(?![\s\S])"
            })
        );

        let variants = schemas["Update"]["oneOf"].as_array().unwrap();
        assert_eq!(variants.len(), 2);
        for (action, fields) in [
            ("configure", &["action", "settings"][..]),
            ("disable", &["action"][..]),
        ] {
            let variant = variants
                .iter()
                .find(|variant| variant["properties"]["action"]["enum"] == json!([action]))
                .unwrap();
            assert_eq!(variant["additionalProperties"], false, "{action}");
            let properties = variant["properties"].as_object().unwrap();
            assert_eq!(properties.len(), fields.len(), "{action}");
            let required = variant["required"].as_array().unwrap();
            assert_eq!(required.len(), fields.len(), "{action}");
            for field in fields {
                assert!(properties.contains_key(*field), "{action}.{field}");
                assert!(
                    required.iter().any(|name| name == *field),
                    "{action}.{field}"
                );
            }
        }
        assert_eq!(schemas["Settings"]["additionalProperties"], false);
    }

    #[test]
    fn integration_update_accepts_each_documented_action() {
        for (body, expected) in [
            (
                r#"{"action":"configure","settings":{"symbol":"BTCUSDT"}}"#,
                "configure",
            ),
            (r#"{"action":"disable"}"#, "disable"),
        ] {
            let request = serde_json::from_str::<Update>(body).unwrap();
            let actual = match request {
                Update::Configure { .. } => "configure",
                Update::Disable { .. } => "disable",
            };
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn integration_selectors_accept_catalog_symbols_or_configured_profile_names() {
        for (connector, payload, expected) in [
            (Connector::IbkrPaper, json!({"symbol":"SPY"}), true),
            (Connector::IbkrPaper, json!({"symbol":"BTC"}), false),
            (Connector::GoogleDrive, json!({"profile":"secondary"}), true),
            (
                Connector::GoogleDrive,
                json!({"profile":"actual_drive"}),
                false,
            ),
            (
                Connector::GoogleDrive,
                json!({"profile":"primary","path":"/tmp/other"}),
                false,
            ),
            (
                Connector::StrategyFile,
                json!({"profile":"primary","symbol":"SPY"}),
                false,
            ),
            (Connector::Smtp, json!({}), false),
        ] {
            let actual = serde_json::from_value::<Settings>(payload)
                .ok()
                .is_some_and(|settings| settings.validate(connector).is_ok());
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn credential_token_validator_enforces_schema_boundaries() {
        for (token, valid) in [
            ("a".repeat(15), false),
            ("a".repeat(16), true),
            ("9azAZ._~+-012345=".into(), true),
            ("9azAZ._~+-012345==".into(), true),
            (format!("{}==", "z".repeat(4094)), true),
            ("z".repeat(4097), false),
            (".0123456789abcdef".into(), false),
            ("a0123=456789abcdef".into(), false),
            ("a0123456789abcdef===".into(), false),
            ("a0123456789abcde/".into(), false),
            ("a0123456789abcdeé".into(), false),
            ("a0123456789abcdef\n".into(), false),
        ] {
            let credential =
                serde_json::from_value::<Credential>(json!({"api_token": token})).unwrap();
            assert_eq!(credential.validate().is_ok(), valid);
        }
    }
}
