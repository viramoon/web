use axum::{Json, Router, routing::put};
use serde_json::{Value, json};

pub(crate) const PATH: &str = "/api/v1/market/subscriptions/demand";

pub(crate) struct DemandStub {
    pub(crate) url: String,
    server: tokio::task::JoinHandle<()>,
}

impl DemandStub {
    pub(crate) async fn enabled() -> Self {
        Self::start(
            Router::new().route(
                PATH,
                put(|Json(body): Json<Value>| async move {
                    Json(subscription_status(&body, "enabled"))
                }),
            ),
        )
        .await
    }

    pub(crate) async fn start(app: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, server }
    }
}

impl Drop for DemandStub {
    fn drop(&mut self) {
        self.server.abort();
    }
}

// Synthetic F15 reply from the submitted fixture IDs and fixed slot counts.
pub(crate) fn subscription_status(request: &Value, state: &str) -> Value {
    let demand = if state == "disabled" {
        json!({"watchlist": [], "depth_focus": null})
    } else {
        request.clone()
    };
    json!({
        "state": state,
        "demand": demand,
        "budget": null,
        "depth_budget": {"pinned_slots": 2, "page_focus_slots": 1, "access": null}
    })
}

pub(crate) fn upstream_error_body() -> Value {
    json!({"error": {"code": "synthetic-fixture-value", "message": "synthetic-fixture-value",
        "path": "/synthetic-fixture-value"}})
}
