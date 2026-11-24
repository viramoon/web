//! A single locally provisioned bearer credential for writes. No token is sent upstream.
use crate::{api_error, config::ConfigError};
use axum::{
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
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

#[derive(Clone)]
pub struct WriteAccess(Arc<Policy>);
struct Policy {
    digest: Option<[u8; 32]>,
    limit: usize,
    window: Duration,
    accepted: Mutex<VecDeque<Instant>>,
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
    RateLimited(u64),
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
            Self::RateLimited(seconds) => {
                let mut response = api_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate_limited",
                    "Write rate limit reached",
                );
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
        Ok(Self(Arc::new(Policy {
            digest: load_digest(path)?,
            limit: max_requests as usize,
            window,
            accepted: Mutex::new(VecDeque::new()),
        })))
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Result<(), WriteDenied> {
        let policy = &self.0;
        let Some(expected) = &policy.digest else {
            return Err(WriteDenied::Disabled);
        };
        let mut values = headers.get_all(header::AUTHORIZATION).iter();
        let token = values
            .next()
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
            .map(|(_, token)| token.trim_start_matches(' '));
        if values.next().is_some()
            || !token.is_some_and(|token| {
                let candidate: [u8; 32] = Sha256::digest(token.as_bytes()).into();
                bool::from(expected.ct_eq(&candidate))
            })
        {
            return Err(WriteDenied::Unauthorized);
        }
        Ok(())
    }

    pub fn authorize(&self, headers: &HeaderMap) -> Result<(), WriteDenied> {
        self.authenticate(headers)?;
        let policy = &self.0;
        // Only the provisioned credential owns this queue. Network addresses and
        // caller-controlled forwarding headers cannot create additional buckets.
        let mut accepted = policy.accepted.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        while accepted
            .front()
            .is_some_and(|time| now.duration_since(*time) >= policy.window)
        {
            accepted.pop_front();
        }
        if accepted.len() >= policy.limit {
            let remaining = policy
                .window
                .saturating_sub(now.duration_since(accepted[0]));
            let seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0);
            return Err(WriteDenied::RateLimited(seconds.max(1)));
        }
        accepted.push_back(now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write, os::unix::fs::symlink};
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
