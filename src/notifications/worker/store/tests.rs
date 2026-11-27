use super::*;
use crate::test_postgres::Cluster;
use std::time::Duration;

#[tokio::test]
async fn quiet_hours_dispatch_reuses_the_sender_connection_after_unlock() {
    let cluster = Cluster::create();
    let db = Database::new(
        &cluster.url(),
        Duration::from_secs(2),
        Duration::from_secs(2),
    )
    .unwrap();
    db.migrate().await.unwrap();

    // Hold all but one pool slot. A new backend on the last slot means the
    // successful sender discarded its connection.
    let mut busy_connections = Vec::new();
    for _ in 0..4 {
        busy_connections.push(db.connection().await.unwrap());
    }

    let now = "2026-11-27T15:00:00Z".parse::<DateTime<Utc>>().unwrap();
    let id = Uuid::new_v4();
    let mut available = db.connection().await.unwrap();
    sqlx::query(
        "INSERT INTO app.notification_outbox
            (id, kind, priority, test_key, created_at, available_at, updated_at)
         VALUES ($1, 'test', 'normal', $1, $2, $2, $2)",
    )
    .bind(id)
    .bind(now)
    .execute(&mut *available)
    .await
    .unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *available)
        .await
        .unwrap();
    drop(available);

    let (mut sender, job) = claim(&db, &NotificationPolicy::default(), || now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.id, id);
    finish(&mut sender, &job, AttemptOutcome::Accepted, now)
        .await
        .unwrap();
    drop(sender);

    let mut reused = db.connection().await.unwrap();
    let (reused_pid, unlocked): (i32, bool) = sqlx::query_as(
        "SELECT pg_backend_pid(), NOT EXISTS (
            SELECT FROM pg_locks
            WHERE pid = pg_backend_pid() AND locktype = 'advisory' AND granted
        )",
    )
    .fetch_one(&mut *reused)
    .await
    .unwrap();
    assert_eq!(reused_pid, pid);
    assert!(unlocked);
}
