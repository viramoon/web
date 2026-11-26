use super::database::{Cluster, Server, checked, expect_http, http_fixture, web};
use openssl::{pkey::PKey, rsa::Rsa};
use reqwest::{
    RequestBuilder, Response, StatusCode,
    header::{CACHE_CONTROL, COOKIE, ETAG, HeaderMap, IF_MATCH, ORIGIN, SET_COOKIE},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use uuid::Uuid;

const SITE: &str = "https://web.example.test";

struct CredentialFixture {
    bearer: String,
    token_file: PathBuf,
    public_key: PathBuf,
    envelopes: PathBuf,
}

impl CredentialFixture {
    fn new(directory: &Path) -> Self {
        let bearer = Uuid::new_v4().simple().to_string();
        let token_file = directory.join("web.env");
        writeln!(
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&token_file)
                .unwrap(),
            "WEB_ADMIN_TOKEN={bearer}"
        )
        .unwrap();
        let recipient = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let public_key = directory.join("recipient.pem");
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&public_key)
            .unwrap()
            .write_all(&recipient.public_key_to_pem().unwrap())
            .unwrap();
        let envelopes = directory.join("envelopes");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&envelopes)
            .unwrap();
        Self {
            bearer,
            token_file,
            public_key,
            envelopes,
        }
    }

    fn command(&self, pg: &Cluster, public: &Path, port: u16) -> Command {
        let mut command = web(pg, public, port);
        command
            .env("WEB_ADMIN_TOKEN_FILE", &self.token_file)
            .env("WEB_PUBLIC_ORIGIN", SITE)
            .env("WEB_CREDENTIAL_DIR", &self.envelopes)
            .env("WEB_CREDENTIAL_RECIPIENT_PUBLIC_KEY_FILE", &self.public_key)
            .env("RUST_LOG", "web=info,sqlx=debug");
        command
    }
}

fn persisted_integrations(pg: &Cluster) -> String {
    checked(pg.client("psql").args([
        "-XAt",
        "-d",
        "web_test_db",
        "-c",
        "SELECT jsonb_build_object(
            'configs', (SELECT jsonb_agg(c) FROM app.integration_configs c),
            'credentials', (SELECT jsonb_agg(c) FROM app.integration_credentials c),
            'versions', (SELECT jsonb_agg(v) FROM app.integration_versions v),
            'tests', (SELECT jsonb_agg(t) FROM app.integration_tests t),
            'audit', (SELECT jsonb_agg(a) FROM app.integration_audit a)
         )",
    ]))
}

async fn response(request: RequestBuilder, status: StatusCode, token: &str) -> (HeaderMap, Value) {
    checked_response(request.send().await.unwrap(), status, token).await
}

async fn checked_response(
    response: Response,
    status: StatusCode,
    token: &str,
) -> (HeaderMap, Value) {
    assert_eq!(response.status(), status);
    let headers = response.headers().clone();
    assert_eq!(headers[CACHE_CONTROL], "no-store");
    assert!(headers.values().all(|header| {
        !header
            .as_bytes()
            .windows(token.len())
            .any(|bytes| bytes == token.as_bytes())
    }));
    let raw = response.text().await.unwrap();
    assert!(!raw.contains(token));
    (headers, serde_json::from_str(&raw).unwrap())
}

#[tokio::test]
async fn postgres_integration_http_permissions_conflicts_and_secret_redaction() {
    let pg = Cluster::create();
    let (public, port, base, client) = http_fixture();
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let fixture = CredentialFixture::new(temp.path());
    let bearer = fixture.bearer.as_str();
    let envelopes = &fixture.envelopes;
    let command = || fixture.command(&pg, public.path(), port);
    checked(command().arg("migrate"));
    let mut logs = tempfile::tempfile_in("/tmp").unwrap();
    let server = Server(
        command()
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(Stdio::from(logs.try_clone().unwrap()))
            .spawn()
            .unwrap(),
    );
    expect_http(&client, &format!("{base}/healthz/live"), 200).await;

    let catalog = format!("{base}/api/v1/integrations/catalog");
    let configs = format!("{base}/api/v1/integrations");
    let token = format!("w15-token-{}", Uuid::new_v4());
    for url in [&catalog, &configs] {
        response(client.get(url), StatusCode::UNAUTHORIZED, &token).await;
    }
    let (_, entries) = response(
        client.get(&catalog).bearer_auth(bearer),
        StatusCode::OK,
        &token,
    )
    .await;
    assert!(
        entries
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["connector"] == "licensed_options")
    );

    let (headers, session) = response(
        client
            .post(format!("{base}/api/v1/admin/session"))
            .bearer_auth(bearer)
            .header(ORIGIN, SITE),
        StatusCode::CREATED,
        bearer,
    )
    .await;
    let cookie = headers[SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let csrf = session["csrf_token"].as_str().unwrap();
    let authorized = |request: RequestBuilder| {
        request
            .header(COOKIE, cookie)
            .header(ORIGIN, SITE)
            .header("x-csrf-token", csrf)
    };
    let (_, existing) = response(
        client.get(&configs).header(COOKIE, cookie),
        StatusCode::OK,
        &token,
    )
    .await;
    assert_eq!(existing, json!([]));

    let (headers, market) = response(
        authorized(client.post(&configs)).json(&json!({
            "connector": "binance_spot",
            "settings": {"symbol": "BTCUSDT"}
        })),
        StatusCode::CREATED,
        &token,
    )
    .await;
    assert_eq!(market["latest"]["state"], "draft");
    assert!(market["active"].is_null());
    let market_url = format!("{configs}/{}", market["id"].as_str().unwrap());
    response(
        authorized(client.patch(&market_url))
            .header(IF_MATCH, headers[ETAG].clone())
            .json(&json!({"action": "configure", "settings": {"symbol": "ETHUSDT"}})),
        StatusCode::OK,
        &token,
    )
    .await;

    let (headers, integration) = response(
        client.post(&configs).bearer_auth(bearer).json(&json!({
            "connector": "licensed_options",
            "settings": {"symbol": "SPX"}
        })),
        StatusCode::CREATED,
        &token,
    )
    .await;
    let id = integration["id"].as_str().unwrap();
    let credential_url = format!("{configs}/{id}/credential");
    let tests_url = format!("{configs}/{id}/tests");
    let initial_revision = &headers[ETAG];
    let rejected = client
        .put(&credential_url)
        .header(IF_MATCH, initial_revision)
        .json(&json!({".env": token}));
    response(rejected, StatusCode::UNAUTHORIZED, &token).await;
    response(
        client
            .put(&credential_url)
            .bearer_auth(bearer)
            .header(IF_MATCH, initial_revision)
            .json(&json!({".env": token})),
        StatusCode::UNPROCESSABLE_ENTITY,
        &token,
    )
    .await;
    assert_eq!(fs::read_dir(envelopes).unwrap().count(), 0);

    let credential = json!({"api_token": token});
    let session_update = authorized(client.put(&credential_url))
        .header(IF_MATCH, initial_revision)
        .json(&credential);
    let bearer_update = client
        .put(&credential_url)
        .bearer_auth(bearer)
        .header(IF_MATCH, initial_revision)
        .json(&credential);
    let (first, second) = tokio::join!(session_update.send(), bearer_update.send());
    let first = first.unwrap();
    let second = second.unwrap();
    let (updated, rejected) = if first.status() == StatusCode::OK {
        (first, second)
    } else {
        (second, first)
    };
    let (_, conflict) = checked_response(rejected, StatusCode::CONFLICT, &token).await;
    assert_eq!(conflict["error"]["code"], "integration_conflict");
    let (headers, staged) = checked_response(updated, StatusCode::OK, &token).await;
    let staged_revision = &headers[ETAG];
    assert_eq!(staged["latest"]["credential"]["mask"], "********");
    assert!(staged["latest"]["credential"]["rotated_at"].is_string());
    assert!(staged["latest"]["credential"].get("reference").is_none());
    assert_eq!(fs::read_dir(envelopes).unwrap().count(), 1);

    response(
        client
            .post(&tests_url)
            .bearer_auth(bearer)
            .header(IF_MATCH, staged_revision)
            .json(&json!({"target": format!("https://example.invalid/{token}")})),
        StatusCode::UNPROCESSABLE_ENTITY,
        &token,
    )
    .await;
    let (_, test) = response(
        client
            .post(&tests_url)
            .bearer_auth(bearer)
            .header(IF_MATCH, staged_revision),
        StatusCode::ACCEPTED,
        &token,
    )
    .await;
    assert_eq!(test["status"], "unavailable");
    assert_eq!(test["integration_id"], id);
    assert!(test["finished_at"].is_string());
    let result_url = format!(
        "{base}/api/v1/integration-tests/{}",
        test["id"].as_str().unwrap()
    );
    response(client.get(&result_url), StatusCode::UNAUTHORIZED, &token).await;
    let (_, result) = response(
        client.get(&result_url).header(COOKIE, cookie),
        StatusCode::OK,
        &token,
    )
    .await;
    assert_eq!(result, test);
    let (_, rows) = response(
        client.get(&configs).bearer_auth(bearer),
        StatusCode::OK,
        &token,
    )
    .await;
    let candidate = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id)
        .unwrap();
    assert_eq!(candidate["latest"]["state"], "candidate");
    assert!(candidate["active"].is_null());
    let current_revision = candidate["revision"].as_i64().unwrap();
    let (_, revoked) = response(
        client
            .delete(&credential_url)
            .bearer_auth(bearer)
            .header(IF_MATCH, format!("\"{current_revision}\"")),
        StatusCode::OK,
        &token,
    )
    .await;
    assert!(revoked["latest"]["credential"].is_null());

    let persisted = persisted_integrations(&pg);
    assert!(!persisted.contains(&token));
    let persisted: Value = serde_json::from_str(&persisted).unwrap();
    let credentials = persisted["credentials"].as_array().unwrap();
    assert_eq!(credentials.len(), 1);
    let credential = &credentials[0];
    assert_eq!(credential["mask"], "********");
    assert_eq!(credential["format"], "jwe");
    assert_eq!(credential["integration_id"], id);
    assert!(credential["revoked_at"].is_string());
    let reference = Uuid::parse_str(credential["id"].as_str().unwrap()).unwrap();
    let blob = envelopes.join(format!("{reference}.jwe"));
    assert_eq!(
        fs::metadata(&blob).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    assert!(!fs::read_to_string(blob).unwrap().contains(&token));
    assert!(
        persisted["audit"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["action"] == "credential_revoked")
    );

    drop(server);
    pg.stop();
    logs.seek(SeekFrom::Start(0)).unwrap();
    let mut captured = String::new();
    logs.read_to_string(&mut captured).unwrap();
    assert!(captured.contains("request complete"));
    assert!(!captured.contains(&token));
    assert!(!captured.contains(bearer));
}

#[tokio::test]
async fn postgres_rejects_untyped_integration_inputs_and_revokes_without_credential_store() {
    let pg = Cluster::create();
    let (public, port, base, client) = http_fixture();
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let fixture = CredentialFixture::new(temp.path());
    let bearer = fixture.bearer.as_str();
    let command = || fixture.command(&pg, public.path(), port);
    checked(command().arg("migrate"));
    let server = Server(
        command()
            .arg("serve")
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    expect_http(&client, &format!("{base}/healthz/live"), 200).await;

    let marker = format!("w15-invalid-{}", Uuid::new_v4());
    let configs = format!("{base}/api/v1/integrations");
    let (headers, integration) = response(
        client.post(&configs).bearer_auth(bearer).json(&json!({
            "connector": "licensed_options",
            "settings": {"symbol": "SPX"}
        })),
        StatusCode::CREATED,
        &marker,
    )
    .await;
    let revision = &headers[ETAG];
    let id = integration["id"].as_str().unwrap();
    let config_url = format!("{configs}/{id}");
    let credential_url = format!("{config_url}/credential");
    let path = format!("/tmp/{marker}/adapter.pem");
    let dotenv = format!("API_TOKEN={marker}\n");
    let pem = format!("-----BEGIN PRIVATE KEY-----\n{marker}\n-----END PRIVATE KEY-----");
    let before = persisted_integrations(&pg);
    for body in [
        json!({"action": "disable", ".env": dotenv}),
        json!({"action": "disable", "path": path}),
    ] {
        let (_, error) = response(
            client
                .patch(&config_url)
                .bearer_auth(bearer)
                .header(IF_MATCH, revision)
                .json(&body),
            StatusCode::UNPROCESSABLE_ENTITY,
            &marker,
        )
        .await;
        assert_eq!(error["error"]["code"], "invalid_request");
    }
    for input in [path, dotenv, pem] {
        let (_, error) = response(
            client
                .put(&credential_url)
                .bearer_auth(bearer)
                .header(IF_MATCH, revision)
                .json(&json!({"api_token": input})),
            StatusCode::UNPROCESSABLE_ENTITY,
            &marker,
        )
        .await;
        assert_eq!(error["error"]["code"], "invalid_request");
    }
    let (_, after) = response(
        client.get(&configs).bearer_auth(bearer),
        StatusCode::OK,
        &marker,
    )
    .await;
    assert_eq!(after, json!([integration]));
    assert_eq!(fs::read_dir(&fixture.envelopes).unwrap().count(), 0);
    let persisted = persisted_integrations(&pg);
    assert!(!persisted.contains(&marker));
    assert_eq!(persisted, before);

    let token = format!("w15-token-{}", Uuid::new_v4());
    let (headers, staged) = response(
        client
            .put(&credential_url)
            .bearer_auth(bearer)
            .header(IF_MATCH, revision)
            .json(&json!({"api_token": token})),
        StatusCode::OK,
        &token,
    )
    .await;
    assert_eq!(staged["latest"]["credential"]["mask"], "********");
    assert_eq!(fs::read_dir(&fixture.envelopes).unwrap().count(), 1);
    drop(server);

    let (public, port, base, client) = http_fixture();
    let server = Server(
        fixture
            .command(&pg, public.path(), port)
            .env_remove("WEB_CREDENTIAL_DIR")
            .env_remove("WEB_CREDENTIAL_RECIPIENT_PUBLIC_KEY_FILE")
            .arg("serve")
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    expect_http(&client, &format!("{base}/healthz/live"), 200).await;
    let (_, revoked) = response(
        client
            .delete(format!("{base}/api/v1/integrations/{id}/credential"))
            .bearer_auth(bearer)
            .header(IF_MATCH, &headers[ETAG]),
        StatusCode::OK,
        &token,
    )
    .await;
    assert!(revoked["latest"]["credential"].is_null());
    let persisted = persisted_integrations(&pg);
    assert!(!persisted.contains(&token));
    let persisted: Value = serde_json::from_str(&persisted).unwrap();
    assert!(persisted["credentials"][0]["revoked_at"].is_string());
    drop(server);
    pg.stop();
}

#[tokio::test]
async fn postgres_market_http_persists_candidate_evidence_and_activates_only_a_pass() {
    use axum::{Json, Router, extract::Path as RoutePath, routing::post};
    use chrono::Utc;
    use tokio::net::TcpListener;

    // This deterministic fixture uses fixed observations and the test clock.
    // The loopback producer generates no subscriptions or broker requests.
    let producer = Router::new().route(
        "/integrations/{id}/tests",
        post(
            |RoutePath(id): RoutePath<String>, Json(request): Json<Value>| async move {
                assert_eq!(request["connector"], "ibkr_paper");
                let (status, observation) = match request["symbol"].as_str().unwrap() {
                    "SPY" => (
                        "verified",
                        json!({
                            "origin": "live",
                            "observed_at": Utc::now(),
                            "details": {"kind": "ibkr_paper", "data": {
                                "paper_endpoint": true,
                                "paper_account": true,
                                "read_only": true,
                                "unique_market_data_client": true,
                                "stock_l1_market_data_type": 1,
                                "option_l1_market_data_type": 1,
                                "option_contract_id": 42,
                                "option_open_interest": true,
                                "option_greeks": true,
                                "nasdaq_depth_callback": true,
                                "remaining_l1_lines": 30,
                                "remaining_depth_lines": 1
                            }}
                        }),
                    ),
                    "QQQ" => ("no_permission", Value::Null),
                    _ => panic!("unexpected synthetic selector"),
                };
                Json(json!({
                    "integration_id": id,
                    "test_id": request["test_id"],
                    "config_version": request["config_version"],
                    "connector": request["connector"],
                    "symbol": request["symbol"],
                    "status": status,
                    "observation": observation
                }))
            },
        ),
    );
    let pg = Cluster::create();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let finance_base = format!("http://{}", listener.local_addr().unwrap());
    let producer_task = tokio::spawn(async move { axum::serve(listener, producer).await.unwrap() });
    let (public, port, base, client) = http_fixture();
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let fixture = CredentialFixture::new(temp.path());
    let command = || {
        let mut command = fixture.command(&pg, public.path(), port);
        command.env("FINANCE_BASE_URL", &finance_base);
        command
    };
    checked(command().arg("migrate"));
    let server = Server(
        command()
            .arg("serve")
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    expect_http(&client, &format!("{base}/healthz/live"), 200).await;
    let bearer = &fixture.bearer;
    let configs = format!("{base}/api/v1/integrations");
    let (created_headers, created) = response(
        client.post(configs).bearer_auth(bearer).json(&json!({
            "connector": "ibkr_paper", "settings": {"symbol": "SPY"}
        })),
        StatusCode::CREATED,
        bearer,
    )
    .await;
    let id = created["id"].as_str().unwrap();
    let config = format!("{base}/api/v1/integrations/{id}");
    let tests = format!("{config}/tests");
    let activate = format!("{config}/activate");
    let (test_headers, passed) = response(
        client
            .post(&tests)
            .bearer_auth(bearer)
            .header(IF_MATCH, &created_headers[ETAG]),
        StatusCode::ACCEPTED,
        bearer,
    )
    .await;
    assert_eq!(passed["integration_id"], id);
    assert_eq!(passed["config_version"], 1);
    assert_eq!(passed["status"], "passed");
    assert_eq!(passed["evidence"]["kind"], "market");
    let evidence = &passed["evidence"]["details"];
    assert_eq!(evidence["result"], "verified");
    assert_eq!(
        evidence["observation"]["details"]["data"]["option_contract_id"],
        42
    );
    let lookup = format!("{base}{}", test_headers["location"].to_str().unwrap());
    let (_, retrieved) = response(
        client.get(lookup).bearer_auth(bearer),
        StatusCode::OK,
        bearer,
    )
    .await;
    assert_eq!(retrieved, passed);
    let rows: Value = serde_json::from_str(&persisted_integrations(&pg)).unwrap();
    assert_eq!(rows["tests"][0]["evidence"], retrieved["evidence"]);
    let (candidate_headers, candidate) = response(
        client.get(&config).bearer_auth(bearer),
        StatusCode::OK,
        bearer,
    )
    .await;
    assert!(candidate["active"].is_null());

    let (active_headers, active) = response(
        client
            .post(&activate)
            .bearer_auth(bearer)
            .header(IF_MATCH, &candidate_headers[ETAG])
            .json(&json!({"test_id": passed["id"]})),
        StatusCode::OK,
        bearer,
    )
    .await;
    assert_eq!(active["active"]["version"], 1);
    let (staged_headers, _) = response(
        client
            .patch(&config)
            .bearer_auth(bearer)
            .header(IF_MATCH, &active_headers[ETAG])
            .json(&json!({"action": "configure", "settings": {"symbol": "QQQ"}})),
        StatusCode::OK,
        bearer,
    )
    .await;
    let (_, refused) = response(
        client
            .post(&tests)
            .bearer_auth(bearer)
            .header(IF_MATCH, &staged_headers[ETAG]),
        StatusCode::ACCEPTED,
        bearer,
    )
    .await;
    assert_eq!(refused["config_version"], 2);
    assert_eq!(refused["status"], "failed");
    assert_eq!(refused["evidence"]["details"]["result"], "no_permission");
    let (retained_headers, retained) = response(
        client.get(config).bearer_auth(bearer),
        StatusCode::OK,
        bearer,
    )
    .await;
    assert_eq!(retained["active"]["version"], 1);
    let (_, conflict) = response(
        client
            .post(activate)
            .bearer_auth(bearer)
            .header(IF_MATCH, &retained_headers[ETAG])
            .json(&json!({"test_id": refused["id"]})),
        StatusCode::CONFLICT,
        bearer,
    )
    .await;
    assert_eq!(conflict["error"]["code"], "integration_conflict");
    drop(server);
    producer_task.abort();
    let _ = producer_task.await;
    pg.stop();
}
