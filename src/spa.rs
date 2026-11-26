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
    RootNotice,
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
    if first == b"NOTICE" && segments.clone().nth(1).is_none() {
        return PathKind::RootNotice;
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
    let mut resp = match kind {
        PathKind::Invalid => StatusCode::NOT_FOUND.into_response(),
        PathKind::RootNotice => ServeFile::new(root.join("third-party/lightweight-charts/NOTICE"))
            .oneshot(req)
            .await
            .expect("ServeFile is infallible")
            .into_response(),
        _ => ServeDir::new(root)
            .oneshot(req)
            .await
            .expect("ServeDir is infallible")
            .into_response(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::WriteAccess, config::Settings, database::Database, finance::Finance, router,
    };
    use axum::body::to_bytes;
    use std::{fs, sync::Arc, time::Duration};

    #[tokio::test]
    async fn root_notice_serves_the_chart_notice_at_the_normalized_root() {
        let public = tempfile::tempdir().unwrap();
        let root = public.path();
        let chart = root.join("third-party/lightweight-charts");
        fs::create_dir_all(&chart).unwrap();
        let notice_file = chart.join("NOTICE");
        let notice = b"Lightweight Charts\r\nCopyright TradingView, Inc.\r\n";
        let license = b"Apache License\n";
        let index = b"<!doctype html><title>SPA</title>";
        fs::write(&notice_file, notice).unwrap();
        fs::write(chart.join("LICENSE"), license).unwrap();
        fs::write(root.join("index.html"), index).unwrap();

        let mut config = Settings::load(Path::new("config"), |_| None).unwrap();
        config.paths.static_root = root.into();
        let timeout = Duration::from_secs(1);
        let app = router(AppState {
            finance: Finance::new(&config.finance).unwrap(),
            db: Database::new(&config.database.url, timeout, timeout).unwrap(),
            write_access: WriteAccess::load(&root.join("unused-admin-token"), 10, timeout).unwrap(),
            credential_store: None,
            config: Arc::new(config),
            started_at: chrono::Utc::now(),
        });

        let get = |path: &str| {
            let request = Request::get(path).body(Body::empty()).unwrap();
            app.clone().oneshot(request)
        };
        for (path, expected) in [
            ("/NOTICE", notice.as_slice()),
            ("/%4eOTICE", notice.as_slice()),
            ("/./%2fNOTICE//", notice.as_slice()),
            ("/third-party/lightweight-charts/NOTICE", notice.as_slice()),
            (
                "/third-party/lightweight-charts/LICENSE",
                license.as_slice(),
            ),
            ("/", index.as_slice()),
            ("/dashboard", index.as_slice()),
        ] {
            let response = get(path).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "no-cache",
                "{path}"
            );
            assert_eq!(
                to_bytes(response.into_body(), 1024).await.unwrap(),
                expected,
                "{path}"
            );
        }

        for path in [
            "/static/third-party/lightweight-charts/NOTICE",
            "/assets/missing",
            "/third-party/lightweight-charts/missing",
            "/docs/NOTICE",
            "/docs%2fNOTICE",
            "/NOTICE/NOTICE",
            "/docs/../NOTICE",
            "/docs/%2e%2e/NOTICE",
            "/NOTICE/%FF",
        ] {
            let response = get(path).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert!(
                to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap()
                    .is_empty(),
                "{path}"
            );
        }

        for path in ["/api", "/healthz", "/%61pi/%2e%2e/NOTICE", "/%68ealthz/%FF"] {
            let response = get(path).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            let body = to_bytes(response.into_body(), 1024).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], "not_found", "{path}");
        }
        for path in ["/healthz/live", "/api/v1/me/capabilities"] {
            assert_eq!(get(path).await.unwrap().status(), StatusCode::OK, "{path}");
        }

        fs::remove_file(&notice_file).unwrap();
        let response = get("/NOTICE").await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
