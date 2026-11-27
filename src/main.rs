use chrono::Utc;
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    sync::watch,
};
use web::{
    AppState, config::Settings, credentials::CredentialStore, database::Database, finance::Finance,
    openapi, router_with_shutdown, telemetry,
};

type Failure = (&'static str, Option<&'static str>);

enum Command<'a> {
    Serve,
    Migrate,
    PublishQuality(&'a Path),
}

fn main() -> ExitCode {
    telemetry::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err((stage, reason)) => {
            if let Some(reason) = reason {
                tracing::error!(stage, reason, "web command failed");
            } else {
                tracing::error!(stage, "web command failed");
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Failure> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args == ["export-openapi"] {
        let mut output = io::stdout().lock();
        // Keep a line-oriented diff while fitting the repository's blob limit.
        let formatter = serde_json::ser::PrettyFormatter::with_indent(b"");
        let mut serializer = serde_json::Serializer::with_formatter(&mut output, formatter);
        serde::Serialize::serialize(&openapi(), &mut serializer)
            .map_err(|_| ("openapi_export", None))?;
        writeln!(output).map_err(|_| ("openapi_export", None))?;
        return Ok(());
    }
    let command = match args.as_slice() {
        [] => Command::Serve,
        [command] if command == "serve" => Command::Serve,
        [command] if command == "migrate" => Command::Migrate,
        [command, inputs] if command == "publish-quality" => {
            Command::PublishQuality(Path::new(inputs))
        }
        _ => {
            eprintln!("usage: web [serve|migrate|export-openapi|publish-quality INPUTS]");
            return Err(("command_line", None));
        }
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|_| ("async_runtime", None))?
        .block_on(execute(command))
}

async fn execute(command: Command<'_>) -> Result<(), Failure> {
    let dir = std::env::var_os("WEB_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config"));
    let s = Settings::load(&dir, |name| std::env::var(name).ok())
        .map_err(|_| ("configuration", None))?;
    if let Command::PublishQuality(inputs) = command {
        return s
            .ops
            .publish_quality(inputs, &s.paths.static_root)
            .await
            .map_err(|reason| ("quality_publish", Some(reason)));
    }
    serve_or_migrate(s, matches!(command, Command::Migrate))
        .await
        .map_err(|stage| (stage, None))
}

async fn serve_or_migrate(s: Settings, migrate: bool) -> Result<(), &'static str> {
    let db = Database::new(
        &s.database.url,
        Duration::from_millis(s.database.acquire_timeout_ms),
        Duration::from_millis(s.database.connect_timeout_ms),
    )
    .map_err(|_| "database_configuration")?;
    if migrate {
        db.migrate().await.map_err(|_| "database_migration")?;
        db.close().await;
        return Ok(());
    }
    let finance = Finance::new(&s.finance).map_err(|_| "finance_client")?;
    let write_access = web::auth::WriteAccess::load(
        &s.security.admin_token_file,
        s.security.max_requests,
        Duration::from_millis(s.security.window_ms),
    )
    .map_err(|_| "admin_token_file")?
    .with_sessions(s.security.public_origin.as_deref(), s.admin_session_ttl())
    .map_err(|_| "admin_session")?;
    let credential_store = s
        .credentials
        .directory
        .as_deref()
        .zip(s.credentials.recipient_public_key_file.as_deref())
        .map(|(dir, key)| {
            let static_root = &s.paths.static_root;
            let resolved_static = static_root.canonicalize();
            let resolved_dir = dir.canonicalize();
            if dir.starts_with(static_root)
                || resolved_static
                    .ok()
                    .zip(resolved_dir.ok())
                    .is_some_and(|(root, credentials)| credentials.starts_with(root))
            {
                return Err("credential_directory");
            }
            CredentialStore::open(dir, key).map_err(|_| "credential_store")
        })
        .transpose()?;
    let drain_timeout = s.drain_timeout();
    let listener = TcpListener::bind((s.server.bind_host, s.server.port))
        .await
        .map_err(|_| "http_listen")?;
    let address = listener.local_addr().map_err(|_| "http_listen")?;
    let mut term = signal(SignalKind::terminate()).map_err(|_| "shutdown_signals")?;
    let mut int = signal(SignalKind::interrupt()).map_err(|_| "shutdown_signals")?;
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
    tracing::info!(%address, "listening");
    let serving = axum::serve(listener, router_with_shutdown(state, rx.clone()))
        .with_graceful_shutdown(async move {
            let _ = rx.wait_for(|v| *v).await;
        })
        .into_future();
    tokio::pin!(serving);
    let deadline = tokio::select! {
        result = &mut serving => {
            result.map_err(|_| "http_serve")?;
            tokio::time::Instant::now() + drain_timeout
        },
        _ = async { tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} } } => {
            shutdown.send_replace(true);
            tracing::info!("draining HTTP connections");
            let deadline = tokio::time::Instant::now() + drain_timeout;
            match tokio::time::timeout_at(deadline, &mut serving).await {
                Ok(result) => result.map_err(|_| "http_serve")?,
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
