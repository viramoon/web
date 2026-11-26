use std::time::Duration;
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::watch,
    time,
};
use tracing_subscriber::EnvFilter;
use web::{
    config::ConfigError,
    database::Database,
    notifications::{
        channel::EmailChannels,
        worker::{self, NotificationPolicy},
    },
};

#[tokio::main]
async fn main() -> Result<(), ConfigError> {
    if std::env::args_os().len() != 1 {
        return Err("usage: web-notifier".into());
    }
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let get = |key: &str| std::env::var(key).ok();
    let policy = NotificationPolicy::from_environment(get)?;
    let channel = EmailChannels::from_environment(get)?;
    let database_url = get("DATABASE_URL").ok_or("DATABASE_URL is required")?;
    let db = Database::new(
        &database_url,
        Duration::from_secs(2),
        Duration::from_secs(2),
    )
    .map_err(|_| "invalid DATABASE_URL")?;

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let (shutdown, stop) = watch::channel(false);
    let running = worker::run(&db, &policy, &channel, stop);
    tokio::pin!(running);
    tokio::select! {
        result = &mut running => result?,
        _ = async { tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} } } => {
            shutdown.send_replace(true);
            tracing::info!("draining notifier");
            match time::timeout(channel.drain_timeout(), &mut running).await {
                Ok(result) => result?,
                Err(_) => tracing::warn!("notifier drain deadline reached"),
            }
        }
    }
    let _ = time::timeout(Duration::from_secs(2), db.close()).await;
    tracing::info!("notifier stopped");
    Ok(())
}
