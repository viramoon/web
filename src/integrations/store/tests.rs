use super::*;
use serde_json::json;
use std::time::Duration;

#[path = "../../../tests/api/postgres.rs"]
mod postgres;

fn new_reference() -> CredentialRef {
    CredentialRef {
        reference: Uuid::new_v4(),
        mask: "********".into(),
        format: "jwe".into(),
    }
}

fn test_id(run: &TestInput) -> Uuid {
    Uuid::parse_str(&run.test.id).unwrap()
}

fn fixture(outcome: Outcome) -> (Outcome, TestEvidence) {
    let evidence = match outcome {
        Outcome::Passed => AttemptOutcome::Accepted,
        Outcome::Failed => AttemptOutcome::Rejected,
        Outcome::Unavailable => AttemptOutcome::ConfigurationError,
    };
    (outcome, TestEvidence::Smtp(evidence))
}

#[tokio::test]
async fn postgres_rotation_preserves_active_and_supersedes_stale_attempts() {
    let pg = postgres::Cluster::create();
    let db = Database::new(&pg.url(), Duration::from_secs(2), Duration::from_secs(2)).unwrap();
    db.migrate().await.unwrap();
    let settings = json!({"symbol": "SPX"});
    let created = create(&db, "licensed_options", settings.clone())
        .await
        .unwrap();
    let id = Uuid::parse_str(&created.id).unwrap();
    let first = set_credential(&db, id, created.revision, new_reference(), async { Ok(()) })
        .await
        .unwrap();
    let run = begin_test(&db, id, first.revision).await.unwrap();
    let pending = get(&db, id).await.unwrap();
    assert!(matches!(pending.latest.state, VersionState::Candidate));
    assert!(pending.active.is_none());
    assert_eq!(pending.last_test_id.as_deref(), Some(run.test.id.as_str()));
    let passed = finish_test(&db, id, test_id(&run), fixture(Outcome::Passed))
        .await
        .unwrap();
    assert!(matches!(passed.status, TestStatus::Passed));
    let tested = get(&db, id).await.unwrap();
    assert!(tested.active.is_none());
    let active = activate(&db, id, tested.revision, test_id(&run), false)
        .await
        .unwrap();
    let active_version = active.active.unwrap().version;
    assert_eq!(active_version, first.latest.version);

    let staged = set_credential(&db, id, active.revision, new_reference(), async { Ok(()) })
        .await
        .unwrap();
    assert!(matches!(staged.latest.state, VersionState::Draft));
    let run = begin_test(&db, id, staged.revision).await.unwrap();
    let failed = finish_test(&db, id, test_id(&run), fixture(Outcome::Failed))
        .await
        .unwrap();
    assert!(matches!(failed.status, TestStatus::Failed));
    let after_failure = get(&db, id).await.unwrap();
    let previous = after_failure.active.unwrap();
    assert_eq!(previous.version, active_version);
    assert_eq!(previous.credential.unwrap().version, first.latest.version);
    assert_eq!(
        after_failure.latest.credential.unwrap().version,
        staged.latest.version
    );

    let stale = begin_test(&db, id, after_failure.revision).await.unwrap();
    let pending = get(&db, id).await.unwrap();
    let retry = begin_test(&db, id, pending.revision).await.unwrap();
    let stale_result = finish_test(&db, id, test_id(&stale), fixture(Outcome::Passed))
        .await
        .unwrap();
    assert!(matches!(stale_result.status, TestStatus::Superseded));
    let before_retry = get(&db, id).await.unwrap();
    assert_eq!(before_retry.active.unwrap().version, active_version);
    assert_eq!(
        before_retry.last_test_id.as_deref(),
        Some(retry.test.id.as_str())
    );
    let retry_result = finish_test(&db, id, test_id(&retry), fixture(Outcome::Passed))
        .await
        .unwrap();
    assert!(matches!(retry_result.status, TestStatus::Passed));
    let tested = get(&db, id).await.unwrap();
    assert_eq!(tested.active.unwrap().version, active_version);
    let rotated = activate(&db, id, tested.revision, test_id(&retry), false)
        .await
        .unwrap();
    assert_eq!(rotated.active.unwrap().version, staged.latest.version);

    let (left, right) = tokio::join!(
        change(
            &db,
            id,
            rotated.revision,
            Change::Settings(settings.clone())
        ),
        change(&db, id, rotated.revision, Change::Settings(settings)),
    );
    let edited = match (left, right) {
        (Ok(value), Err(Error::Conflict)) | (Err(Error::Conflict), Ok(value)) => value,
        _ => panic!("expected one committed edit and one conflicting edit"),
    };
    let revoke = change(&db, id, edited.revision, Change::RevokeCredential)
        .await
        .unwrap();
    assert!(revoke.active.is_none());
    assert!(revoke.latest.credential.is_none());
    assert!(revoke.rollback_versions.is_empty());
    assert!(matches!(
        rollback(&db, id, revoke.revision, active_version, false).await,
        Err(Error::Conflict)
    ));
    let audit = postgres::checked(pg.client("psql").args([
        "-XAt",
        "-d",
        "web_test_db",
        "-c",
        "SELECT action FROM app.integration_audit ORDER BY sequence",
    ]));
    assert!(audit.lines().any(|action| action == "test_superseded"));
    assert_eq!(
        audit
            .lines()
            .filter(|action| *action == "test_passed")
            .count(),
        2
    );
    assert_eq!(audit.lines().last(), Some("credential_revoked"));
    db.close().await;
    pg.stop();
}
