use crate::notifications::worker::{self, NotificationHealth};
use serde::Serialize;
use sqlx::{PgPool, Postgres, pool::PoolConnection, postgres::PgPoolOptions};
use std::time::{Duration, Instant};
use tokio::{
    sync::watch,
    time::{sleep, timeout},
};
use utoipa::ToSchema;

#[derive(Clone, Default, Serialize, ToSchema)]
pub struct DbStatus {
    pub ok: bool,
    pub latency_ms: Option<u64>,
}

#[derive(Clone)]
pub struct Database {
    pool: PgPool,
    connect_timeout: Duration,
    status: watch::Sender<(DbStatus, NotificationHealth)>,
    #[cfg(test)]
    stall_connections: bool,
}

impl Database {
    /// Parsing config starts no network I/O: liveness is independent of Postgres.
    pub fn new(
        url: &str,
        acquire_timeout: Duration,
        connect_timeout: Duration,
    ) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .min_connections(0)
            .acquire_timeout(acquire_timeout)
            .connect_lazy(url)?;
        Ok(Self {
            pool,
            connect_timeout,
            status: watch::channel(Default::default()).0,
            #[cfg(test)]
            stall_connections: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_stalled_connections(mut self) -> Self {
        self.stall_connections = true;
        self
    }

    pub(crate) async fn connection(&self) -> Result<PoolConnection<Postgres>, sqlx::Error> {
        #[cfg(test)]
        if self.stall_connections {
            return std::future::pending().await;
        }
        // A stale idle connection can be replaced during any acquire, so the
        // handshake deadline applies to warm pools as well as cold startups.
        timeout(self.connect_timeout, self.pool.acquire())
            .await
            .map_err(|_| sqlx::Error::PoolTimedOut)?
    }

    pub(crate) async fn write_connection(&self) -> Result<PoolConnection<Postgres>, sqlx::Error> {
        let mut connection = self.connection().await?;
        // A request can be cancelled while BEGIN is still awaiting acknowledgment.
        // Close this connection on drop so an open transaction cannot reenter the pool.
        connection.close_on_drop();
        Ok(connection)
    }

    pub async fn migrate(&self) -> Result<(), sqlx::migrate::MigrateError> {
        let mut connection = self.connection().await?;
        // A cancellation or migration error must release all session-level migration locks.
        connection.close_on_drop();
        sqlx::migrate!().run_direct(&mut *connection).await
    }

    async fn ready(&self) -> Result<bool, sqlx::Error> {
        let mut connection = self.connection().await?;
        timeout(self.connect_timeout, async {
            let initialized = sqlx::query_scalar!(
                r#"SELECT (version = 1 AND to_regnamespace('blog') IS NOT NULL) AS "ready!"
               FROM app.schema_info WHERE component = 'web'"#
            )
            .fetch_optional(&mut *connection)
            .await?;
            if initialized != Some(true) {
                return Ok(false);
            }
            sqlx::query_scalar(
                "SELECT to_regclass('app.integration_configs') IS NOT NULL
                    AND to_regclass('app.notification_health') IS NOT NULL
                    AND EXISTS (
                        SELECT FROM pg_catalog.pg_attribute
                        WHERE attrelid = to_regclass('app.notification_outbox')
                          AND attname = 'replaces_id' AND NOT attisdropped
                    )",
            )
            .fetch_one(&mut *connection)
            .await
        })
        .await
        .map_err(|_| sqlx::Error::PoolTimedOut)?
    }

    pub fn status(&self) -> DbStatus {
        self.status.borrow().0.clone()
    }

    pub(crate) fn status_snapshot(&self) -> (DbStatus, NotificationHealth) {
        self.status.borrow().clone()
    }

    pub async fn monitor(self, auto_migrate: bool, mut shutdown: watch::Receiver<bool>) {
        let mut delay = Duration::from_millis(250);
        let mut migrate = auto_migrate;
        let mut previous_probe = None;
        loop {
            if *shutdown.borrow() {
                break;
            }
            let start = Instant::now();
            let probe = async {
                if migrate {
                    self.migrate().await.map_err(|_| "migration failed")?;
                }
                match self.ready().await {
                    Ok(true) => Ok(()),
                    Ok(false) => Err("required schemas or migrations missing"),
                    Err(_) => Err("database or schema metadata unavailable"),
                }?;
                let readiness = DbStatus {
                    ok: true,
                    latency_ms: Some(start.elapsed().as_millis() as u64),
                };
                // System status reads this snapshot without a database round trip.
                Ok::<_, &str>((readiness, worker::health(&self).await))
            };
            let result = tokio::select! {
                res = probe => res,
                _ = shutdown.wait_for(|v| *v) => break,
            };
            let (probe, status) = match result {
                Ok(status) => (Ok(()), status),
                Err(reason) => (Err(reason), Default::default()),
            };
            let ok = probe.is_ok();
            if Some(probe) != previous_probe {
                match probe {
                    Ok(()) => tracing::info!("database ready"),
                    Err(reason) => tracing::warn!(reason, "database unavailable"),
                }
                previous_probe = Some(probe);
            }
            self.status.send_replace(status);
            migrate = auto_migrate && !ok;
            let interval = if ok {
                delay = Duration::from_millis(250);
                Duration::from_secs(1)
            } else {
                let interval = delay;
                delay = (delay * 2).min(Duration::from_secs(4));
                interval
            };
            tokio::select! {
                _ = sleep(interval) => {},
                _ = shutdown.wait_for(|v| *v) => break,
            }
        }
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}
