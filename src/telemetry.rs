use tracing_subscriber::{
    EnvFilter, Layer,
    filter::{FilterExt, filter_fn},
    prelude::*,
};

pub fn init() {
    let levels = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let web_events = filter_fn(|metadata| {
        let target = metadata.target();
        matches!(target, "web" | "web_notifier")
            || target.starts_with("web::")
            || target.starts_with("web_notifier::")
    });
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::io::stderr)
                .with_filter(web_events.and(levels)),
        )
        .init();

    // A panic hook runs before CatchPanicLayer, even for a recovered request.
    std::panic::set_hook(Box::new(|_| tracing::error!("process panicked")));
}
