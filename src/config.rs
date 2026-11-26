use serde::Deserialize;
use serde_yaml::Value;
use std::{
    error::Error,
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use url::Url;

pub type ConfigError = Box<dyn Error + Send + Sync>;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub server: Server,
    pub database: DatabaseConfig,
    pub finance: FinanceConfig,
    pub paths: Paths,
    pub security: SecurityConfig,
    #[serde(default)]
    pub credentials: CredentialConfig,
    #[serde(default)]
    pub binance_probes_enabled: bool,
    #[serde(default)]
    pub local_probes: crate::integrations::local_probe::LocalProbeConfig,
    #[serde(default)]
    pub ops: crate::ops::OpsConfig,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub bind_host: IpAddr,
    pub port: u16,
    pub request_timeout_ms: u64,
    pub max_body_bytes: usize,
    pub shutdown_timeout_ms: u64,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub url: String,
    pub connect_timeout_ms: u64,
    pub acquire_timeout_ms: u64,
    pub auto_migrate: bool,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinanceConfig {
    pub base_url: String,
    pub ready_timeout_ms: u64,
    pub required_for_ready: bool,
    pub connect_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub max_retries: u32,
    pub backoff_ms: u64,
    pub circuit_failure_threshold: u32,
    pub circuit_open_ms: u64,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    pub static_root: PathBuf,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityConfig {
    pub admin_token_file: PathBuf,
    pub max_requests: u32,
    pub window_ms: u64,
    pub public_origin: Option<String>,
    #[serde(default = "session_ttl_seconds")]
    pub session_ttl_seconds: u64,
}

fn session_ttl_seconds() -> u64 {
    900
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialConfig {
    pub directory: Option<PathBuf>,
    pub recipient_public_key_file: Option<PathBuf>,
}

fn overlay(base: &mut Value, layer: Value) {
    match (base, layer) {
        (Value::Mapping(base), Value::Mapping(update)) => {
            for (k, v) in update {
                overlay(base.entry(k).or_insert(Value::Null), v);
            }
        }
        (base, update) => *base = update,
    }
}

impl Settings {
    pub fn load(dir: &Path, get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let profile = get("FERTILE_PROFILE").unwrap_or_else(|| "cloudbox".into());
        if !matches!(profile.as_str(), "cloudbox" | "stable") {
            return Err("FERTILE_PROFILE must be cloudbox or stable".into());
        }
        let mut base: Value = serde_yaml::from_str(&fs::read_to_string(dir.join("base.yaml"))?)?;
        overlay(
            &mut base,
            serde_yaml::from_str(&fs::read_to_string(dir.join(format!("{profile}.yaml")))?)?,
        );
        let mut cfg: Self = serde_yaml::from_value(base)?;
        if let Some(s) = get("WEB_BIND_HOST") {
            cfg.server.bind_host = s.parse()?;
        }
        if let Some(s) = get("WEB_PORT") {
            cfg.server.port = s.parse()?;
        }
        if let Some(s) = get("WEB_STATIC_ROOT") {
            cfg.paths.static_root = PathBuf::from(s);
        }
        if let Some(s) = get("FINANCE_BASE_URL") {
            cfg.finance.base_url = s;
        }
        if let Some(s) = get("DATABASE_URL") {
            cfg.database.url = s;
        }
        if let Some(s) = get("WEB_AUTO_MIGRATE") {
            cfg.database.auto_migrate = s.parse()?;
        }
        if let Some(s) = get("WEB_REQUIRE_FINANCE") {
            cfg.finance.required_for_ready = s.parse()?;
        }
        if let Some(path) = get("WEB_ADMIN_TOKEN_FILE") {
            cfg.security.admin_token_file = PathBuf::from(path);
        }
        if let Some(origin) = get("WEB_PUBLIC_ORIGIN") {
            cfg.security.public_origin = Some(origin);
        }
        if let Some(seconds) = get("WEB_ADMIN_SESSION_TTL_SECONDS") {
            cfg.security.session_ttl_seconds = seconds.parse()?;
        }
        if let Some(path) = get("WEB_CREDENTIAL_DIR") {
            cfg.credentials.directory = Some(PathBuf::from(path));
        }
        if let Some(path) = get("WEB_CREDENTIAL_RECIPIENT_PUBLIC_KEY_FILE") {
            cfg.credentials.recipient_public_key_file = Some(PathBuf::from(path));
        }
        if let Some(value) = get("WEB_BINANCE_PROBES_ENABLED") {
            cfg.binance_probes_enabled = value
                .parse()
                .map_err(|_| "WEB_BINANCE_PROBES_ENABLED must be true or false")?;
        }
        cfg.local_probes.overlay(&get);
        cfg.ops.overlay(&get);
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let api = &self.server;
        if api.port == 0 || api.max_body_bytes == 0 || !self.paths.static_root.is_absolute() {
            return Err("port/body limit must be positive; static_root must be absolute".into());
        }
        if api.port == 8080 {
            return Err("port 8080 is reserved for the existing Ruler-managed website".into());
        }
        if !self.security.admin_token_file.is_absolute()
            || self.security.max_requests == 0
            || !(1..=86_400_000).contains(&self.security.window_ms)
        {
            return Err("security requires an absolute secret path, a positive request limit and a window between 1ms and one day".into());
        }
        crate::auth::validate_session_settings(
            self.security.public_origin.as_deref(),
            self.admin_session_ttl(),
        )?;
        match (
            self.credentials.directory.as_deref(),
            self.credentials.recipient_public_key_file.as_deref(),
        ) {
            (None, None) => {}
            (Some(dir), Some(key)) if dir.is_absolute() && key.is_absolute() => {}
            _ => return Err("both credential paths must be configured and absolute".into()),
        }
        self.local_probes.validate(&self.paths.static_root)?;
        self.ops.validate(&self.paths.static_root)?;
        for ms in [
            api.request_timeout_ms,
            api.shutdown_timeout_ms,
            self.database.connect_timeout_ms,
            self.database.acquire_timeout_ms,
            self.finance.ready_timeout_ms,
            self.finance.connect_timeout_ms,
            self.finance.request_timeout_ms,
            self.finance.backoff_ms,
            self.finance.circuit_open_ms,
        ] {
            if !(1..=300_000).contains(&ms) {
                return Err("timeouts must be between 1 ms and 5 minutes".into());
            }
        }
        if self.finance.max_retries > 2 || self.finance.circuit_failure_threshold == 0 {
            return Err(
                "finance max_retries must be 0..2 and circuit threshold must be positive".into(),
            );
        }
        let url = Url::parse(&self.finance.base_url)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("invalid FINANCE_BASE_URL".into());
        }
        let db = Url::parse(&self.database.url).map_err(|_| "invalid DATABASE_URL")?;
        if !matches!(db.scheme(), "postgres" | "postgresql") {
            return Err("DATABASE_URL must use postgres".into());
        }
        Ok(())
    }

    pub fn drain_timeout(&self) -> Duration {
        Duration::from_millis(self.server.shutdown_timeout_ms)
    }

    pub fn admin_session_ttl(&self) -> Duration {
        Duration::from_secs(self.security.session_ttl_seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_and_env_merge_then_validate() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("base.yaml"),
            include_str!("../config/base.yaml"),
        )
        .unwrap();
        fs::write(d.path().join("stable.yaml"), "server: { port: 8200 }").unwrap();
        let profile = |key: &str| (key == "FERTILE_PROFILE").then(|| "stable".into());
        assert_eq!(Settings::load(d.path(), profile).unwrap().server.port, 8200);
        let settings = Settings::load(d.path(), |key| match key {
            "WEB_PORT" => Some("8300".into()),
            "WEB_STATIC_ROOT" => Some("/alternate".into()),
            _ => profile(key),
        })
        .unwrap();
        assert_eq!(settings.server.port, 8300);
        assert_eq!(settings.paths.static_root, Path::new("/alternate"));
        assert!(!settings.binance_probes_enabled);
        for (key, value) in [
            ("WEB_PORT", "0"),
            ("WEB_PORT", "8080"),
            ("FINANCE_BASE_URL", "file:///config"),
            ("DATABASE_URL", "file:///data"),
            ("WEB_STATIC_ROOT", "relative"),
            ("FERTILE_PROFILE", "../escape"),
            ("WEB_PUBLIC_ORIGIN", "http://example.test"),
            ("WEB_ADMIN_SESSION_TTL_SECONDS", "901"),
            ("WEB_CREDENTIAL_DIR", "/tmp/only-one-path"),
            ("WEB_BINANCE_PROBES_ENABLED", "on"),
        ] {
            assert!(
                Settings::load(d.path(), |k| if k == key {
                    Some(value.into())
                } else {
                    profile(k)
                })
                .is_err()
            );
        }
        fs::write(d.path().join("stable.yaml"), "server: { bad_port: 123 }").unwrap();
        assert!(Settings::load(d.path(), profile).is_err());
    }
}
