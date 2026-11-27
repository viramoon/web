use std::{future::Future, process::ExitCode, time::Duration};
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::watch,
    time,
};
use web::{
    database::Database,
    notifications::{
        channel::EmailChannels,
        worker::{self, NotificationPolicy},
    },
    telemetry,
};

type Failure = (&'static str, &'static str);

enum Startup {
    Idle(&'static str),
    Ready(Database, NotificationPolicy, EmailChannels),
}

fn main() -> ExitCode {
    telemetry::init();
    match command() {
        Ok(()) => ExitCode::SUCCESS,
        Err((stage, reason)) => {
            tracing::error!(stage, reason, "notifier command failed");
            ExitCode::FAILURE
        }
    }
}

fn command() -> Result<(), Failure> {
    if std::env::args_os().len() != 1 {
        return Err(("command_line", "usage: web-notifier"));
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|_| {
            (
                "async_runtime",
                "notifier could not start the async runtime",
            )
        })?
        .block_on(run())
}

async fn run() -> Result<(), Failure> {
    let signal_error = |_| {
        (
            "shutdown_signals",
            "notifier could not register shutdown signals",
        )
    };
    let mut term = signal(SignalKind::terminate()).map_err(signal_error)?;
    let mut int = signal(SignalKind::interrupt()).map_err(signal_error)?;
    run_with_startup(startup(), async {
        tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
    })
    .await
}

async fn startup() -> Result<Startup, Failure> {
    let get = |key: &str| std::env::var(key).ok();
    if get("FERTILE_ENABLE_WEB_NOTIFIER").as_deref() == Some("0") {
        return Ok(Startup::Idle("notification delivery is disabled"));
    }
    let channel = match EmailChannels::for_notifier(get) {
        Ok(channel) => channel,
        Err(error) => return Ok(Startup::Idle(error.reason())),
    };
    let policy = NotificationPolicy::from_environment(get).map_err(|_| {
        (
            "notification_policy",
            "invalid notifier policy configuration",
        )
    })?;
    let database_url =
        get("DATABASE_URL").ok_or(("database_configuration", "DATABASE_URL is required"))?;
    let db = Database::new(
        &database_url,
        Duration::from_secs(2),
        Duration::from_secs(2),
    )
    .map_err(|_| ("database_configuration", "invalid DATABASE_URL"))?;

    let idle_reason = worker::smtp_idle_reason(&db, &channel).await.map_err(|_| {
        (
            "smtp_configuration",
            "notifier could not read the SMTP configuration from PostgreSQL",
        )
    })?;
    Ok(match idle_reason {
        Some(reason) => Startup::Idle(reason),
        None => Startup::Ready(db, policy, channel),
    })
}

async fn run_with_startup(
    startup: impl Future<Output = Result<Startup, Failure>>,
    stop_signal: impl Future<Output = ()>,
) -> Result<(), Failure> {
    tokio::pin!(stop_signal);
    let startup = tokio::select! {
        result = startup => result?,
        _ = &mut stop_signal => return Ok(()),
    };
    let (db, policy, channel) = match startup {
        Startup::Idle(reason) => {
            tracing::warn!(stage = "smtp_configuration", reason, "notifier idle");
            stop_signal.await;
            return Ok(());
        }
        Startup::Ready(db, policy, channel) => (db, policy, channel),
    };

    let (shutdown, stop) = watch::channel(false);
    let running = worker::run(&db, &policy, &channel, stop);
    let worker_error = |_| ("notification_worker", "notifier worker failed");
    tokio::pin!(running);
    tokio::select! {
        result = &mut running => result.map_err(worker_error)?,
        _ = stop_signal => {
            shutdown.send_replace(true);
            tracing::info!("draining notifier");
            match time::timeout(channel.drain_timeout(), &mut running).await {
                Ok(result) => result.map_err(worker_error)?,
                Err(_) => tracing::warn!("notifier drain deadline reached"),
            }
        }
    }
    let _ = time::timeout(Duration::from_secs(2), db.close()).await;
    tracing::info!("notifier stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn notifier_announces_asynchronous_idle_decision_and_waits_for_shutdown() {
        let stderr = tempfile::NamedTempFile::new_in("/tmp").unwrap();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(stderr.reopen().unwrap())
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);
        let (shutdown, stop) = oneshot::channel();
        let decision = async {
            tokio::task::yield_now().await;
            Ok(Startup::Idle("active SMTP snapshot is unavailable"))
        };
        let running = run_with_startup(decision, async {
            stop.await.unwrap();
        });
        tokio::pin!(running);
        let log = time::timeout(Duration::from_secs(2), async {
            loop {
                let log = std::fs::read_to_string(stderr.path()).unwrap();
                if log.contains('\n') {
                    break log;
                }
                time::sleep(Duration::from_millis(10)).await;
            }
        });
        let log = tokio::select! {
            log = log => log.unwrap(),
            result = &mut running => panic!("notifier stopped while idle: {result:?}"),
        };
        let entry: Value = serde_json::from_str(&log).unwrap();
        assert_eq!(entry["level"], "WARN");
        assert_eq!(entry["fields"]["message"], "notifier idle");
        assert_eq!(entry["fields"]["stage"], "smtp_configuration");
        assert_eq!(
            entry["fields"]["reason"],
            "active SMTP snapshot is unavailable"
        );
        shutdown.send(()).unwrap();
        time::timeout(Duration::from_secs(2), running)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read_to_string(stderr.path()).unwrap(), log);
    }
}
