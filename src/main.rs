use chrono::Utc;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    sync::watch,
};
use tracing_subscriber::EnvFilter;
use web::{
    AppState,
    config::{ConfigError, Settings},
    credentials::CredentialStore,
    database::Database,
    finance::Finance,
    openapi, router,
};

#[tokio::main]
async fn main() -> Result<(), ConfigError> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args == ["export-openapi"] {
        println!("{}", openapi().to_pretty_json()?);
        return Ok(());
    }
    let publish_inputs = match args.as_slice() {
        [command, inputs] if command == "publish-quality" => Some(Path::new(inputs)),
        [] => None,
        [command] if matches!(command.as_str(), "serve" | "migrate") => None,
        _ => return Err("usage: web [serve|migrate|export-openapi|publish-quality INPUTS]".into()),
    };
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let dir = std::env::var_os("WEB_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config"));
    let s = Settings::load(&dir, |name| std::env::var(name).ok())?;
    if let Some(inputs) = publish_inputs {
        return s.ops.publish_quality(inputs, &s.paths.static_root).await;
    }
    let db = Database::new(
        &s.database.url,
        Duration::from_millis(s.database.acquire_timeout_ms),
        Duration::from_millis(s.database.connect_timeout_ms),
    )?;
    if args == ["migrate"] {
        db.migrate().await?;
        db.close().await;
        return Ok(());
    }
    let finance = Finance::new(&s.finance)?;
    let write_access = web::auth::WriteAccess::load(
        &s.security.admin_token_file,
        s.security.max_requests,
        Duration::from_millis(s.security.window_ms),
    )?
    .with_sessions(s.security.public_origin.as_deref(), s.admin_session_ttl())?;
    let credential_store = s
        .credentials
        .directory
        .as_deref()
        .zip(s.credentials.recipient_public_key_file.as_deref())
        .map(|(dir, key)| -> Result<_, ConfigError> {
            let static_root = &s.paths.static_root;
            let resolved_static = static_root.canonicalize();
            let resolved_dir = dir.canonicalize();
            if dir.starts_with(static_root)
                || resolved_static
                    .ok()
                    .zip(resolved_dir.ok())
                    .is_some_and(|(root, credentials)| credentials.starts_with(root))
            {
                return Err("credential directory must be outside the static root".into());
            }
            Ok(CredentialStore::open(dir, key)?)
        })
        .transpose()?;
    let drain_timeout = s.drain_timeout();
    let listener = TcpListener::bind((s.server.bind_host, s.server.port)).await?;
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let (shutdown, mut rx) = watch::channel(false);
    let monitor = tokio::spawn(db.clone().monitor(s.database.auto_migrate, rx.clone()));
    let state = AppState {
        config: Arc::new(s),
        db: db.clone(),
        finance,
        write_access,
        credential_store,
        started_at: Utc::now(),
    };
    tracing::info!(address=%listener.local_addr()?, "listening");
    let serving = axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            let _ = rx.wait_for(|v| *v).await;
        })
        .into_future();
    tokio::pin!(serving);
    let deadline = tokio::select! {
        result = &mut serving => { result?; tokio::time::Instant::now() + drain_timeout },
        _ = async { tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} } } => {
            shutdown.send_replace(true);
            tracing::info!("draining HTTP connections");
            let deadline = tokio::time::Instant::now() + drain_timeout;
            match tokio::time::timeout_at(deadline, &mut serving).await {
                Ok(result) => result?,
                Err(_) => tracing::warn!("HTTP drain deadline reached"),
            }
            deadline
        }
    };
    shutdown.send_replace(true);
    monitor.abort();
    let _ = monitor.await;
    let _ = tokio::time::timeout_at(deadline, db.close()).await;
    tracing::info!("stopped");
    Ok(())
}
