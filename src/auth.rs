//! A single locally provisioned administrator, with optional browser sessions.
use crate::{AppState, ErrorBody, api_error, config::ConfigError};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs::OpenOptions,
    io::{ErrorKind, Read},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use url::Url;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Clone)]
pub struct WriteAccess {
    bearer: Arc<Policy>,
    sessions: Option<Arc<Sessions>>,
}

struct Policy {
    digest: Option<[u8; 32]>,
    limit: usize,
    window: Duration,
    accepted: Mutex<VecDeque<Instant>>,
    market_reads: Mutex<VecDeque<Instant>>,
}

pub const SESSION_COOKIE_NAME: &str = "__Host-web_admin_session";
const MAX_SESSIONS: usize = 64;
const CSRF_HEADER: header::HeaderName = header::HeaderName::from_static("x-csrf-token");
const FETCH_SITE: header::HeaderName = header::HeaderName::from_static("sec-fetch-site");

struct Sessions {
    origin: Option<HeaderValue>,
    ttl: Duration,
    entries: Mutex<VecDeque<Session>>,
}

struct Session {
    token_digest: [u8; 32],
    csrf_token: Arc<str>,
    expires_at: Instant,
}

enum Identity {
    Bearer,
    Session(Arc<str>),
}

#[derive(Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Product {
    System,
    Backtests,
    MarketStatus,
    BacktestSubmission,
    Integrations,
    RestrictedMarketData,
    Notifications,
}

impl Product {
    pub(crate) const ALL: [Self; 7] = [
        Self::System,
        Self::Backtests,
        Self::MarketStatus,
        Self::BacktestSubmission,
        Self::Integrations,
        Self::RestrictedMarketData,
        Self::Notifications,
    ];

    fn readable_by(self, subject: Subject) -> bool {
        match self {
            Self::System | Self::Backtests | Self::MarketStatus => true,
            Self::BacktestSubmission
            | Self::Integrations
            | Self::RestrictedMarketData
            | Self::Notifications => matches!(subject, Subject::Admin),
        }
    }
}

#[derive(Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum Subject {
    Anonymous,
    Admin,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct Capabilities {
    subject: Subject,
    products: Vec<Product>,
    #[serde(skip_serializing_if = "Option::is_none")]
    csrf_token: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SessionCreated {
    csrf_token: String,
    /// Remaining lifetime of the new session in seconds.
    expires_in: u64,
}

fn session_origin(origin: Option<&str>, ttl: Duration) -> Result<Option<HeaderValue>, ConfigError> {
    if !(1..=900).contains(&ttl.as_secs()) || ttl.subsec_nanos() != 0 {
        return Err("session lifetime must be between 1 and 900 whole seconds".into());
    }
    origin
        .map(|origin| {
            let url = Url::parse(origin).map_err(|_| "invalid administrator session origin")?;
            if url.scheme() != "https" || url.origin().ascii_serialization() != origin {
                return Err("administrator session origin must be a canonical HTTPS origin".into());
            }
            Ok(HeaderValue::from_str(origin)?)
        })
        .transpose()
}

pub fn validate_session_settings(origin: Option<&str>, ttl: Duration) -> Result<(), ConfigError> {
    session_origin(origin, ttl).map(|_| ())
}

fn unique_header(headers: &HeaderMap, name: header::HeaderName) -> Option<&HeaderValue> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn same_origin_fetch(headers: &HeaderMap) -> bool {
    unique_header(headers, FETCH_SITE).is_some_and(|site| site == "same-origin")
}

pub(crate) fn stream_auth_headers(headers: &HeaderMap) -> HeaderMap {
    let mut retained = HeaderMap::new();
    for name in [
        header::AUTHORIZATION,
        header::COOKIE,
        header::HOST,
        header::ORIGIN,
        FETCH_SITE,
    ] {
        for value in headers.get_all(&name) {
            let mut value = value.clone();
            value.set_sensitive(true);
            retained.append(name.clone(), value);
        }
    }
    retained
}

fn check_loopback_origin(headers: &HeaderMap, require_origin: bool) -> Result<(), WriteDenied> {
    let authority = unique_header(headers, header::HOST)
        .and_then(|host| host.to_str().ok())
        .ok_or(WriteDenied::OriginRejected)?;
    // Compare the wire form so URL normalization cannot turn an alias into an allowed host.
    let (host, port) = authority
        .split_once(':')
        .ok_or(WriteDenied::OriginRejected)?;
    if !matches!(host, "127.0.0.1" | "localhost")
        || port.starts_with('0')
        || !port.bytes().all(|byte| byte.is_ascii_digit())
        || port.parse::<u16>().is_err()
    {
        return Err(WriteDenied::OriginRejected);
    }
    let same_origin_fetch = same_origin_fetch(headers);
    let allowed = if require_origin || headers.contains_key(header::ORIGIN) {
        unique_header(headers, header::ORIGIN)
            .and_then(|origin| origin.to_str().ok())
            .and_then(|origin| origin.strip_prefix("http://"))
            == Some(authority)
    } else {
        same_origin_fetch
    };
    if !allowed || (headers.contains_key(&FETCH_SITE) && !same_origin_fetch) {
        return Err(WriteDenied::OriginRejected);
    }
    Ok(())
}

fn cookie_digest(headers: &HeaderMap) -> Option<[u8; 32]> {
    let mut token = None;
    for value in headers.get_all(header::COOKIE) {
        for cookie in value.to_str().ok()?.split(';') {
            let Some((name, value)) = cookie.trim().split_once('=') else {
                continue;
            };
            if name == SESSION_COOKIE_NAME {
                if token.is_some()
                    || value.len() != 64
                    || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return None;
                }
                token = Some(value);
            }
        }
    }
    token.map(|value| Sha256::digest(value.as_bytes()).into())
}

fn random_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn session_cookie(token: &str, max_age: u64) -> HeaderValue {
    let mut cookie = HeaderValue::from_str(&format!(
        "{SESSION_COOKIE_NAME}={token}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Strict"
    ))
    .expect("session cookie uses only ASCII");
    cookie.set_sensitive(true);
    cookie
}

impl Sessions {
    fn check_origin(&self, headers: &HeaderMap) -> Result<(), WriteDenied> {
        match &self.origin {
            Some(origin) => check_configured_origin(headers, origin),
            None => check_loopback_origin(headers, true),
        }
    }

    fn read(&self, digest: &[u8; 32]) -> Result<Arc<str>, WriteDenied> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .iter()
            .find(|entry| {
                bool::from(entry.token_digest.ct_eq(digest)) && Instant::now() < entry.expires_at
            })
            .map(|entry| Arc::clone(&entry.csrf_token))
            .ok_or(WriteDenied::SessionUnauthorized)
    }

    fn check_csrf(&self, headers: &HeaderMap, expected: &str) -> Result<(), WriteDenied> {
        self.check_origin(headers)?;
        if !unique_header(headers, CSRF_HEADER)
            .is_some_and(|value| bool::from(expected.as_bytes().ct_eq(value.as_bytes())))
        {
            return Err(WriteDenied::CsrfRejected);
        }
        Ok(())
    }

    fn create(&self, previous: Option<[u8; 32]>) -> (String, SessionCreated) {
        let token = random_token();
        let csrf_token = random_token();
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        entries.retain(|entry| {
            now < entry.expires_at
                && !previous.is_some_and(|previous| bool::from(entry.token_digest.ct_eq(&previous)))
        });
        if entries.len() == MAX_SESSIONS {
            entries.pop_front();
        }
        entries.push_back(Session {
            token_digest: Sha256::digest(token.as_bytes()).into(),
            csrf_token: Arc::from(csrf_token.as_str()),
            expires_at: now + self.ttl,
        });
        (
            token,
            SessionCreated {
                csrf_token,
                expires_in: self.ttl.as_secs(),
            },
        )
    }

    fn verify_deletion(&self, headers: &HeaderMap) -> Result<[u8; 32], WriteDenied> {
        let digest = cookie_digest(headers).ok_or(WriteDenied::SessionUnauthorized)?;
        let csrf = self.read(&digest)?;
        self.check_csrf(headers, &csrf)?;
        Ok(digest)
    }

    fn delete(&self, headers: &HeaderMap) -> Result<(), WriteDenied> {
        let digest = self.verify_deletion(headers)?;
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|entry| !bool::from(entry.token_digest.ct_eq(&digest)));
        Ok(())
    }
}

fn check_configured_origin(headers: &HeaderMap, origin: &HeaderValue) -> Result<(), WriteDenied> {
    if unique_header(headers, header::ORIGIN) != Some(origin) {
        return Err(WriteDenied::OriginRejected);
    }
    Ok(())
}

fn load_digest(path: &Path) -> Result<Option<[u8; 32]>, ConfigError> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    // Validate the already-open descriptor. A pathname replacement cannot make
    // us validate one file's permissions and then consume another file's secret.
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o7777 != 0o600 {
        return Err("web admin secret must be a regular file with mode 0600".into());
    }
    let mut input = String::new();
    const MAX_FILE: u64 = 8192;
    file.take(MAX_FILE + 1).read_to_string(&mut input)?;
    if input.len() as u64 > MAX_FILE {
        return Err("web admin secret file is too large".into());
    }
    let mut assignments = input.lines().filter(|line| {
        let trimmed = line.trim_start();
        !trimmed.is_empty() && !trimmed.starts_with('#')
    });
    let token = assignments
        .next()
        .and_then(|line| line.strip_prefix("WEB_ADMIN_TOKEN="))
        .ok_or("expected a WEB_ADMIN_TOKEN assignment")?;
    // RFC 6750 b64token. Accept common hex, base64 and URL-safe random tokens.
    let unpadded = token.trim_end_matches('=');
    if assignments.next().is_some()
        || unpadded.is_empty()
        || !unpadded
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-._~+/".contains(&c))
    {
        return Err("invalid web admin secret format".into());
    }
    Ok(Some(Sha256::digest(token.as_bytes()).into()))
}

pub enum WriteDenied {
    Disabled,
    Unauthorized,
    SessionDisabled,
    SessionUnauthorized,
    OriginRejected,
    CsrfRejected,
    RateLimited(u64),
    MarketReadRateLimited(u64),
}
impl IntoResponse for WriteDenied {
    fn into_response(self) -> Response {
        match self {
            Self::Disabled => api_error(
                StatusCode::FORBIDDEN,
                "writes_disabled",
                "Writes are disabled",
            ),
            Self::Unauthorized => {
                let mut response = api_error(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    "A valid bearer credential is required",
                );
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                response
            }
            Self::SessionDisabled => api_error(
                StatusCode::FORBIDDEN,
                "sessions_disabled",
                "Administrator sessions are disabled",
            ),
            Self::SessionUnauthorized => api_error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "A valid administrator session is required",
            ),
            Self::OriginRejected => api_error(
                StatusCode::FORBIDDEN,
                "origin_rejected",
                "Request origin is not allowed",
            ),
            Self::CsrfRejected => api_error(
                StatusCode::FORBIDDEN,
                "csrf_rejected",
                "A valid CSRF token is required",
            ),
            Self::RateLimited(seconds) | Self::MarketReadRateLimited(seconds) => {
                let message = if matches!(self, Self::MarketReadRateLimited(_)) {
                    "Market read rate limit reached"
                } else {
                    "Write rate limit reached"
                };
                let mut response =
                    api_error(StatusCode::TOO_MANY_REQUESTS, "rate_limited", message);
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
                response
            }
        }
    }
}
impl WriteAccess {
    pub fn load(path: &Path, max_requests: u32, window: Duration) -> Result<Self, ConfigError> {
        if max_requests == 0 || window.is_zero() {
            return Err("write rate limit and window must be positive".into());
        }
        Ok(Self {
            bearer: Arc::new(Policy {
                digest: load_digest(path)?,
                limit: max_requests as usize,
                window,
                accepted: Mutex::new(VecDeque::new()),
                market_reads: Mutex::new(VecDeque::new()),
            }),
            sessions: None,
        })
    }

    pub fn with_sessions(
        mut self,
        origin: Option<&str>,
        ttl: Duration,
    ) -> Result<Self, ConfigError> {
        self.sessions = Some(Arc::new(Sessions {
            origin: session_origin(origin, ttl)?,
            ttl,
            entries: Mutex::new(VecDeque::new()),
        }));
        Ok(self)
    }

    fn authenticate_bearer(&self, headers: &HeaderMap) -> Result<(), WriteDenied> {
        let Some(expected) = &self.bearer.digest else {
            return Err(WriteDenied::Disabled);
        };
        let token = unique_header(headers, header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
            .map(|(_, token)| token.trim_start_matches(' '));
        if !token.is_some_and(|token| {
            let candidate: [u8; 32] = Sha256::digest(token.as_bytes()).into();
            bool::from(expected.ct_eq(&candidate))
        }) {
            return Err(WriteDenied::Unauthorized);
        }
        Ok(())
    }

    fn identity(&self, headers: &HeaderMap) -> Result<Identity, WriteDenied> {
        if !headers.contains_key(header::AUTHORIZATION)
            && let (Some(sessions), Some(digest)) = (&self.sessions, cookie_digest(headers))
        {
            return sessions.read(&digest).map(Identity::Session);
        }
        self.authenticate_bearer(headers)?;
        Ok(Identity::Bearer)
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Result<(), WriteDenied> {
        self.read_identity(headers).map(|_| ())
    }

    fn read_identity(&self, headers: &HeaderMap) -> Result<Identity, WriteDenied> {
        let identity = self.identity(headers)?;
        if matches!(identity, Identity::Session(_)) && self.session_policy()?.origin.is_none() {
            check_loopback_origin(headers, false)?;
        }
        Ok(identity)
    }

    pub fn check_read(&self, headers: &HeaderMap, product: Product) -> Result<(), WriteDenied> {
        if product.readable_by(Subject::Anonymous) {
            return Ok(());
        }
        self.authenticate(headers)
    }

    pub(crate) fn authorize_stream(
        &self,
        headers: &HeaderMap,
        require_session_origin: bool,
    ) -> Result<(), WriteDenied> {
        match self.identity(headers)? {
            Identity::Session(_) => {
                let sessions = self.session_policy()?;
                if sessions.origin.is_none() {
                    return check_loopback_origin(headers, require_session_origin);
                }
                if headers.contains_key(&FETCH_SITE) && !same_origin_fetch(headers) {
                    return Err(WriteDenied::OriginRejected);
                }
                if require_session_origin || headers.contains_key(header::ORIGIN) {
                    sessions.check_origin(headers)?;
                }
            }
            Identity::Bearer if headers.contains_key(header::ORIGIN) => {
                self.check_bearer_origin(headers)?;
            }
            Identity::Bearer => {}
        }
        Ok(())
    }

    pub(crate) fn authorize_market_read(&self, headers: &HeaderMap) -> Result<(), WriteDenied> {
        self.authorize_stream(headers, false)?;
        consume_quota(&self.bearer.market_reads, 120, Duration::from_secs(60))
            .map_err(WriteDenied::MarketReadRateLimited)
    }

    pub fn authorize(&self, headers: &HeaderMap) -> Result<(), WriteDenied> {
        self.authorize_write(headers, false)
    }

    pub fn authorize_same_origin(&self, headers: &HeaderMap) -> Result<(), WriteDenied> {
        self.authorize_write(headers, true)
    }

    pub(crate) fn check_session_creation(&self, headers: &HeaderMap) -> Result<(), WriteDenied> {
        self.authenticate_bearer(headers)?;
        self.session_policy()?.check_origin(headers)
    }

    pub(crate) fn check_session_deletion(&self, headers: &HeaderMap) -> Result<(), WriteDenied> {
        self.session_policy()?.verify_deletion(headers).map(|_| ())
    }

    fn authorize_write(
        &self,
        headers: &HeaderMap,
        check_bearer_origin: bool,
    ) -> Result<(), WriteDenied> {
        match self.identity(headers)? {
            Identity::Session(csrf) => self.session_policy()?.check_csrf(headers, &csrf)?,
            Identity::Bearer if check_bearer_origin && headers.contains_key(header::ORIGIN) => {
                self.check_bearer_origin(headers)?;
            }
            Identity::Bearer => {}
        }
        self.consume_write_quota()
    }

    fn check_bearer_origin(&self, headers: &HeaderMap) -> Result<(), WriteDenied> {
        let origin = self
            .session_policy()?
            .origin
            .as_ref()
            .ok_or(WriteDenied::SessionDisabled)?;
        check_configured_origin(headers, origin)
    }

    fn session_policy(&self) -> Result<&Sessions, WriteDenied> {
        self.sessions.as_deref().ok_or(WriteDenied::SessionDisabled)
    }

    fn consume_write_quota(&self) -> Result<(), WriteDenied> {
        let policy = &self.bearer;
        consume_quota(&policy.accepted, policy.limit, policy.window)
            .map_err(WriteDenied::RateLimited)
    }
}

fn consume_quota(
    accepted: &Mutex<VecDeque<Instant>>,
    limit: usize,
    window: Duration,
) -> Result<(), u64> {
    // All authorized requests share the single v1 administrator's quotas.
    let mut accepted = accepted.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    while accepted
        .front()
        .is_some_and(|time| now.duration_since(*time) >= window)
    {
        accepted.pop_front();
    }
    if accepted.len() >= limit {
        let remaining = window.saturating_sub(now.duration_since(accepted[0]));
        let seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0);
        return Err(seconds.max(1));
    }
    accepted.push_back(now);
    Ok(())
}

#[utoipa::path(post, path = "/api/v1/admin/session", security(("adminBearer" = [])),
    params(("Origin" = String, Header, description="Configured HTTPS origin, or http://127.0.0.1:<port> or http://localhost:<port> matching Host when unset")),
    responses((status=201, body=SessionCreated,
        headers(("Set-Cookie" = String, description="HttpOnly, Secure, SameSite=Strict session cookie"))),
        (status=401, body=ErrorBody), (status=403, body=ErrorBody), (status=413, body=ErrorBody),
        (status=429, body=ErrorBody)))]
pub(crate) async fn create_session(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, WriteDenied> {
    let access = &s.write_access;
    access.check_session_creation(&headers)?;
    let sessions = access.session_policy()?;
    access.consume_write_quota()?;
    let (token, created) = sessions.create(cookie_digest(&headers));
    let cookie = session_cookie(&token, created.expires_in);
    Ok((
        StatusCode::CREATED,
        [(header::SET_COOKIE, cookie)],
        Json(created),
    )
        .into_response())
}

#[utoipa::path(delete, path = "/api/v1/admin/session", security(("adminSession" = [])),
    params(("Origin" = String, Header, description="Configured HTTPS origin, or http://127.0.0.1:<port> or http://localhost:<port> matching Host when unset"),
        ("X-CSRF-Token" = String, Header, description="Token for the administrator session")),
    responses((status=204, headers(("Set-Cookie" = String, description="Expires the session cookie"))),
        (status=401, body=ErrorBody), (status=403, body=ErrorBody), (status=413, body=ErrorBody)))]
pub(crate) async fn delete_session(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, WriteDenied> {
    s.write_access.session_policy()?.delete(&headers)?;
    Ok((
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, session_cookie("", 0))],
    )
        .into_response())
}

#[utoipa::path(get, path = "/api/v1/me/capabilities",
    security((), ("adminBearer" = []), ("adminSession" = [])),
    responses((status=200, body=Capabilities)))]
pub(crate) async fn capabilities(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Json<Capabilities> {
    let (subject, csrf_token) = match s.write_access.read_identity(&headers) {
        Ok(Identity::Session(token)) => (Subject::Admin, Some(token.to_string())),
        Ok(Identity::Bearer) => (Subject::Admin, None),
        Err(_) => (Subject::Anonymous, None),
    };
    Json(Capabilities {
        subject,
        csrf_token,
        products: Product::ALL
            .into_iter()
            .filter(|product| product.readable_by(subject))
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write, os::unix::fs::symlink};

    const ORIGIN: &str = "https://web.example.test";

    #[test]
    fn session_configuration_requires_canonical_origin_and_short_lifetime() {
        for invalid in [
            "http://web.example.test",
            "https://web.example.test/",
            "https://web.example.test:443",
            "https://web.example.test/path",
            "https://web.example.test?query",
            "https://web.example.test#fragment",
            "https://user@web.example.test",
            "https://WEB.example.test",
            "null",
        ] {
            assert!(validate_session_settings(Some(invalid), Duration::from_secs(300)).is_err());
        }
        assert!(validate_session_settings(Some(ORIGIN), Duration::from_secs(900)).is_ok());
        for invalid in [
            Duration::ZERO,
            Duration::from_millis(1_500),
            Duration::from_secs(901),
        ] {
            assert!(validate_session_settings(Some(ORIGIN), invalid).is_err());
        }
    }

    #[test]
    fn loopback_origin_requires_an_exact_unambiguous_http_authority() {
        for (host, origin) in [
            ("127.0.0.1:8090", "http://127.0.0.1:8090"),
            ("localhost:65535", "http://localhost:65535"),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, HeaderValue::from_static(host));
            headers.insert(header::ORIGIN, HeaderValue::from_static(origin));
            assert!(check_loopback_origin(&headers, true).is_ok());
            headers.append(header::ORIGIN, HeaderValue::from_static(origin));
            assert!(check_loopback_origin(&headers, false).is_err());
            headers.remove(header::ORIGIN);
            assert!(check_loopback_origin(&headers, false).is_err());
            headers.insert(FETCH_SITE, HeaderValue::from_static("same-origin"));
            assert!(check_loopback_origin(&headers, false).is_ok());
            assert!(check_loopback_origin(&headers, true).is_err());
            headers.append(FETCH_SITE, HeaderValue::from_static("same-origin"));
            assert!(check_loopback_origin(&headers, false).is_err());
            headers.insert(FETCH_SITE, HeaderValue::from_static("same-origin"));
            headers.append(header::HOST, HeaderValue::from_static(host));
            assert!(check_loopback_origin(&headers, false).is_err());
        }

        for authority in [
            "127.0.0.1.evil:8090",
            "localhost.evil:8090",
            "user@localhost:8090",
            "localhost:8090@localhost:8090",
            "127.0.0.2:8090",
            "127.1:8090",
            "2130706433:8090",
            "[::1]:8090",
            "localhost.:8090",
            "localhost",
            "localhost:",
            "localhost:0",
            "localhost:08090",
            "localhost:+8090",
            "localhost:65536",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, HeaderValue::from_static(authority));
            headers.insert(
                header::ORIGIN,
                HeaderValue::from_str(&format!("http://{authority}")).unwrap(),
            );
            assert!(
                check_loopback_origin(&headers, true).is_err(),
                "{authority}"
            );
        }
    }

    #[test]
    fn browser_session_requires_unique_origin_and_csrf_and_expires_server_side() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web.env");
        let bearer = Uuid::new_v4().to_string();
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap()
            .write_all(format!("WEB_ADMIN_TOKEN={bearer}").as_bytes())
            .unwrap();
        let access = WriteAccess::load(&path, 10, Duration::from_secs(60))
            .unwrap()
            .with_sessions(Some(ORIGIN), Duration::from_secs(300))
            .unwrap();
        let sessions = access.session_policy().ok().unwrap();
        let (token, created) = sessions.create(None);
        let cookie_value =
            HeaderValue::from_str(&format!("{SESSION_COOKIE_NAME}={token}")).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, cookie_value.clone());
        assert!(
            access
                .check_read(&headers, Product::RestrictedMarketData)
                .is_ok()
        );
        assert!(matches!(
            access.authorize(&headers),
            Err(WriteDenied::OriginRejected)
        ));
        headers.insert(header::ORIGIN, HeaderValue::from_static(ORIGIN));
        assert!(matches!(
            access.authorize(&headers),
            Err(WriteDenied::CsrfRejected)
        ));
        let csrf = HeaderValue::from_str(&created.csrf_token).unwrap();
        headers.insert(CSRF_HEADER, csrf.clone());
        headers.append(header::ORIGIN, HeaderValue::from_static(ORIGIN));
        assert!(matches!(
            access.authorize(&headers),
            Err(WriteDenied::OriginRejected)
        ));
        headers.insert(header::ORIGIN, HeaderValue::from_static(ORIGIN));
        headers.append(CSRF_HEADER, csrf);
        assert!(matches!(
            access.authorize(&headers),
            Err(WriteDenied::CsrfRejected)
        ));
        headers.insert(
            CSRF_HEADER,
            HeaderValue::from_str(&created.csrf_token).unwrap(),
        );
        assert!(access.authorize(&headers).is_ok());

        headers.append(header::COOKIE, cookie_value.clone());
        assert!(matches!(
            access.check_read(&headers, Product::Integrations),
            Err(WriteDenied::Unauthorized)
        ));
        headers.insert(header::COOKIE, cookie_value);
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer invalid"),
        );
        assert!(matches!(
            access.check_read(&headers, Product::Integrations),
            Err(WriteDenied::Unauthorized)
        ));
        headers.remove(header::AUTHORIZATION);
        sessions
            .entries
            .lock()
            .unwrap()
            .front_mut()
            .unwrap()
            .expires_at = Instant::now();
        assert!(matches!(
            access.check_read(&headers, Product::Integrations),
            Err(WriteDenied::SessionUnauthorized)
        ));

        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {bearer}")).unwrap(),
        );
        headers.remove(header::ORIGIN);
        assert!(access.authorize_same_origin(&headers).is_ok());
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://other.example.test"),
        );
        assert!(matches!(
            access.authorize_same_origin(&headers),
            Err(WriteDenied::OriginRejected)
        ));
        assert!(access.authorize(&headers).is_ok());
    }

    #[test]
    fn secret_file_rejects_wrong_mode_symlinks_and_ambiguous_assignments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web.env");
        assert!(load_digest(&path).unwrap().is_none());
        let assignment = format!("WEB_ADMIN_TOKEN={}", uuid::Uuid::new_v4());
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap()
            .write_all(assignment.as_bytes())
            .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(load_digest(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_digest(&path).unwrap().is_some());
        let link = dir.path().join("secret-link");
        symlink(&path, &link).unwrap();
        assert!(load_digest(&link).is_err());
        for input in [
            format!("{assignment}\n{assignment}"),
            format!("{assignment}\nOTHER=value"),
            "WEB_ADMIN_TOKEN=".into(),
        ] {
            fs::write(&path, input).unwrap();
            assert!(load_digest(&path).is_err());
        }
    }
}
