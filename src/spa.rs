use crate::{AppState, api_error};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use std::path::Path;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

pub fn reserved(path: &str) -> bool {
    matches!(path, "/api" | "/healthz")
        || path.starts_with("/api/")
        || path.starts_with("/healthz/")
}

fn hashed_asset(path: &str) -> bool {
    if !path.starts_with("/assets/") {
        return false;
    }
    let Some((stem, _)) = path.rsplit('/').next().unwrap_or("").rsplit_once('.') else {
        return false;
    };
    // Default Vite content hashes are eight base64url characters, including '-' and '_'.
    stem.as_bytes()
        .split_at_checked(stem.len().saturating_sub(8))
        .is_some_and(|(prefix, hash)| {
            hash.len() == 8
                && prefix.ends_with(b"-")
                && hash
                    .iter()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        })
}

pub async fn serve(State(state): State<AppState>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    if reserved(&path) {
        return api_error(StatusCode::NOT_FOUND, "not_found", "Unknown endpoint");
    }
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    // The configured directory is a trusted build artifact. ServeDir validates request paths.
    let root = &state.config.paths.static_root;
    let method = req.method().clone();
    let mut resp = ServeDir::new(root)
        .oneshot(req)
        .await
        .expect("ServeDir is infallible")
        .into_response();
    if resp.status() == StatusCode::NOT_FOUND
        && method == Method::GET
        && path != "/assets"
        && !path.starts_with("/assets/")
    {
        let request = Request::new(Body::empty());
        resp = ServeFile::new(root.join("index.html"))
            .oneshot(request)
            .await
            .expect("ServeFile is infallible")
            .into_response();
    }
    let cache = if resp.status().is_success() && hashed_asset(&path) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    resp
}

pub async fn available(root: &Path) -> bool {
    tokio::fs::metadata(root.join("index.html"))
        .await
        .is_ok_and(|m| m.is_file())
}
