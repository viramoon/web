use std::{process::ExitCode, time::Duration};
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
    let get = |key: &str| std::env::var(key).ok();
    let signal_error = |_| {
        (
            "shutdown_signals",
            "notifier could not register shutdown signals",
        )
    };
    let mut term = signal(SignalKind::terminate()).map_err(signal_error)?;
    let mut int = signal(SignalKind::interrupt()).map_err(signal_error)?;
    let stop_signal = async {
        tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
    };

    let channel = if get("FERTILE_ENABLE_WEB_NOTIFIER").as_deref() == Some("0") {
        Err("notification delivery is disabled")
    } else {
        EmailChannels::for_notifier(get).map_err(|error| error.reason())
    };
    let channel = match channel {
        Ok(channel) => channel,
        Err(reason) => {
            tracing::warn!(stage = "smtp_configuration", reason, "notifier idle");
            stop_signal.await;
            return Ok(());
        }
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
