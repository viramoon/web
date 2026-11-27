use super::{MARKET_STREAM, ORIGIN, SESSION_PATH, StreamTestApp, router, send};
use axum::{
    Router,
    http::{HeaderValue, Request, StatusCode, header},
    serve::Listener,
};
use std::{future::pending, io, time::Duration};
use tokio::{
    io::{DuplexStream, duplex},
    sync::oneshot,
    time::timeout,
};
use tokio_tungstenite::{
    client_async,
    tungstenite::{Error as SocketError, client::IntoClientRequest},
};

struct SingleConnection(Option<DuplexStream>);

impl Listener for SingleConnection {
    type Io = DuplexStream;
    type Addr = ();

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.0.take() {
            Some(stream) => (stream, ()),
            None => pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(())
    }
}

async fn assert_upgrade_rejected(app: Router, request: Request<()>, status: StatusCode) {
    let (client, server) = duplex(4096);
    let (shutdown, stopping) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(SingleConnection(Some(server)), app)
            .with_graceful_shutdown(async move {
                let _ = stopping.await;
            })
            .await
            .unwrap();
    });
    let deadline = Duration::from_secs(5);
    let response = timeout(deadline, client_async(request, client))
        .await
        .unwrap();
    let Err(SocketError::Http(response)) = response else {
        panic!("an unauthorized peer must receive an HTTP rejection before upgrade");
    };
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    shutdown.send(()).unwrap();
    timeout(deadline, server).await.unwrap().unwrap();
}

#[tokio::test]
async fn websocket_wire_handshake_rejects_unauthenticated_and_cross_origin_clients() {
    let fixture = StreamTestApp::new();
    let app = router(fixture.state);
    let request = || {
        format!("ws://web.example.test{MARKET_STREAM}")
            .into_client_request()
            .unwrap()
    };
    assert_upgrade_rejected(app.clone(), request(), StatusCode::UNAUTHORIZED).await;

    let session = send(
        &app,
        Request::post(SESSION_PATH)
            .header(header::AUTHORIZATION, fixture.bearer)
            .header(header::ORIGIN, ORIGIN),
    )
    .await;
    assert_eq!(session.status(), StatusCode::CREATED);
    let mut cookie = HeaderValue::from_str(
        session.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap(),
    )
    .unwrap();
    cookie.set_sensitive(true);
    let mut cross_origin = request();
    cross_origin.headers_mut().insert(header::COOKIE, cookie);
    cross_origin.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_static("https://other.example.test"),
    );
    assert_upgrade_rejected(app.clone(), cross_origin.clone(), StatusCode::FORBIDDEN).await;
    cross_origin
        .headers_mut()
        .insert(header::HOST, HeaderValue::from_static("localhost:8090"));
    cross_origin.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_static("http://localhost:8090"),
    );
    assert_upgrade_rejected(app, cross_origin, StatusCode::FORBIDDEN).await;
}

#[tokio::test]
async fn loopback_cookie_websocket_rejects_invalid_origin_or_host_before_upgrade() {
    let fixture = StreamTestApp::with_origin(None);
    let app = router(fixture.state);
    let session = send(
        &app,
        Request::post(SESSION_PATH)
            .header(header::AUTHORIZATION, fixture.bearer)
            .header(header::HOST, "localhost:8090")
            .header(header::ORIGIN, "http://localhost:8090"),
    )
    .await;
    assert_eq!(session.status(), StatusCode::CREATED);
    let mut cookie = HeaderValue::from_str(
        session.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap(),
    )
    .unwrap();
    cookie.set_sensitive(true);

    for (host, origin) in [
        ("localhost:8090", "http://localhost:8090"),
        ("127.0.0.1:8090", "http://127.0.0.1:8090"),
    ] {
        let response = send(
            &app,
            Request::get(MARKET_STREAM)
                .header(header::HOST, host)
                .header(header::ORIGIN, origin)
                .header(header::COOKIE, &cookie),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    for (host, origin) in [
        ("localhost:8090", None),
        ("localhost:8090", Some("http://remote.invalid:8090")),
        ("localhost:8090", Some("http://localhost:8091")),
        ("localhost:8090", Some("http://127.0.0.1:8090")),
        ("localhost:8090", Some("http://user@localhost:8090")),
        ("localhost.evil:8090", Some("http://localhost.evil:8090")),
        ("127.0.0.1.evil:8090", Some("http://127.0.0.1.evil:8090")),
    ] {
        let mut request = format!("ws://localhost:8090{MARKET_STREAM}")
            .into_client_request()
            .unwrap();
        let headers = request.headers_mut();
        headers.insert(header::HOST, HeaderValue::from_static(host));
        headers.insert(header::COOKIE, cookie.clone());
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, HeaderValue::from_static(origin));
        }
        assert_upgrade_rejected(app.clone(), request, StatusCode::FORBIDDEN).await;
    }
}
