use super::database::{Cluster, Server, checked, expect_http, http_fixture, web};
use chrono::{DateTime, TimeDelta};
use reqwest::{
    RequestBuilder, StatusCode,
    header::{CACHE_CONTROL, ETAG, HeaderMap, IF_MATCH, LOCATION},
};
use serde_json::{Value, json};
use std::{
    fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt, process::Stdio, time::Duration,
};
use uuid::Uuid;
use web::{
    database::Database,
    notifications::{
        model::{AlertSignal, Condition, Phase},
        store,
    },
};

async fn check(request: RequestBuilder, status: StatusCode, token: &str) -> (HeaderMap, Value) {
    let response = request.bearer_auth(token).send().await.unwrap();
    let actual = response.status();
    let headers = response.headers().clone();
    let body = response.text().await.unwrap();
    assert!(!body.contains(token));
    assert_eq!(actual, status, "{body}");
    assert_eq!(headers[CACHE_CONTROL], "no-store");
    (headers, serde_json::from_str(&body).unwrap())
}

#[tokio::test]
async fn postgres_notification_http_rule_versions_alert_paging_and_idempotent_test() {
    let pg = Cluster::create();
    let (public, port, base, client) = http_fixture();
    let local = tempfile::tempdir_in("/tmp").unwrap();
    let token_file = local.path().join("web.env");
    let token = Uuid::new_v4().to_string();
    writeln!(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&token_file)
            .unwrap(),
        "WEB_ADMIN_TOKEN={token}"
    )
    .unwrap();
    let command = || {
        let mut command = web(&pg, public.path(), port);
        command.env("WEB_ADMIN_TOKEN_FILE", &token_file);
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
    expect_http(&client, &format!("{base}/healthz/ready"), 200).await;
    let status = expect_http(&client, &format!("{base}/api/v1/system/status"), 200).await;
    assert_eq!(status["notifications"]["state"], "idle");

    // A fixed pair of synthetic rule IDs and UTC instants gives repeatable page order.
    let rule_id = Uuid::from_u128(1);
    let other_rule_id = Uuid::from_u128(2);
    let rules = format!("{base}/api/v1/notification-rules");
    let rule = format!("{rules}/{rule_id}");
    let other_rule = format!("{rules}/{other_rule_id}");
    let mut settings = json!({
        "condition": "source_unavailable",
        "severity": "important",
        "enabled": true,
        "email_enabled": true,
    });
    check(client.get(&rule), StatusCode::NOT_FOUND, &token).await;
    let (headers, created) = check(
        client.put(&rule).header(IF_MATCH, "\"0\"").json(&settings),
        StatusCode::CREATED,
        &token,
    )
    .await;
    assert_eq!(headers[ETAG], "\"1\"");
    assert_eq!(created["id"], rule_id.to_string());
    assert_eq!(created["min_occurrences"], 1);
    settings["cooldown_seconds"] = json!(300);
    let (revised_headers, revised) = check(
        client
            .put(&rule)
            .header(IF_MATCH, headers[ETAG].clone())
            .json(&settings),
        StatusCode::OK,
        &token,
    )
    .await;
    assert_eq!(revised_headers[ETAG], "\"2\"");
    assert_eq!(revised["cooldown_seconds"], 300);
    check(
        client
            .put(&rule)
            .header(IF_MATCH, headers[ETAG].clone())
            .json(&settings),
        StatusCode::CONFLICT,
        &token,
    )
    .await;
    let (headers, read_rule) = check(client.get(&rule), StatusCode::OK, &token).await;
    assert_eq!(headers[ETAG], revised_headers[ETAG]);
    assert_eq!(read_rule, revised);
    settings["enabled"] = json!(false);
    check(
        client
            .put(&other_rule)
            .header(IF_MATCH, "\"0\"")
            .json(&settings),
        StatusCode::CREATED,
        &token,
    )
    .await;
    let (_, first) = check(
        client.get(&rules).query(&[("limit", "1")]),
        StatusCode::OK,
        &token,
    )
    .await;
    assert_eq!(first["items"][0]["id"], rule_id.to_string());
    assert_eq!(first["next_cursor"], rule_id.to_string());
    let (_, second) = check(
        client.get(&rules).query(&[
            ("limit", "1"),
            ("cursor", first["next_cursor"].as_str().unwrap()),
        ]),
        StatusCode::OK,
        &token,
    )
    .await;
    assert_eq!(second["items"][0]["id"], other_rule_id.to_string());
    assert!(second["next_cursor"].is_null());

    let test_id = Uuid::from_u128(3);
    let tests = format!("{base}/api/v1/notification-channels/email/tests");
    let queue = || {
        client
            .post(&tests)
            .header("Idempotency-Key", test_id.to_string())
    };
    let (headers, test) = check(queue(), StatusCode::ACCEPTED, &token).await;
    assert_eq!(test["id"], test_id.to_string());
    assert_eq!(test["status"], "pending");
    let location = headers[LOCATION].to_str().unwrap();
    assert_eq!(
        location,
        format!("/api/v1/notification-channels/email/tests/{test_id}")
    );
    let (_, test_read) = check(
        client.get(format!("{base}{location}")),
        StatusCode::OK,
        &token,
    )
    .await;
    let (_, duplicate) = check(queue(), StatusCode::ACCEPTED, &token).await;
    assert_eq!(test_read, test);
    assert_eq!(duplicate, test);
    let count = checked(pg.client("psql").args([
        "-XAt",
        "-d",
        "web_test_db",
        "-c",
        "SELECT count(*) FROM app.notification_outbox WHERE kind = 'test'",
    ]));
    assert_eq!(count.trim(), "1");

    let db = Database::new(&pg.url(), Duration::from_secs(2), Duration::from_secs(2)).unwrap();
    let start = DateTime::parse_from_rfc3339("2026-11-27T00:00:00Z")
        .unwrap()
        .to_utc();
    let mut newest_event = None;
    for (incident, elapsed) in [(4, 0), (5, 60)] {
        let when = start + TimeDelta::seconds(elapsed);
        let recorded = store::record_signal(
            &db,
            rule_id,
            AlertSignal {
                condition: Condition::SourceUnavailable,
                incident_key: Uuid::from_u128(incident),
                window_start: when,
                observed_at: when,
                phase: Phase::Triggered,
            },
        )
        .await
        .unwrap();
        newest_event = Some(recorded.id);
    }
    // Seed normalized audit outcomes for the HTTP read path. The worker suite
    // exercises creating these records from a process-internal SMTP substitute.
    let pool = sqlx::PgPool::connect(&pg.url()).await.unwrap();
    let delivery: Uuid = sqlx::query_scalar(
        "UPDATE app.notification_outbox SET status = 'failed', attempts = 1,
             send_started_at = now(), last_outcome = 'rejected' WHERE event_id = $1 RETURNING id",
    )
    .bind(newest_event.unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO app.notification_attempts (outbox_id, attempt, started_at, finished_at, outcome)
         VALUES ($1, 1, now(), now(), 'rejected')",
    )
    .bind(delivery)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO app.notification_deferrals (outbox_id, reason, deferred_at, available_at)
         VALUES ($1, 'quiet_hours', now() - interval '1 hour', now())",
    )
    .bind(delivery)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    let alerts = format!("{base}/api/v1/alerts");
    let (_, newest) = check(
        client.get(&alerts).query(&[("limit", "1")]),
        StatusCode::OK,
        &token,
    )
    .await;
    let item = &newest["items"][0];
    assert_eq!(item["disposition"], "pending");
    assert_eq!(item["observed_at"], "2026-11-27T00:01:00Z");
    let delivery = &item["deliveries"][0];
    assert_eq!(delivery["status"], "failed");
    assert_eq!(delivery["last_outcome"], "rejected");
    assert_eq!(delivery["attempts"][0]["outcome"], "rejected");
    assert_eq!(delivery["deferrals"][0]["reason"], "quiet_hours");
    assert_eq!(item["sequence"], newest["next_cursor"]);
    let cursor = newest["next_cursor"].as_str().unwrap();
    let (_, oldest) = check(
        client
            .get(&alerts)
            .query(&[("limit", "1"), ("cursor", cursor)]),
        StatusCode::OK,
        &token,
    )
    .await;
    assert_eq!(oldest["items"][0]["observed_at"], "2026-11-27T00:00:00Z");
    assert!(oldest["next_cursor"].is_null());

    let alert = format!("{alerts}/{}", item["id"].as_str().unwrap());
    let ack = format!("{alert}/ack");
    let (_, before) = check(client.get(&alert), StatusCode::OK, &token).await;
    assert!(before["acknowledged_at"].is_null());
    let (_, acked) = check(client.post(&ack), StatusCode::OK, &token).await;
    assert!(acked["acknowledged_at"].is_string());
    let (_, repeated) = check(client.post(&ack), StatusCode::OK, &token).await;
    assert_eq!(repeated["acknowledged_at"], acked["acknowledged_at"]);
    let (_, detail) = check(client.get(&alert), StatusCode::OK, &token).await;
    assert_eq!(detail, acked);
    let (_, unacknowledged) = check(
        client.get(&alerts).query(&[("unacknowledged", "true")]),
        StatusCode::OK,
        &token,
    )
    .await;
    assert_eq!(unacknowledged["items"], oldest["items"]);
    db.close().await;
    drop(server);
}
