//! Builds operational events in isolated PostgreSQL and records in-process fake SMTP attempts.

use super::{FakeSmtp, Fixture, RuleEditor, later};
use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, Method, Request, StatusCode, header},
};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;
use web::database::Database;
use web::notifications::{
    channel::{DeliveryOutcome, MailKind},
    model::{AlertSignal, AttemptOutcome, Condition, Phase},
    store::record_signal,
    worker::{NotificationPolicy, dispatch_one},
};

const REASON: &str = "Synthetic maintenance window";
const ORIGIN: &str = "https://web.test";

fn rule_path(rule: Uuid) -> String {
    format!("/api/v1/notification-rules/{rule}/silence")
}

fn alert_path(alert: Uuid) -> String {
    format!("/api/v1/alerts/{alert}/silence")
}

fn request(method: Method, path: &str, payload: Option<Value>) -> Request<Body> {
    let req = Request::builder().method(method).uri(path);
    match payload {
        Some(value) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    }
}

async fn send(
    admin: &RuleEditor,
    request: Request<Body>,
    expected: StatusCode,
) -> (HeaderMap, Value) {
    let response = admin.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), expected);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 65_536).await.unwrap();
    (headers, serde_json::from_slice(&body).unwrap())
}

async fn bearer(
    admin: &RuleEditor,
    method: Method,
    path: &str,
    payload: Option<Value>,
    expected: StatusCode,
) -> Value {
    let mut req = request(method, path, payload);
    req.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {}", admin.token).parse().unwrap(),
    );
    send(admin, req, expected).await.1
}

async fn set(admin: &RuleEditor, path: &str, until: DateTime<Utc>) -> Value {
    bearer(
        admin,
        Method::PUT,
        path,
        Some(json!({"reason": REASON, "until": until})),
        StatusCode::OK,
    )
    .await
}

async fn release(admin: &RuleEditor, path: &str) -> Value {
    bearer(admin, Method::DELETE, path, None, StatusCode::OK).await
}

async fn next(fixture: &Fixture, mail: &FakeSmtp, now: DateTime<Utc>) -> bool {
    let policy = NotificationPolicy::from_environment(|key| match key {
        "WEB_NOTIFIER_QUIET_START" | "WEB_NOTIFIER_QUIET_END" => Some("00:00".into()),
        "WEB_NOTIFIER_COOLDOWN_SECONDS" => Some("0".into()),
        _ => None,
    })
    .unwrap();
    dispatch_one(&fixture.db, &policy, mail, || now)
        .await
        .unwrap()
}

async fn delivery(fixture: &Fixture, alert: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT id FROM app.notification_outbox WHERE event_id = $1")
        .bind(alert)
        .fetch_one(&fixture.pool)
        .await
        .unwrap()
}

async fn session(admin: &RuleEditor) -> (String, String) {
    let mut login = request(Method::POST, "/api/v1/admin/session", None);
    login
        .headers_mut()
        .insert(header::ORIGIN, ORIGIN.parse().unwrap());
    login.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {}", admin.token).parse().unwrap(),
    );
    let response = admin.app.clone().oneshot(login).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65_536).await.unwrap()).unwrap();
    (cookie, body["csrf_token"].as_str().unwrap().to_owned())
}

#[tokio::test]
async fn postgres_rule_silence_records_events_and_summarizes_after_release() {
    let fixture = Fixture::create().await;
    let now = Utc::now();
    let rule = fixture.rule(false).await;
    let other_rule = fixture.rule(false).await;
    let admin = RuleEditor::new(&fixture.db, now);
    let mail = FakeSmtp::default();
    let path = rule_path(rule);
    assert_eq!(set(&admin, &path, later(now, 3600)).await["active"], true);

    let mut deliveries = Vec::new();
    for (id, rule_id) in [(1, rule), (2, rule), (3, other_rule)] {
        let signal = AlertSignal {
            condition: Condition::SourceUnavailable,
            incident_key: Uuid::from_u128(id),
            window_start: now,
            observed_at: now,
            phase: Phase::Triggered,
        };
        let event = record_signal(&fixture.db, rule_id, signal).await.unwrap();
        let retry = record_signal(&fixture.db, rule_id, signal).await.unwrap();
        assert!(retry.duplicate);
        assert_eq!(retry.id, event.id);
        let read = admin
            .get_json(&format!("/api/v1/alerts/{}", event.id))
            .await;
        assert_eq!(read["duplicate_count"], 1);
        assert_eq!(read["deliveries"][0]["status"], "pending");
        deliveries.push(delivery(&fixture, event.id).await);
    }
    let before_release = Utc::now();
    assert!(next(&fixture, &mail, before_release).await);
    assert!(!next(&fixture, &mail, before_release).await);
    assert_eq!(mail.sent.lock().unwrap()[0].0, deliveries[2]);
    assert!(fixture.attempts(deliveries[0]).await.is_empty());
    assert!(fixture.attempts(deliveries[1]).await.is_empty());

    assert_eq!(release(&admin, &path).await["active"], false);
    let after_release = Utc::now();
    assert!(next(&fixture, &mail, after_release).await);
    assert!(!next(&fixture, &mail, after_release).await);
    let sent = mail.sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert!(deliveries[..2].contains(&sent[1].0));
    assert_eq!(sent[1].1, Some(MailKind::Digest { count: 2 }));
}

#[tokio::test]
async fn postgres_alert_silence_is_scoped_and_expires_at_deadline() {
    let fixture = Fixture::create().await;
    let now = Utc::now();
    let until = DateTime::from_timestamp_micros(later(now, 3600).timestamp_micros()).unwrap();
    let rule = fixture.rule(true).await;
    let admin = RuleEditor::new(&fixture.db, now);
    let mail = FakeSmtp::default();
    let target = fixture
        .queue_event(rule, Uuid::from_u128(1), "alert", now)
        .await;
    let sibling = fixture
        .queue_event(rule, Uuid::from_u128(1), "alert", later(now, 1))
        .await;
    let event_id = sqlx::query_scalar("SELECT event_id FROM app.notification_outbox WHERE id = $1")
        .bind(target)
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
    let path = alert_path(event_id);
    assert_eq!(set(&admin, &path, until).await["active"], true);
    assert_eq!(admin.get_json(&rule_path(rule)).await["active"], false);

    let just_before = until - TimeDelta::microseconds(1);
    assert!(next(&fixture, &mail, just_before).await);
    assert!(!next(&fixture, &mail, just_before).await);
    assert!(fixture.attempts(target).await.is_empty());
    assert_eq!(fixture.attempts(sibling).await, [AttemptOutcome::Accepted]);
    assert!(next(&fixture, &mail, until).await);
    assert_eq!(fixture.attempts(target).await, [AttemptOutcome::Accepted]);

    // Move the synthetic interval into the database clock's past to verify
    // readback and repeat DELETE without sleeping or running an expiry job.
    sqlx::query(
        "UPDATE app.notification_silences
         SET started_at = clock_timestamp() - interval '2 hours',
             until_at = clock_timestamp() - interval '1 hour' WHERE alert_id = $1",
    )
    .bind(event_id)
    .execute(&fixture.pool)
    .await
    .unwrap();
    let expired = admin.get_json(&path).await;
    assert_eq!(expired["active"], false);
    assert!(expired["silence"]["ended_at"].is_null());
    assert_eq!(release(&admin, &path).await, expired);
}

#[tokio::test]
async fn postgres_overlapping_silences_hold_delivery_until_both_clear() {
    let fixture = Fixture::create().await;
    let now = Utc::now();
    let rule = fixture.rule(true).await;
    let target = fixture
        .queue_event(rule, Uuid::from_u128(1), "alert", now)
        .await;
    let event_id = sqlx::query_scalar("SELECT event_id FROM app.notification_outbox WHERE id = $1")
        .bind(target)
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
    let admin = RuleEditor::new(&fixture.db, now);
    let mail = FakeSmtp::default();
    let rule_path = rule_path(rule);
    let alert_path = alert_path(event_id);
    set(&admin, &rule_path, later(now, 3600)).await;
    set(&admin, &alert_path, later(now, 3600)).await;
    assert!(!next(&fixture, &mail, Utc::now()).await);
    release(&admin, &alert_path).await;
    assert!(!next(&fixture, &mail, Utc::now()).await);
    set(&admin, &alert_path, later(now, 3600)).await;
    release(&admin, &rule_path).await;
    assert!(!next(&fixture, &mail, Utc::now()).await);
    release(&admin, &alert_path).await;
    assert!(next(&fixture, &mail, Utc::now()).await);
    assert_eq!(fixture.attempts(target).await, [AttemptOutcome::Accepted]);
}

#[tokio::test]
async fn postgres_summary_retry_excludes_manually_silenced_alert() {
    let fixture = Fixture::create().await;
    let now = Utc::now();
    let rule = fixture.rule(false).await;
    let primary = fixture
        .queue_event(rule, Uuid::from_u128(1), "alert", now)
        .await;
    let member = fixture
        .queue_event(rule, Uuid::from_u128(2), "alert", later(now, 1))
        .await;
    let admin = RuleEditor::new(&fixture.db, now);
    let mail = FakeSmtp::with_outcomes([DeliveryOutcome::Timeout, DeliveryOutcome::Accepted]);
    sqlx::query("UPDATE app.notification_outbox SET defer_reason = 'quiet_hours'")
        .execute(&fixture.pool)
        .await
        .unwrap();
    assert!(next(&fixture, &mail, later(now, 5)).await);
    assert_eq!(
        mail.sent.lock().unwrap()[0],
        (primary, Some(MailKind::Digest { count: 2 })),
    );

    let member_alert =
        sqlx::query_scalar("SELECT event_id FROM app.notification_outbox WHERE id = $1")
            .bind(member)
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    let path = alert_path(member_alert);
    set(&admin, &path, later(now, 3600)).await;
    assert!(!next(&fixture, &mail, later(now, 34)).await);
    assert!(next(&fixture, &mail, later(now, 35)).await);
    assert!(next(&fixture, &mail, later(now, 35)).await);
    assert!(!next(&fixture, &mail, later(now, 35)).await);
    let replacement = |original| {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM app.notification_outbox WHERE replaces_id = $1",
        )
        .bind(original)
        .fetch_one(&fixture.pool)
    };
    let primary_replacement = replacement(primary).await.unwrap();
    assert_eq!(
        mail.sent.lock().unwrap()[1],
        (
            primary_replacement,
            Some(MailKind::Alert {
                condition: Condition::SourceUnavailable
            }),
        ),
    );
    let member_replacement = replacement(member).await.unwrap();
    assert!(fixture.attempts(member_replacement).await.is_empty());
    assert_eq!(fixture.attempts(primary).await, [AttemptOutcome::Timeout]);
    assert!(fixture.attempts(member).await.is_empty());
    assert_eq!(
        mail.sent.lock().unwrap()[2].1,
        Some(MailKind::TransportRecovered { count: 1 }),
    );

    release(&admin, &path).await;
    assert!(next(&fixture, &mail, later(now, 40).max(Utc::now())).await);
    assert_eq!(mail.sent.lock().unwrap()[3].0, member_replacement);
}

#[tokio::test]
async fn silence_routes_require_admin_and_csrf_before_database() {
    let local = tempfile::tempdir_in("/tmp").unwrap();
    let db = Database::new(
        &format!(
            "postgresql://web_test@localhost/web_test?host={}",
            local.path().display()
        ),
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .unwrap();
    let now = Utc::now();
    let admin = RuleEditor::new(&db, now);
    let (cookie, _) = session(&admin).await;
    for path in [
        rule_path(Uuid::from_u128(1)),
        alert_path(Uuid::from_u128(2)),
    ] {
        for method in [Method::GET, Method::HEAD, Method::PUT, Method::DELETE] {
            let response = admin
                .app
                .clone()
                .oneshot(request(method, &path, None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
        for method in [Method::PUT, Method::DELETE] {
            let mut no_csrf = request(method, &path, None);
            no_csrf
                .headers_mut()
                .insert(header::COOKIE, cookie.parse().unwrap());
            no_csrf
                .headers_mut()
                .insert(header::ORIGIN, ORIGIN.parse().unwrap());
            send(&admin, no_csrf, StatusCode::FORBIDDEN).await;
        }
    }
}

#[tokio::test]
async fn postgres_silence_http_admin_writes_preserve_audit() {
    let fixture = Fixture::create().await;
    let now = Utc::now();
    let admin = RuleEditor::new(&fixture.db, now);
    let rule = fixture.rule(false).await;
    let path = rule_path(rule);
    let alert = alert_path(Uuid::from_u128(1));
    let initial = admin.get_json(&path).await;
    assert_eq!(initial, json!({"active": false, "silence": null}));
    bearer(&admin, Method::GET, &alert, None, StatusCode::NOT_FOUND).await;
    let until = later(now, 3600);
    for payload in [
        json!({"reason": REASON, "until": now}),
        json!({"reason": "   ", "until": until}),
        json!({"reason": REASON, "until": until, "started_by": "anonymous"}),
    ] {
        bearer(
            &admin,
            Method::PUT,
            &path,
            Some(payload),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }

    assert_eq!(admin.get_json(&path).await, initial);
    let (cookie, csrf) = session(&admin).await;
    let first = set(&admin, &path, until).await;
    assert_eq!(first["silence"]["started_by"], "admin");
    assert_eq!(first["silence"]["reason"], REASON);
    assert_eq!(set(&admin, &path, until).await, first);
    let replaced = set(&admin, &path, later(until, 300)).await;
    assert_ne!(replaced["silence"]["until"], first["silence"]["until"]);
    assert_eq!(admin.get_json(&path).await, replaced);

    let mut delete = request(Method::DELETE, &path, None);
    delete
        .headers_mut()
        .insert(header::COOKIE, cookie.parse().unwrap());
    delete
        .headers_mut()
        .insert(header::ORIGIN, ORIGIN.parse().unwrap());
    delete
        .headers_mut()
        .insert("x-csrf-token", csrf.parse().unwrap());
    let (_, released) = send(&admin, delete, StatusCode::OK).await;
    assert_eq!(released["active"], false);
    assert_eq!(released["silence"]["ended_by"], "admin");
    assert_eq!(released["silence"]["end_action"], "released");
    assert_eq!(released["silence"]["until"], replaced["silence"]["until"]);
    assert_eq!(release(&admin, &path).await, released);

    let rows: Vec<(String, String, DateTime<Utc>, String, String)> = sqlx::query_as(
        "SELECT started_by, reason, until_at, ended_by, end_action
         FROM app.notification_silences WHERE rule_id = $1 ORDER BY sequence",
    )
    .bind(rule)
    .fetch_all(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            (
                "admin".into(),
                REASON.into(),
                first["silence"]["until"].as_str().unwrap().parse().unwrap(),
                "admin".into(),
                "replaced".into()
            ),
            (
                "admin".into(),
                REASON.into(),
                released["silence"]["until"]
                    .as_str()
                    .unwrap()
                    .parse()
                    .unwrap(),
                "admin".into(),
                "released".into()
            )
        ],
    );
}

#[test]
fn openapi_registers_private_silence_operations() {
    let spec = serde_json::to_value(web::openapi()).unwrap();
    for (path, scope) in [
        ("/api/v1/notification-rules/{id}/silence", "rule"),
        ("/api/v1/alerts/{id}/silence", "alert"),
    ] {
        let operations = &spec["paths"][path];
        for method in ["get", "put", "delete"] {
            let operation = &operations[method];
            assert_eq!(
                operation["operationId"],
                format!("{method}_{scope}_silence")
            );
            assert_eq!(
                operation["security"],
                json!([{"adminBearer": []}, {"adminSession": []}])
            );
            assert_eq!(
                operation["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
                "#/components/schemas/SilenceStatus",
            );
        }
        assert_eq!(
            operations["put"]["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/SilenceRequest",
        );
        assert!(operations["delete"].get("requestBody").is_none());
    }
}
