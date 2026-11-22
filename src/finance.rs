use crate::config::{ConfigError, FinanceConfig};
use serde::Serialize;
use std::time::{Duration, Instant};
use utoipa::ToSchema;

#[derive(Clone, Serialize, ToSchema)]
pub struct FinanceStatus {
    pub ok: bool,
    pub latency_ms: u64,
}

#[derive(Clone)]
pub struct Finance {
    client: reqwest::Client,
    ready_url: String,
    timeout: Duration,
}
impl Finance {
    pub fn new(config: &FinanceConfig) -> Result<Self, ConfigError> {
        let timeout = Duration::from_millis(config.ready_timeout_ms);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(timeout)
            .build()?;
        Ok(Self {
            client,
            ready_url: format!("{}/healthz/ready", config.base_url.trim_end_matches('/')),
            timeout,
        })
    }
    pub async fn probe(&self) -> FinanceStatus {
        let begin = Instant::now();
        let ok = self
            .client
            .get(&self.ready_url)
            .timeout(self.timeout)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
        FinanceStatus {
            ok,
            latency_ms: begin.elapsed().as_millis() as u64,
        }
    }
}
