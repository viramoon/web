use crate::{
    ErrorBody, ErrorDetail, ErrorLocation,
    circuit::Circuit,
    config::{ConfigError, FinanceConfig},
};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use finance_client::{
    apis::{
        Error,
        configuration::Configuration,
        default_api::{self, ReadyError},
    },
    models,
};
use serde::Serialize;
use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};
use utoipa::ToSchema;

#[derive(Clone, Serialize, ToSchema)]
pub struct FinanceStatus {
    pub ok: bool,
    pub latency_ms: u64,
    pub circuit: String,
}

pub struct Fault {
    pub status: StatusCode,
    detail: ErrorDetail,
    transient: bool,
}

impl Fault {
    fn new(status: StatusCode, code: &str, message: &str, transient: bool) -> Self {
        Self {
            status,
            detail: ErrorDetail::new(code, message),
            transient,
        }
    }

    fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "finance_unavailable",
            "Finance is unavailable",
            true,
        )
    }

    fn timeout() -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "finance_timeout",
            "Finance request timed out",
            true,
        )
    }

    pub(crate) fn invalid_response() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "finance_invalid_response",
            "Invalid finance response",
            true,
        )
    }

    pub(crate) fn stream_transport(error: reqwest::Error) -> Self {
        Self::upstream(Error::<()>::Reqwest(error))
    }

    pub(crate) fn stream_status(status: StatusCode) -> Self {
        match status {
            StatusCode::BAD_REQUEST => Self::new(
                status,
                "finance_request_rejected",
                "Finance rejected the stream request",
                false,
            ),
            StatusCode::SERVICE_UNAVAILABLE => Self::unavailable(),
            StatusCode::GATEWAY_TIMEOUT => Self::timeout(),
            _ => Self::invalid_response(),
        }
    }

    fn upstream<E>(err: Error<E>) -> Self {
        match err {
            Error::Reqwest(e) if e.is_timeout() => Self::timeout(),
            Error::Reqwest(e) if e.is_connect() => Self::unavailable(),
            Error::ResponseError(r) if r.status.is_client_error() => {
                let parsed = serde_json::from_str::<models::ErrorBody>(&r.content).ok();
                if let Some(error) = parsed {
                    let error = error.error;
                    Self {
                        status: r.status,
                        detail: ErrorDetail {
                            code: error.code,
                            message: error.message,
                            path: error.path.flatten(),
                            location: error.location.flatten().map(|location| ErrorLocation {
                                line: location.line,
                                column: location.column,
                            }),
                        },
                        transient: false,
                    }
                } else {
                    Self::new(
                        r.status,
                        "finance_request_rejected",
                        "Finance rejected the request",
                        false,
                    )
                }
            }
            Error::ResponseError(r) if r.status == StatusCode::SERVICE_UNAVAILABLE => {
                Self::unavailable()
            }
            Error::ResponseError(r) if r.status == StatusCode::GATEWAY_TIMEOUT => Self::timeout(),
            Error::Serde(_) => Self::invalid_response(),
            _ => Self::new(
                StatusCode::BAD_GATEWAY,
                "finance_upstream_error",
                "Finance request failed",
                true,
            ),
        }
    }

    pub fn write_error(self) -> Self {
        self.with_timeout_hint(
            "Submission outcome unknown; query by Idempotency-Key before retrying",
        )
    }

    pub(crate) fn with_timeout_hint(mut self, hint: &str) -> Self {
        if self.status == StatusCode::GATEWAY_TIMEOUT {
            self.detail.message = hint.into();
        }
        self
    }
}

impl IntoResponse for Fault {
    fn into_response(self) -> Response {
        (self.status, Json(ErrorBody { error: self.detail })).into_response()
    }
}

#[derive(Clone)]
pub struct Finance {
    connection: Arc<Configuration>,
    stream_connection: Arc<Configuration>,
    probe_client: Option<reqwest::Client>,
    policy: FinanceConfig,
    circuit: Circuit,
}

impl Finance {
    pub fn new(config: &FinanceConfig) -> Result<Self, ConfigError> {
        Self::build(config, |builder| builder)
    }

    #[cfg(test)]
    pub(crate) fn new_for_unix_socket(
        config: &FinanceConfig,
        path: &std::path::Path,
    ) -> Result<Self, ConfigError> {
        Self::build(config, |builder| builder.unix_socket(path))
    }

    fn build(
        config: &FinanceConfig,
        configure: impl Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
    ) -> Result<Self, ConfigError> {
        let local = loopback_endpoint(&config.base_url);
        let builder = || {
            let builder = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .connect_timeout(Duration::from_millis(config.connect_timeout_ms));
            configure(if local { builder.no_proxy() } else { builder })
        };
        let client = builder()
            .timeout(Duration::from_millis(config.request_timeout_ms))
            .build()?;
        let probe_client = local.then(|| client.clone());
        let connection = Configuration {
            base_path: config.base_url.trim_end_matches('/').into(),
            client,
            user_agent: Some(concat!("web/", env!("CARGO_PKG_VERSION")).into()),
            basic_auth: None,
            oauth_access_token: None,
            bearer_access_token: None,
            api_key: None,
        };
        let stream_connection = Configuration {
            client: builder().build()?,
            ..connection.clone()
        };
        Ok(Self {
            connection: Arc::new(connection),
            stream_connection: Arc::new(stream_connection),
            probe_client,
            policy: config.clone(),
            circuit: Circuit::new(
                config.circuit_failure_threshold,
                Duration::from_millis(config.circuit_open_ms),
            ),
        })
    }

    pub(crate) fn integration_probe_client(&self) -> Option<(&reqwest::Client, &str)> {
        Some((self.probe_client.as_ref()?, &self.connection.base_path))
    }

    /// Apply the finance request deadline to stream establishment only.
    pub(crate) async fn open_stream<T>(
        &self,
        open: impl AsyncFnOnce(&Configuration) -> Result<T, Fault>,
    ) -> Result<T, Fault> {
        let permit = self.circuit.admit().ok_or_else(Fault::unavailable)?;
        let deadline = Duration::from_millis(self.policy.request_timeout_ms);
        let result = tokio::time::timeout(deadline, open(&self.stream_connection))
            .await
            .unwrap_or_else(|_| Err(Fault::timeout()));
        permit.finish(!result.as_ref().is_err_and(|fault| fault.transient));
        result
    }

    /// Idempotent reads may back off and retry. Mutations always make at most one attempt.
    pub async fn read<T, E, F, Fut>(&self, call: F) -> Result<T, Fault>
    where
        F: Fn(Arc<Configuration>) -> Fut,
        Fut: Future<Output = Result<T, Error<E>>>,
    {
        self.execute(
            call,
            self.policy.max_retries,
            self.policy.request_timeout_ms,
        )
        .await
    }

    pub async fn write<T, E, F, Fut>(&self, call: F) -> Result<T, Fault>
    where
        F: Fn(Arc<Configuration>) -> Fut,
        Fut: Future<Output = Result<T, Error<E>>>,
    {
        self.post_once(call).await.map_err(Fault::write_error)
    }

    pub(crate) async fn post_once<T, E, F, Fut>(&self, call: F) -> Result<T, Fault>
    where
        F: Fn(Arc<Configuration>) -> Fut,
        Fut: Future<Output = Result<T, Error<E>>>,
    {
        self.execute(call, 0, self.policy.request_timeout_ms).await
    }

    async fn execute<T, E, F, Fut>(
        &self,
        call: F,
        retries: u32,
        timeout_ms: u64,
    ) -> Result<T, Fault>
    where
        F: Fn(Arc<Configuration>) -> Fut,
        Fut: Future<Output = Result<T, Error<E>>>,
    {
        let permit = self.circuit.admit().ok_or_else(Fault::unavailable)?;
        let retry_limit = if permit.is_trial { 0 } else { retries };
        let mut attempt = 0;
        loop {
            if !permit.active() {
                return Err(Fault::unavailable());
            }
            let request = call(Arc::clone(&self.connection));
            let result =
                match tokio::time::timeout(Duration::from_millis(timeout_ms), request).await {
                    Ok(result) => result.map_err(Fault::upstream),
                    Err(_) => Err(Fault::timeout()),
                };
            let transient = result.as_ref().is_err_and(|error| error.transient);
            if !transient || attempt == retry_limit {
                permit.finish(!transient);
                return result;
            }
            let cap = self.policy.backoff_ms * (1 << attempt);
            let wait = cap / 2 + fastrand::u64(0..=cap - cap / 2);
            attempt += 1;
            tracing::debug!(attempt, delay_ms = wait, "retrying finance GET");
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
    }

    pub fn circuit_state(&self) -> &'static str {
        self.circuit.label()
    }

    pub async fn probe(&self) -> (FinanceStatus, bool) {
        let start = Instant::now();
        // The health budget is independent of normal GET retries.
        let response = self
            .execute(
                |cfg| async move { classify_readiness(default_api::ready(&cfg).await) },
                0,
                self.policy.ready_timeout_ms,
            )
            .await;
        let reachable = response.is_ok();
        (
            FinanceStatus {
                ok: response.unwrap_or(false),
                latency_ms: start.elapsed().as_millis() as u64,
                circuit: self.circuit_state().into(),
            },
            reachable,
        )
    }
}

fn classify_readiness(
    response: Result<models::Health, Error<ReadyError>>,
) -> Result<bool, Error<ReadyError>> {
    match response {
        Ok(_) => Ok(true),
        // The generated error enum matches the body without checking the HTTP status.
        Err(Error::ResponseError(r))
            if r.status == StatusCode::SERVICE_UNAVAILABLE
                && matches!(r.entity, Some(ReadyError::Status503(_))) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn loopback_endpoint(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && match url.host() {
                Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                _ => false,
            }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use finance_client::apis::ResponseContent;
    use serde_json::json;

    #[tokio::test]
    async fn loopback_finance_client_ignores_environment_proxy() {
        const TEST: &str = "finance::tests::loopback_finance_client_ignores_environment_proxy";
        const DIRECT_PORT: &str = "WEB_TEST_FINANCE_DIRECT_PORT";
        if let Ok(port) = std::env::var(DIRECT_PORT) {
            let port: u16 = port.parse().unwrap();
            let base_url = format!("http://127.0.0.1:{port}");
            let mut config =
                crate::config::Settings::load(std::path::Path::new("config"), |_| None)
                    .unwrap()
                    .finance;
            config.base_url = base_url.clone();
            // Check that a client using environment proxies would hit the local proxy.
            let through_proxy = reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap()
                .get(format!("{base_url}/healthz/ready"))
                .send()
                .await
                .unwrap();
            assert_eq!(through_proxy.status(), StatusCode::BAD_GATEWAY);

            let (status, reachable) = Finance::new(&config).unwrap().probe().await;
            assert!(reachable && status.ok);
            return;
        }

        let direct = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let direct_port = direct.local_addr().unwrap().port();
        let origin_task = tokio::spawn(async {
            let routes = axum::Router::new().route(
                "/healthz/ready",
                axum::routing::get(|| async { Json(json!({"status":"ok","checks":[]})) }),
            );
            axum::serve(direct, routes).await.unwrap();
        });
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
        let proxy_task = tokio::spawn(async {
            let routes = axum::Router::new().fallback(|| async { StatusCode::BAD_GATEWAY });
            axum::serve(proxy, routes).await.unwrap();
        });
        let result = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST])
            .env_clear()
            .env(DIRECT_PORT, direct_port.to_string())
            .env("http_proxy", &proxy_url)
            .env("HTTP_PROXY", &proxy_url)
            .output()
            .await
            .unwrap();
        origin_task.abort();
        proxy_task.abort();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
    }

    #[tokio::test]
    async fn strategy_diagnostics_keep_redacted_path_and_source_coordinates() {
        let diagnostic = json!({"error": {
            "code": "UNKNOWN_FIELD", "message": "Unknown field", "path": "/spec/<key>",
            "location": {"line": 3, "column": 5}
        }});
        let response = Fault::upstream(Error::<()>::ResponseError(ResponseContent {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            content: diagnostic.to_string(),
            entity: None,
        }))
        .into_response();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            diagnostic
        );
    }

    #[test]
    fn finance_readiness_requires_http_503_to_accept_a_not_ready_health_body() {
        let health = models::Health::new(Vec::new(), "degraded".into());
        let body = serde_json::to_string(&health).unwrap();
        let upstream_error = |status, content: &str| {
            Err(Error::ResponseError(ResponseContent {
                status,
                content: content.into(),
                entity: serde_json::from_str(content).ok(),
            }))
        };

        assert!(matches!(classify_readiness(Ok(health)), Ok(true)));
        assert!(classify_readiness(upstream_error(StatusCode::BAD_GATEWAY, &body)).is_err());
        assert!(matches!(
            classify_readiness(upstream_error(StatusCode::SERVICE_UNAVAILABLE, &body)),
            Ok(false)
        ));
        assert!(classify_readiness(upstream_error(StatusCode::SERVICE_UNAVAILABLE, "{}")).is_err());
    }
}
