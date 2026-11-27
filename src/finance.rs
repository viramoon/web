use crate::{
    api_error,
    circuit::Circuit,
    config::{ConfigError, FinanceConfig},
};
use axum::{
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
    code: String,
    message: String,
    transient: bool,
}

impl Fault {
    fn new(status: StatusCode, code: &str, message: &str, transient: bool) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
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

    fn upstream<E>(err: Error<E>) -> Self {
        match err {
            Error::Reqwest(e) if e.is_timeout() => Self::timeout(),
            Error::Reqwest(e) if e.is_connect() => Self::unavailable(),
            Error::ResponseError(r) if r.status.is_client_error() => {
                let parsed = serde_json::from_str::<models::ErrorBody>(&r.content).ok();
                if let Some(error) = parsed {
                    Self::new(r.status, &error.error.code, &error.error.message, false)
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
            Error::Serde(_) => Self::new(
                StatusCode::BAD_GATEWAY,
                "finance_invalid_response",
                "Invalid finance response",
                true,
            ),
            _ => Self::new(
                StatusCode::BAD_GATEWAY,
                "finance_upstream_error",
                "Finance request failed",
                true,
            ),
        }
    }

    pub fn write_error(self) -> Self {
        if self.status == StatusCode::GATEWAY_TIMEOUT {
            return Self {
                message: "Submission outcome unknown; query by Idempotency-Key before retrying"
                    .into(),
                ..self
            };
        }
        self
    }
}

impl IntoResponse for Fault {
    fn into_response(self) -> Response {
        api_error(self.status, &self.code, &self.message)
    }
}

#[derive(Clone)]
pub struct Finance {
    connection: Arc<Configuration>,
    probe_client: Option<reqwest::Client>,
    policy: FinanceConfig,
    circuit: Circuit,
}

impl Finance {
    pub fn new(config: &FinanceConfig) -> Result<Self, ConfigError> {
        let client_builder = || {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
                .timeout(Duration::from_millis(config.request_timeout_ms))
        };
        let connection = Configuration {
            base_path: config.base_url.trim_end_matches('/').into(),
            client: client_builder().build()?,
            user_agent: Some(concat!("web/", env!("CARGO_PKG_VERSION")).into()),
            basic_auth: None,
            oauth_access_token: None,
            bearer_access_token: None,
            api_key: None,
        };
        let probe_client = loopback_endpoint(&config.base_url)
            .then(|| client_builder().no_proxy().build())
            .transpose()?;
        Ok(Self {
            connection: Arc::new(connection),
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
        self.execute(call, 0, self.policy.request_timeout_ms)
            .await
            .map_err(Fault::write_error)
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
    use finance_client::apis::ResponseContent;

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
