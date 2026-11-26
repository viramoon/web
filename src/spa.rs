use crate::{AppState, api_error};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use percent_encoding::percent_decode_str;
use std::{borrow::Cow, path::Path};
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

#[derive(PartialEq, Eq)]
enum PathKind {
    Reserved,
    Invalid,
    Asset,
    Page,
}

fn classify_path(path: &str) -> PathKind {
    let decoded: Cow<'_, [u8]> = percent_decode_str(path).into();
    let mut segments = decoded
        .split(|&byte| byte == b'/')
        .filter(|segment| !segment.is_empty() && *segment != b".");
    let first = segments.clone().next().unwrap_or_default();
    // Reserved paths keep their JSON error response even with a malformed suffix.
    if matches!(first, b"api" | b"healthz") {
        return PathKind::Reserved;
    }
    if segments.clone().any(|segment| segment == b"..") || std::str::from_utf8(&decoded).is_err() {
        return PathKind::Invalid;
    }
    let filename = segments.next_back().unwrap_or_default();
    if matches!(first, b"assets" | b"static" | b"third-party")
        || filename.contains(&b'.')
        || matches!(filename, b"LICENSE" | b"NOTICE")
    {
        PathKind::Asset
    } else {
        PathKind::Page
    }
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
    let kind = classify_path(&path);
    if kind == PathKind::Reserved {
        return api_error(StatusCode::NOT_FOUND, "not_found", "Unknown endpoint");
    }
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    // The configured directory is a trusted build artifact. ServeDir validates request paths.
    let root = &state.config.paths.static_root;
    let method = req.method().clone();
    let mut resp = if kind == PathKind::Invalid {
        StatusCode::NOT_FOUND.into_response()
    } else {
        ServeDir::new(root)
            .oneshot(req)
            .await
            .expect("ServeDir is infallible")
            .into_response()
    };
    if resp.status() == StatusCode::NOT_FOUND && method == Method::GET && kind == PathKind::Page {
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
