use tracing_subscriber::{EnvFilter, filter::filter_fn, fmt::MakeWriter, prelude::*};

pub fn init() {
    let levels = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    subscriber(levels, std::io::stderr).init();

    // A panic hook runs before CatchPanicLayer, even for a recovered request.
    std::panic::set_hook(Box::new(|_| tracing::error!("process panicked")));
}

fn subscriber(
    levels: EnvFilter,
    writer: impl for<'a> MakeWriter<'a> + Send + Sync + 'static,
) -> impl tracing::Subscriber + Send + Sync {
    let web_events = filter_fn(|metadata| {
        let target = metadata.target();
        matches!(target, "web" | "web_notifier")
            || target.starts_with("web::")
            || target.starts_with("web_notifier::")
    });
    // A dependency can check interest without emitting an event. Global
    // filters keep that check independent of the next event's formatting.
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().json().with_writer(writer))
        .with(levels)
        .with(web_events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use tracing::{Level, callsite::Callsite, metadata::Kind};

    #[test]
    fn sqlx_log_interest_check_preserves_the_next_notifier_event() {
        let output = tempfile::NamedTempFile::new_in("/tmp").unwrap();
        let _subscriber = tracing::subscriber::set_default(subscriber(
            EnvFilter::new("trace"),
            output.reopen().unwrap(),
        ));
        let query = tracing::callsite! {
            name: "query log interest",
            kind: Kind::EVENT,
            target: "sqlx::query",
            level: Level::DEBUG,
            fields: message
        };

        // The log bridge calls enabled directly. SQLx may return without emitting
        // an event, so the interest check must not affect the next log record.
        let query_enabled = tracing::dispatcher::get_default(|d| d.enabled(query.metadata()));
        tracing::warn!(
            target: "web_notifier",
            stage = "smtp_configuration",
            reason = "active SMTP snapshot is unavailable",
            "notifier idle"
        );
        tracing::debug!(target: "sqlx::query", "synthetic dependency log");
        tracing::warn!(target: "web_notifier_extra", "synthetic unrelated log");

        let output = std::fs::read_to_string(output.path()).unwrap();
        let event: Value = serde_json::from_str(&output).expect("expected one notifier log event");
        assert_eq!(event["target"], "web_notifier");
        assert_eq!(event["level"], "WARN");
        assert_eq!(event["fields"]["message"], "notifier idle");
        assert_eq!(event["fields"]["stage"], "smtp_configuration");
        assert_eq!(
            event["fields"]["reason"],
            "active SMTP snapshot is unavailable"
        );
        assert!(!query_enabled);
    }
}
