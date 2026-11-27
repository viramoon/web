use super::*;
use crate::{auth::WriteAccess, config::Settings, database::Database, finance::Finance, router};
use axum::{
    body::{Body, to_bytes},
    extract::Request,
    http::{HeaderMap, HeaderValue, Method, Uri, header, request::Builder},
};
use std::{
    collections::HashMap,
    fs, future,
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt;
use uuid::Uuid;

const JOBS: &str = "/api/v1/backtests";
const KEY_LOOKUP: &str = "/api/v1/backtests/by-key";
const ORIGIN: &str = "https://web.example.test";
const SYNTHETIC_ID: &str = "synthetic.momentum";
const VERSION_URL: &str = "/api/v1/strategies/synthetic.momentum/versions/1.0.0";
const FACTOR_REPORT_URL: &str = "/api/v1/factors/synthetic.factor/evaluations/fit";
const START: &str = "2026-01-02T00:00:00Z";
const END: &str = "2026-01-06T00:00:00Z";
const PRECISE: &str = "0.1234567890123456789012345678900000";
const SOURCE: &str = "# Synthetic HTTP sample, with no market data\nmetadata:\n  id: synthetic.momentum\n  version: '1.0.0'\n";

struct Gateway {
    app: Router,
    bearer: HeaderValue,
    _directory: tempfile::TempDir,
    finance_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        if let Some(task) = &self.finance_task {
            task.abort();
        }
    }
}

impl Gateway {
    async fn new(stub: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, stub).await.unwrap() });
        let mut gateway = Self::with_finance_url(format!("http://{addr}"));
        gateway.finance_task = Some(task);
        gateway
    }

    fn with_finance_url(finance_url: String) -> Self {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut token_file = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let token = Uuid::new_v4();
        writeln!(token_file, "WEB_ADMIN_TOKEN={token}").unwrap();
        let mut config = Settings::load(Path::new("config"), |_| None).unwrap();
        config.paths.static_root = directory.path().into();
        fs::write(
            directory.path().join("index.html"),
            "<main>synthetic web</main>",
        )
        .unwrap();
        config.database.url = format!(
            "postgresql://localhost/web_research_test?host={}",
            directory.path().display()
        );
        config.finance.base_url = finance_url;
        config.finance.request_timeout_ms = 300;
        let timeout = Duration::from_secs(1);
        let state = AppState {
            db: Database::new(&config.database.url, timeout, timeout).unwrap(),
            finance: Finance::new(&config.finance).unwrap(),
            write_access: WriteAccess::load(token_file.path(), 64, timeout)
                .unwrap()
                .with_sessions(Some(ORIGIN), Duration::from_secs(120))
                .unwrap(),
            config: Arc::new(config),
            credential_store: None,
            started_at: chrono::Utc::now(),
        };
        let mut bearer = HeaderValue::from_str(&format!("Bearer {token}")).unwrap();
        bearer.set_sensitive(true);
        Self {
            app: router(state),
            bearer,
            _directory: directory,
            finance_task: None,
        }
    }

    fn admin(&self, builder: Builder) -> Builder {
        builder.header(header::AUTHORIZATION, self.bearer.clone())
    }

    async fn send(&self, builder: Builder, body: impl Into<Body>) -> Response {
        self.app
            .clone()
            .oneshot(builder.body(body.into()).unwrap())
            .await
            .unwrap()
    }

    async fn json(&self, builder: Builder, body: Value, status: StatusCode) -> Value {
        let response = self
            .send(
                builder.header(header::CONTENT_TYPE, "application/json"),
                body.to_string(),
            )
            .await;
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let actual = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        assert_eq!(actual, status, "{}", String::from_utf8_lossy(&bytes));
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn get(&self, path: &str) -> Value {
        self.json(self.admin(Request::get(path)), Value::Null, StatusCode::OK)
            .await
    }
}

fn document_body(content: &str) -> Value {
    json!({"format":"yaml", "content":content})
}

fn registered() -> Value {
    json!({"id":SYNTHETIC_ID, "version":"1.0.0", "content_sha256":"1".repeat(64)})
}

fn selection() -> Value {
    json!({"kind":"registered", "id":SYNTHETIC_ID, "version":"1.0.0"})
}

fn inline(content: &str) -> Value {
    json!({"kind":"inline", "format":"yaml", "content":content})
}

fn submission(source: Value) -> Value {
    json!({"strategy":"StrategyBundle", "start":START, "end":END, "source":source})
}

fn assert_finance_headers(headers: &HeaderMap) {
    for name in [
        header::AUTHORIZATION.as_str(),
        header::COOKIE.as_str(),
        "origin",
        "x-csrf-token",
    ] {
        assert!(!headers.contains_key(name), "{name} reached finance");
    }
}

fn preview() -> Value {
    json!({
        "strategy": registered(),
        "plan": {
            "plan_version":"1.0", "content_sha256":"1".repeat(64), "source_sha256":"2".repeat(64),
            "bundle": {"metadata": {"id": SYNTHETIC_ID, "version":"1.0.0"}},
            "inputs": {
                "bars": {"id":"example.bars@1.0.0", "asset":"cash_equity",
                    "available_at":"session_close", "calendar":"XNYS", "currency":"USD",
                    "timeframe":"1d", "price_basis":"raw_split_free_reviewed"},
                "universe": {"id":"example.universe@1.0.0", "asset":"cash_equity",
                    "available_at":"session_open", "calendar":"XNYS", "currency":"USD", "point_in_time":true}
            },
            "factors":[], "modules":[], "execution":"execution.next_close_cash_equity@1.0.0",
            "sleeves":[{"recipe":"A1_B2", "priority":10, "budget_weight":"0.250000000000000001"}],
            "conflict_rules": {"decision_snapshot":"frozen", "phase_order":[],
                "on_entry_exit_collision":"exit_first", "same_session_reentry":false,
                "on_missing_execution_price":"defer_and_report", "missing_input":"block_new_risk",
                "target_constraints_order":[], "position_change_order":[], "priority":"ascending",
                "equal_priority":"pro_rata", "whole_lot_remainder_order":[],
                "duplicate_intents":"reject", "sleeve_accounting":"per_recipe"}
        }
    })
}

#[tokio::test]
async fn upload_and_registered_version_return_the_same_plan() {
    let binding = Arc::new(Mutex::new(false));
    let read_binding = Arc::clone(&binding);
    let stub = Router::new()
        .route(
            VALIDATE,
            post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_finance_headers(&headers);
                assert_eq!(body, document_body(SOURCE));
                Json(preview())
            }),
        )
        .route(
            STRATEGIES,
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let binding = Arc::clone(&binding);
                async move {
                    assert_finance_headers(&headers);
                    assert_eq!(body, document_body(SOURCE));
                    *binding.lock().unwrap() = true;
                    Json(registered())
                }
            })
            .get(|uri: Uri| async move {
                assert_eq!(
                    uri.query(),
                    Some("id=synthetic.momentum&after=synthetic.momentum%400.9.0&limit=1")
                );
                Json(json!({"items":[registered()], "next_cursor":null}))
            }),
        )
        .route(
            VERSION,
            get(move || {
                let binding = Arc::clone(&read_binding);
                async move {
                    if *binding.lock().unwrap() {
                        Json(preview()).into_response()
                    } else {
                        api_error(
                            StatusCode::NOT_FOUND,
                            "strategy_version_not_found",
                            "No registered strategy version",
                        )
                    }
                }
            }),
        );
    let web = Gateway::new(stub).await;
    let upload = web
        .json(
            web.admin(Request::post(VALIDATE)),
            document_body(SOURCE),
            StatusCode::OK,
        )
        .await;
    web.json(
        web.admin(Request::get(VERSION_URL)),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    for _ in 0..2 {
        let binding = web
            .json(
                web.admin(Request::post(STRATEGIES)),
                document_body(SOURCE),
                StatusCode::OK,
            )
            .await;
        assert_eq!(binding, upload["strategy"]);
    }
    let listed = web
        .get("/api/v1/strategies?id=synthetic.momentum&after=synthetic.momentum%400.9.0&limit=1")
        .await;
    assert_eq!(listed["items"], json!([upload["strategy"]]));
    let selected = web.get(VERSION_URL).await;
    assert_eq!(upload, selected);
    assert_eq!(selected, preview());
}

#[tokio::test]
async fn strategy_diagnostic_and_document_limit_cover_every_upload_path() {
    let diagnostic = json!({"error":{"code":"UNKNOWN_FIELD", "message":"Unknown field",
        "path":"/spec/<key>", "location":{"line":3,"column":5}}});
    let upstream = diagnostic.clone();
    let compiler = Router::new()
        .route(
            VALIDATE,
            post({
                let upstream = upstream.clone();
                move |Json(doc): Json<Value>| {
                    let upstream = upstream.clone();
                    async move {
                        assert!(doc["content"].as_str().unwrap().len() <= MAX_DOCUMENT_BYTES);
                        (StatusCode::UNPROCESSABLE_ENTITY, Json(upstream))
                    }
                }
            }),
        )
        .route(
            STRATEGIES,
            post({
                let upstream = upstream.clone();
                move |Json(doc): Json<Value>| {
                    let upstream = upstream.clone();
                    async move {
                        assert_eq!(doc, document_body("invalid"));
                        (StatusCode::UNPROCESSABLE_ENTITY, Json(upstream))
                    }
                }
            }),
        )
        .route(
            JOBS,
            post(move |Json(doc): Json<Value>| {
                let upstream = upstream.clone();
                async move {
                    assert_eq!(doc, submission(inline("invalid")));
                    (StatusCode::UNPROCESSABLE_ENTITY, Json(upstream))
                }
            }),
        );
    let web = Gateway::new(compiler).await;
    for path in [VALIDATE, STRATEGIES, JOBS] {
        let request = || {
            web.admin(Request::post(path))
                .header("Idempotency-Key", "synthetic-check")
        };
        let body = |content: &str| {
            if path == JOBS {
                submission(inline(content))
            } else {
                document_body(content)
            }
        };
        assert_eq!(
            web.json(request(), body("invalid"), StatusCode::UNPROCESSABLE_ENTITY)
                .await,
            diagnostic
        );

        // U+754C is three UTF-8 bytes. Character count alone would miss this limit.
        let oversized = "界".repeat(MAX_DOCUMENT_BYTES / 3 + 1);
        let rejected = web
            .json(request(), body(&oversized), StatusCode::PAYLOAD_TOO_LARGE)
            .await;
        assert_eq!(rejected["error"]["code"], "body_too_large");
        assert!(!rejected.to_string().contains('界'));
    }
    let boundary = "x".repeat(MAX_DOCUMENT_BYTES);
    let at_limit = web
        .json(
            web.admin(Request::post(VALIDATE)),
            document_body(&boundary),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    assert_eq!(at_limit, diagnostic);

    let too_big_envelope = " ".repeat(1 << 20);
    let response = web
        .send(
            web.admin(Request::post(VALIDATE))
                .header(header::CONTENT_TYPE, "application/json"),
            format!("{too_big_envelope}{{}}"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");

    let mixed = submission(
        json!({"kind":"registered", "id":SYNTHETIC_ID, "version":"1.0.0", "content":"unexpected"}),
    );
    let rejected = web
        .json(
            web.admin(Request::post(JOBS))
                .header("Idempotency-Key", "synthetic-check"),
            mixed,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    assert_eq!(rejected["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn research_requires_administrator_and_session_csrf_before_proxying() {
    let stub = Router::new().route(
        VALIDATE,
        post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_finance_headers(&headers);
            assert_eq!(body, document_body(SOURCE));
            Json(preview())
        }),
    );
    let web = Gateway::new(stub).await;
    for path in [
        STRATEGIES,
        VERSION_URL,
        FACTORS,
        "/api/v1/factors/synthetic.factor/evaluations",
        "/api/v1/factors/synthetic.factor/evaluations/fit?version=1.0.0",
    ] {
        for method in [Method::GET, Method::HEAD] {
            let response = web
                .send(Request::builder().method(method).uri(path), Body::empty())
                .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
    }
    for path in [STRATEGIES, VALIDATE] {
        web.json(Request::post(path), Value::Null, StatusCode::UNAUTHORIZED)
            .await;
    }
    let login = web
        .send(
            web.admin(Request::post("/api/v1/admin/session"))
                .header(header::ORIGIN, ORIGIN),
            Body::empty(),
        )
        .await;
    assert_eq!(login.status(), StatusCode::CREATED);
    let cookie = login.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let csrf =
        serde_json::from_slice::<Value>(&to_bytes(login.into_body(), 4096).await.unwrap()).unwrap();
    let csrf = csrf["csrf_token"].as_str().unwrap();
    let write = || {
        Request::post(VALIDATE)
            .header(header::COOKIE, &cookie)
            .header(header::ORIGIN, ORIGIN)
    };
    let denied = web.json(write(), Value::Null, StatusCode::FORBIDDEN).await;
    assert_eq!(denied["error"]["code"], "csrf_rejected");
    let validated = web
        .json(
            write().header("x-csrf-token", csrf),
            document_body(SOURCE),
            StatusCode::OK,
        )
        .await;
    assert_eq!(validated, preview());
    let features = web.get("/api/v1/me/capabilities").await;
    assert!(
        features["products"]
            .as_array()
            .unwrap()
            .contains(&json!("strategy_research"))
    );
    web.json(
        Request::get(FACTORS)
            .header(header::COOKIE, &cookie)
            .header(header::ORIGIN, "https://cross.example.test"),
        Value::Null,
        StatusCode::FORBIDDEN,
    )
    .await;
}

type Queue = Arc<Mutex<HashMap<String, (i64, Value)>>>;

async fn submit_stub(
    State(queue): State<Queue>,
    headers: HeaderMap,
    Json(mut job): Json<Value>,
) -> Response {
    assert_finance_headers(&headers);
    let key = headers["idempotency-key"].to_str().unwrap();
    if job["source"]["kind"] == "inline" {
        assert_eq!(job["source"], inline(SOURCE));
        job["source"] = selection();
    }
    let id = {
        let mut queue = queue.lock().unwrap();
        if let Some((id, existing)) = queue.get(key) {
            return if *existing == job {
                (StatusCode::ACCEPTED, Json(json!({"id":id}))).into_response()
            } else {
                api_error(
                    StatusCode::CONFLICT,
                    "idempotency_conflict",
                    "Key already bound",
                )
            };
        }
        let id = 11 + queue.len() as i64;
        queue.insert(key.into(), (id, job));
        id
    };
    if key == "delayed" {
        future::pending::<()>().await;
    }
    (StatusCode::ACCEPTED, Json(json!({"id":id}))).into_response()
}

fn job(id: i64, request: Value) -> Value {
    json!({"id":id, "status":"queued", "request":request, "attempt":0,
        "created_at":START, "summary":null, "error":null})
}

#[tokio::test]
async fn backtest_proxy_reuses_keys_recovers_timeout_and_keeps_legacy_deep_link() {
    let legacy = job(
        7,
        json!({"strategy":"DemoStrategy", "instrument":"6EH4.XCME", "start":START, "end":END, "params":{}}),
    );
    let mut bundle_request = submission(selection());
    bundle_request["content_sha256"] = registered()["content_sha256"].clone();
    let configured = job(11, bundle_request);
    let queue = Queue::default();
    let stub = Router::new()
        .route(
            JOBS,
            post(submit_stub).get({
                let legacy = legacy.clone();
                let configured = configured.clone();
                move || {
                    let (legacy, configured) = (legacy.clone(), configured.clone());
                    async move { Json(json!({"items":[legacy,configured],"next_cursor":11})) }
                }
            }),
        )
        .route(
            "/api/v1/backtests/7",
            get({
                let legacy = legacy.clone();
                move || {
                    let legacy = legacy.clone();
                    async move { Json(legacy) }
                }
            }),
        )
        .route(
            "/api/v1/backtests/11",
            get({
                let configured = configured.clone();
                move || {
                    let configured = configured.clone();
                    async move { Json(configured) }
                }
            }),
        )
        .route(
            KEY_LOOKUP,
            get(
                |State(queue): State<Queue>, headers: HeaderMap| async move {
                    assert_finance_headers(&headers);
                    let queue = queue.lock().unwrap();
                    let key = headers["idempotency-key"].to_str().unwrap();
                    queue.get(key).map_or_else(
                        || api_error(StatusCode::NOT_FOUND, "job_not_found", "No such job"),
                        |(id, _)| Json(json!({"id":id})).into_response(),
                    )
                },
            ),
        )
        .with_state(Arc::clone(&queue));
    let web = Gateway::new(stub).await;
    let submit = |key| {
        web.admin(Request::post(JOBS))
            .header("Idempotency-Key", key)
    };
    let (uploaded, selected) = tokio::join!(
        web.json(
            submit("same"),
            submission(inline(SOURCE)),
            StatusCode::ACCEPTED
        ),
        web.json(
            submit("same"),
            submission(selection()),
            StatusCode::ACCEPTED
        )
    );
    assert_eq!(uploaded, json!({"id":11}));
    assert_eq!(selected, uploaded);
    let mut changed = submission(selection());
    changed["end"] = json!("2026-01-07T00:00:00Z");
    let conflict = web
        .json(submit("same"), changed, StatusCode::CONFLICT)
        .await;
    assert_eq!(conflict["error"]["code"], "idempotency_conflict");

    let deadline = web
        .json(
            submit("delayed"),
            submission(selection()),
            StatusCode::GATEWAY_TIMEOUT,
        )
        .await;
    assert_eq!(deadline["error"]["code"], "finance_timeout");
    assert!(
        deadline["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Idempotency-Key")
    );
    let lookup = |key| {
        web.admin(Request::get(KEY_LOOKUP))
            .header("Idempotency-Key", key)
    };
    let recovered = web
        .json(lookup("delayed"), Value::Null, StatusCode::OK)
        .await;
    assert_eq!(recovered, json!({"id":12}));
    web.json(lookup("missing"), Value::Null, StatusCode::NOT_FOUND)
        .await;
    assert_eq!(queue.lock().unwrap().len(), 2);

    for path in ["/backtests?job=7", "/strategies/synthetic.momentum"] {
        let page = web.send(Request::get(path), Body::empty()).await;
        assert_eq!(page.status(), StatusCode::OK, "{path}");
        assert_eq!(
            to_bytes(page.into_body(), 256).await.unwrap(),
            "<main>synthetic web</main>"
        );
    }
    let old_job = web
        .json(
            Request::get("/api/v1/backtests/7"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(old_job, legacy);
    web.json(
        Request::get("/api/v1/backtests/11"),
        Value::Null,
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(web.get("/api/v1/backtests/11").await, configured);
    let public = web
        .json(Request::get(JOBS), Value::Null, StatusCode::OK)
        .await;
    assert_eq!(public, json!({"items":[legacy], "next_cursor":11}));
    assert_eq!(
        web.get(JOBS).await,
        json!({"items":[legacy,configured], "next_cursor":11})
    );
}

fn access() -> Value {
    json!({"license":"fixture.synthetic", "record_local":true, "derive_non_display":true, "redistribute":false})
}

fn research_only() -> Value {
    json!({"factor_id":"synthetic.historical-momentum", "version":"1.0.0", "state":"research_only",
        "access":access(), "tradable":false, "selectable_for_backtest":false,
        "details":{"kind":"historical_research", "source_commit":"a".repeat(40),
            "summary_ref":"strategies/synthetic/results/summary.json",
            "decision":{"verdict":"synthetic failed gate", "passed":false, "cost_bps":"25.0000",
                "rule":"positive spread", "comparisons":{"longest_sample":{
                     "main_sharpe":PRECISE, "static_sharpe":"1.00000", "difference":"-0.8",
                     "outperformance":false, "flip":null}}}}})
}

fn factor_report() -> Value {
    let distribution = json!({"count":1, "mean":PRECISE, "sample_std":null, "ratio":null});
    let portfolio = json!({"periods":1, "gross_observations":1, "net_observations":1,
        "turnover_observations":1, "cumulative_gross_return":"0.5", "cumulative_net_return":PRECISE,
        "total_turnover":"1.0", "total_fee_fraction":"0.0025"});
    json!({"factor_id":"synthetic.factor", "version":"1.0.0", "data_version":"synthetic-data-v1",
        "evaluation_id":"fit", "access":access(), "peer_factors":[], "cost_bps":PRECISE, "quantile_count":2,
        "windows":[{"window":{"name":"fit", "phase":"train", "start":"2026-01-02", "end":"2026-01-06"},
            "factor_coverage":"1.0", "scored_coverage":"0.5", "ic":{"1":distribution}, "redundancy":{},
            "quantiles":[portfolio,portfolio], "benchmark":portfolio,
            "gross_information":distribution, "net_information":distribution}]})
}

#[tokio::test]
async fn factor_catalog_and_reports_keep_permissions_cursors_and_exact_decimal_text() {
    let stub = Router::new()
        .route(FACTORS, get(|uri: Uri| async move {
            assert_eq!(uri.query(), Some("factor_id=synthetic.historical-momentum&version=1.0.0&limit=1"));
            Json(json!({"schema_version":1,"as_of":END,"items":[research_only()],"next_cursor":null}))
        }))
        .route(EVALUATIONS, get(|uri: Uri| async move {
            assert_eq!(uri.query(), Some("version=1.0.0&after=1.0.0%40earlier&limit=1"));
            Json(json!({"schema_version":1,"as_of":END,"items":[factor_report()],"next_cursor":"1.0.0@fit"}))
        }))
        .route(EVALUATION, get(|uri: Uri| async move {
            assert_eq!(uri.query(), Some("version=1.0.0"));
            Json(json!({"schema_version":1,"as_of":END,"item":factor_report()}))
        }));
    let web = Gateway::new(stub).await;
    let catalog = web
        .get("/api/v1/factors?factor_id=synthetic.historical-momentum&version=1.0.0&limit=1")
        .await;
    assert_eq!(catalog["items"][0], research_only());
    let evaluations = web.get("/api/v1/factors/synthetic.factor/evaluations?version=1.0.0&after=1.0.0%40earlier&limit=1").await;
    assert_eq!(evaluations["next_cursor"], "1.0.0@fit");
    assert_eq!(evaluations["items"][0], factor_report());
    let report = web.get(&format!("{FACTOR_REPORT_URL}?version=1.0.0")).await;
    assert_eq!(report["item"], factor_report());
    assert_eq!(report["item"]["windows"][0]["ic"]["1"]["mean"], PRECISE);
    web.json(
        web.admin(Request::get(FACTOR_REPORT_URL)),
        Value::Null,
        StatusCode::BAD_REQUEST,
    )
    .await;
    web.json(
        web.admin(Request::get("/api/v1/factors/%2e%2e/evaluations")),
        Value::Null,
        StatusCode::BAD_REQUEST,
    )
    .await;
}

#[test]
fn research_openapi_has_private_routes_and_no_dangling_schema_references() {
    let spec = crate::openapi();
    for path in [
        STRATEGIES,
        VALIDATE,
        VERSION,
        FACTORS,
        EVALUATIONS,
        EVALUATION,
    ] {
        for operation in spec["paths"][path].as_object().unwrap().values() {
            assert_eq!(
                operation["security"],
                json!([{"adminBearer":[]},{"adminSession":[]}])
            );
            assert!(operation["responses"].get("500").is_none());
        }
    }
    assert_eq!(
        spec["paths"][JOBS]["post"]["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/SubmitRequest"
    );
    for (path, method) in [(VALIDATE, "post"), (VERSION, "get")] {
        assert_eq!(
            spec["paths"][path][method]["responses"]["200"]["content"]["application/json"]["schema"]
                ["$ref"],
            "#/components/schemas/StrategyBundlePreview"
        );
    }
    let schemas = &spec["components"]["schemas"];
    assert_eq!(
        schemas["StrategyBundlePreview"]["properties"]["plan"]["$ref"],
        "#/components/schemas/CompiledPlan"
    );
    assert_eq!(
        schemas["LocalEvidence"]["properties"]["strategy"]["oneOf"][1]["$ref"],
        "#/components/schemas/StrategyPreview"
    );
    assert_eq!(
        schemas["StrategyPreview"]["properties"]["execution_model"]["$ref"],
        "#/components/schemas/ExecutionModel"
    );
    let paths = spec["paths"].as_object().unwrap();
    for path in [
        "/api/v1/decisions/signals",
        "/api/v1/backtests/{id}/cancel",
        "/api/v1/backtests/worker-status",
    ] {
        assert!(!paths.contains_key(path));
    }
    fn check_refs(node: &Value, spec: &Value) {
        match node {
            Value::Object(entries) => {
                for (key, value) in entries {
                    if key == "$ref"
                        && let Some(reference) = value.as_str().and_then(|v| v.strip_prefix('#'))
                    {
                        assert!(
                            spec.pointer(reference).is_some_and(Value::is_object),
                            "{reference}"
                        );
                    }
                    check_refs(value, spec);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| check_refs(item, spec)),
            _ => {}
        }
    }
    check_refs(&spec, &spec);
}

#[tokio::test]
async fn research_route_limits_auth_and_old_deep_links_work_without_transport() {
    let web = Gateway::with_finance_url("http://127.0.0.1:1".into());
    let oversized = "界".repeat(MAX_DOCUMENT_BYTES / 3 + 1);
    for path in [VALIDATE, STRATEGIES, JOBS] {
        let body = if path == JOBS {
            submission(inline(&oversized))
        } else {
            document_body(&oversized)
        };
        let request = || Request::post(path).header("Idempotency-Key", "synthetic-check");
        web.json(request(), body.clone(), StatusCode::UNAUTHORIZED)
            .await;
        let response = web
            .json(web.admin(request()), body, StatusCode::PAYLOAD_TOO_LARGE)
            .await;
        assert_eq!(response["error"]["code"], "body_too_large");
    }
    for path in [STRATEGIES, VERSION_URL, FACTORS, FACTOR_REPORT_URL] {
        for method in [Method::GET, Method::HEAD] {
            let response = web
                .send(Request::builder().method(method).uri(path), Body::empty())
                .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
    }
    for path in ["/backtests?job=7", "/strategies/synthetic.momentum"] {
        let page = web.send(Request::get(path), Body::empty()).await;
        assert_eq!(page.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(page.into_body(), 256).await.unwrap(),
            "<main>synthetic web</main>"
        );
    }
    for path in [
        "/assets/missing.js",
        "/strategies/synthetic..momentum",
        "/strategies/a/b.js",
    ] {
        assert_eq!(
            web.send(Request::get(path), Body::empty()).await.status(),
            StatusCode::NOT_FOUND
        );
    }
}

#[test]
fn generated_research_models_round_trip_precise_plans_and_evaluations() {
    let plan: models::StrategyPreview = serde_json::from_value(preview()).unwrap();
    assert_eq!(serde_json::to_value(plan).unwrap(), preview());
    let definition: models::FactorReadItem = serde_json::from_value(research_only()).unwrap();
    assert_eq!(serde_json::to_value(definition).unwrap(), research_only());
    let evaluation: models::FactorEvaluationItem = serde_json::from_value(factor_report()).unwrap();
    assert_eq!(serde_json::to_value(evaluation).unwrap(), factor_report());
}
