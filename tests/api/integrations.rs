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
    process::Stdio,
};
use uuid::Uuid;

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
    const SITE: &str = "https://web.example.test";
    let pg = Cluster::create();
    let (public, port, base, client) = http_fixture();
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let bearer = Uuid::new_v4().simple().to_string();
    let token_file = temp.path().join("web.env");
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
    let public_key = temp.path().join("recipient.pem");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&public_key)
        .unwrap()
        .write_all(&recipient.public_key_to_pem().unwrap())
        .unwrap();
    drop(recipient);
    let envelopes = temp.path().join("envelopes");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&envelopes)
        .unwrap();
    let command = || {
        let mut command = web(&pg, public.path(), port);
        command
            .env("WEB_ADMIN_TOKEN_FILE", &token_file)
            .env("WEB_PUBLIC_ORIGIN", SITE)
            .env("WEB_CREDENTIAL_DIR", &envelopes)
            .env("WEB_CREDENTIAL_RECIPIENT_PUBLIC_KEY_FILE", &public_key)
            .env("RUST_LOG", "web=info,sqlx=debug");
        command
    };
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
        client.get(&catalog).bearer_auth(&bearer),
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
            .bearer_auth(&bearer)
            .header(ORIGIN, SITE),
        StatusCode::CREATED,
        &bearer,
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
        client.post(&configs).bearer_auth(&bearer).json(&json!({
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
            .bearer_auth(&bearer)
            .header(IF_MATCH, initial_revision)
            .json(&json!({".env": token})),
        StatusCode::UNPROCESSABLE_ENTITY,
        &token,
    )
    .await;
    assert_eq!(fs::read_dir(&envelopes).unwrap().count(), 0);

    let credential = json!({"api_token": token});
    let session_update = authorized(client.put(&credential_url))
        .header(IF_MATCH, initial_revision)
        .json(&credential);
    let bearer_update = client
        .put(&credential_url)
        .bearer_auth(&bearer)
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
    assert_eq!(fs::read_dir(&envelopes).unwrap().count(), 1);

    response(
        client
            .post(&tests_url)
            .bearer_auth(&bearer)
            .header(IF_MATCH, staged_revision)
            .json(&json!({"target": format!("https://example.invalid/{token}")})),
        StatusCode::UNPROCESSABLE_ENTITY,
        &token,
    )
    .await;
    let (_, test) = response(
        client
            .post(&tests_url)
            .bearer_auth(&bearer)
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
        client.get(&configs).bearer_auth(&bearer),
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
            .bearer_auth(&bearer)
            .header(IF_MATCH, format!("\"{current_revision}\"")),
        StatusCode::OK,
        &token,
    )
    .await;
    assert!(revoked["latest"]["credential"].is_null());

    let persisted = checked(pg.client("psql").args([
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
    ]));
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
    assert!(!captured.contains(&bearer));
}
