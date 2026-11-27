use super::*;
use serde_json::json;

fn legacy(id: i64) -> models::Job {
    job(
        id,
        json!({"strategy":"DemoStrategy","instrument":"6EH4.XCME","params":{}}),
    )
}

fn bundle(id: i64) -> models::Job {
    job(
        id,
        json!({"strategy":"StrategyBundle","source":{"kind":"registered","id":"synthetic.momentum",
            "version":"1.0.0"}, "content_sha256":"1".repeat(64)}),
    )
}

fn job(id: i64, mut request: Value) -> models::Job {
    let start = "2026-01-02T00:00:00Z";
    request["start"] = json!(start);
    request["end"] = json!("2026-01-06T00:00:00Z");
    serde_json::from_value(json!({"id":id, "status":"queued", "request":request,
        "attempt":0, "created_at":start}))
    .unwrap()
}

fn page(items: Vec<models::Job>, cursor: Option<i64>) -> models::Page {
    models::Page {
        items,
        next_cursor: Some(cursor),
    }
}

fn one_job_query(cursor: Option<i64>) -> ListQuery {
    ListQuery {
        limit: Some(1),
        cursor,
    }
}

fn continuation(body: &Value) -> ListQuery {
    let id = body["next_cursor"].as_i64().expect("integer cursor");
    let uri = format!("/api/v1/backtests?limit=1&cursor={id}")
        .parse()
        .unwrap();
    let query = Query::<ListQuery>::try_from_uri(&uri).unwrap().0;
    assert!(query.valid());
    query
}

// The synthetic queue records visibility separately from the wire model so the
// producer does not use the web permission check to choose its test response.
fn finance_page(
    jobs: &[(bool, models::Job)],
    query: ListQuery,
    visibility: Option<models::BacktestVisibility>,
) -> models::Page {
    let limit = query.limit.unwrap_or(DEFAULT_PAGE_LIMIT) as usize;
    let mut rows = jobs
        .iter()
        .filter(|(public, job)| {
            (visibility.is_none() || *public) && job.id > query.cursor.unwrap_or(0)
        })
        .map(|(_, job)| job.clone());
    let items = rows.by_ref().take(limit).collect::<Vec<_>>();
    let next = rows.next().and_then(|_| items.last().map(|job| job.id));
    page(items, next)
}

async fn response_page(jobs: &[(bool, models::Job)], query: ListQuery, admin: bool) -> Value {
    let result = list_page(query, admin, |sent, visibility| {
        assert_eq!((sent.limit, sent.cursor), (query.limit, query.cursor));
        assert_eq!(
            visibility,
            if admin {
                None
            } else {
                Some(models::BacktestVisibility::Legacy)
            }
        );
        std::future::ready(Ok(finance_page(jobs, sent, visibility)))
    })
    .await;
    json!(result.ok().unwrap())
}

#[tokio::test]
async fn public_backtest_cursor_only_refers_to_visible_jobs_across_private_pages() {
    let second = job(
        13,
        json!({"strategy":"SmaCrossover","instrument":"TEST","params":{}}),
    );
    let jobs = vec![
        (false, bundle(1)),
        (false, bundle(3)),
        (true, legacy(7)),
        (false, bundle(11)),
        (false, bundle(12)),
        (true, second.clone()),
        (false, bundle(17)),
        (false, bundle(19)),
    ];
    let first = response_page(&jobs, one_job_query(None), false).await;
    assert_eq!(first, json!({"items":[legacy(7)], "next_cursor":7}));
    for query in [continuation(&first), one_job_query(Some(7))] {
        assert_eq!(
            response_page(&jobs, query, false).await,
            json!({"items":[second], "next_cursor":null})
        );
    }
    assert_eq!(
        response_page(&jobs, one_job_query(Some(17)), false).await,
        json!({"items":[], "next_cursor":null})
    );
    let admin = response_page(&jobs, one_job_query(Some(7)), true).await;
    assert_eq!(admin, json!({"items":[bundle(11)], "next_cursor":11}));
}

#[tokio::test]
async fn public_backtest_page_rejects_invalid_upstream_continuations() {
    let query = ListQuery {
        limit: Some(2),
        cursor: Some(11),
    };
    for bad_page in [
        page(vec![bundle(11)], Some(11)),
        page(vec![bundle(13)], Some(12)),
        page(vec![], Some(13)),
        page(vec![legacy(13)], Some(14)),
        page(vec![legacy(13), legacy(13)], Some(13)),
        page(vec![legacy(15), legacy(13)], None),
        page(vec![legacy(13), legacy(14), legacy(15)], None),
        page(vec![legacy(13), bundle(14)], Some(14)),
        page(
            vec![job(
                13,
                json!({"strategy":"FuturePrivate","instrument":"TEST","params":{}}),
            )],
            None,
        ),
    ] {
        let result = list_page(query, false, |_, _| std::future::ready(Ok(bad_page))).await;
        let error = result.err().unwrap().into_response();
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(error.into_body(), 512).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"error":{"code":"finance_invalid_response","message":"Invalid finance response"}})
        );
    }
}

#[tokio::test(start_paused = true)]
async fn slow_private_backtest_history_keeps_the_public_response_and_bounds_reads() {
    use tokio::time::{Duration, Instant, sleep, timeout};
    for count in [0, 30, 301, 1000, 10_000] {
        let mut jobs = vec![(true, legacy(7))];
        jobs.extend((8..8 + count).map(|id| (false, bundle(id))));
        for (cursor, expected) in [
            (None, json!({"items":[legacy(7)], "next_cursor":null})),
            (Some(7), json!({"items":[], "next_cursor":null})),
        ] {
            let start = Instant::now();
            let mut reads = Vec::new();
            let result = timeout(
                Duration::from_secs(15),
                list_page(one_job_query(cursor), false, |sent, visibility| {
                    reads.push((sent.limit, sent.cursor, visibility));
                    let reply = finance_page(&jobs, sent, visibility);
                    async move {
                        sleep(Duration::from_millis(520)).await;
                        Ok(reply)
                    }
                }),
            )
            .await
            .expect("public list must finish before HTTP deadline");
            assert_eq!(json!(result.ok().unwrap()), expected, "{count}");
            assert_eq!(
                reads,
                [(Some(1), cursor, Some(models::BacktestVisibility::Legacy))]
            );
            assert_eq!(start.elapsed(), Duration::from_millis(520));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn public_backtest_lookahead_time_budget_keeps_confirmed_jobs() {
    use tokio::time::{Duration, Instant, sleep};
    let start = Instant::now();
    let read = |sent: ListQuery, visibility| async move {
        assert_eq!(visibility, Some(models::BacktestVisibility::Legacy));
        let (delay, reply) = match sent.cursor {
            None => (Duration::from_millis(520), page(vec![legacy(7)], Some(7))),
            Some(7) => (Duration::from_secs(3), page(vec![legacy(109)], None)),
            _ => panic!("unexpected cursor"),
        };
        sleep(delay).await;
        Ok(reply)
    };
    let first = list_page(one_job_query(None), false, read)
        .await
        .ok()
        .unwrap();
    assert_eq!(start.elapsed(), Duration::from_millis(520));
    let first = json!(first);
    assert_eq!(first, json!({"items":[legacy(7)], "next_cursor":7}));
    let next = list_page(continuation(&first), false, read)
        .await
        .ok()
        .unwrap();
    assert_eq!(
        json!(next),
        json!({"items":[legacy(109)],"next_cursor":null})
    );
}

#[tokio::test]
async fn failed_backtest_lookahead_keeps_the_successful_initial_page() {
    let first = list_page(one_job_query(None), false, |sent, _| {
        assert_eq!(sent.cursor, None);
        std::future::ready(Ok(page(vec![legacy(7)], Some(7))))
    })
    .await
    .ok()
    .unwrap();
    let first = json!(first);
    assert_eq!(first, json!({"items":[legacy(7)], "next_cursor":7}));
    let query = continuation(&first);
    let failed = list_page(query, false, |_, _| {
        std::future::ready(Err(Fault::invalid_response()))
    })
    .await;
    assert_eq!(failed.err().unwrap().status, StatusCode::BAD_GATEWAY);
    let next = response_page(&[(true, legacy(13))], query, false).await;
    assert_eq!(next, json!({"items":[legacy(13)],"next_cursor":null}));
}

#[tokio::test]
async fn anonymous_cursor_reaches_the_public_job_after_301_private_jobs() {
    let mut jobs = vec![(true, legacy(7))];
    jobs.extend((8..309).map(|id| (false, bundle(id))));
    jobs.push((true, legacy(309)));
    let first = response_page(&jobs, one_job_query(None), false).await;
    assert_eq!(first["items"], json!([legacy(7)]));
    let issued = first["next_cursor"].as_i64().unwrap();
    assert!(!(8..309).contains(&issued));
    assert_eq!(issued, 7);
    for query in [
        continuation(&first),
        one_job_query(Some(7)),
        one_job_query(Some(208)),
    ] {
        assert_eq!(
            response_page(&jobs, query, false).await,
            json!({"items":[legacy(309)],"next_cursor":null})
        );
    }
}

#[tokio::test]
async fn empty_anonymous_page_can_continue_across_multiple_scan_windows() {
    let mut jobs: Vec<_> = (1..1001).map(|id| (false, bundle(id))).collect();
    assert_eq!(
        response_page(&jobs, one_job_query(None), false).await,
        json!({"items":[], "next_cursor":null})
    );
    jobs.push((true, legacy(1001)));
    for cursor in [None, Some(301), Some(602), Some(903)] {
        assert_eq!(
            response_page(&jobs, one_job_query(cursor), false).await,
            json!({"items":[legacy(1001)], "next_cursor":null})
        );
    }
}

#[tokio::test]
async fn terminal_pages_serialize_a_null_cursor_even_when_the_optional_field_is_absent() {
    for admin in [false, true] {
        let result = list_page(one_job_query(None), admin, |_, _| {
            std::future::ready(Ok(models::Page::new(vec![legacy(7)])))
        })
        .await
        .ok()
        .unwrap();
        assert_eq!(
            json!(result),
            json!({"items":[legacy(7)],"next_cursor":null})
        );
    }
}

#[test]
fn openapi_preserves_the_baseline_numeric_cursor_contract() {
    let spec = crate::openapi();
    let operation = &spec["paths"]["/api/v1/backtests"]["get"];
    let parameters = operation["parameters"].as_array().unwrap();
    assert_eq!(parameters.len(), 2);
    assert!(parameters.iter().all(|p| p["name"] != "visibility"));
    let cursor = parameters.iter().find(|p| p["name"] == "cursor").unwrap();
    assert_eq!(
        cursor["schema"],
        json!({"type":"integer", "format":"int64"})
    );
    let page = &spec["components"]["schemas"]["Page"];
    assert_eq!(
        page["properties"]["items"]["items"]["$ref"],
        "#/components/schemas/Job"
    );
    assert_eq!(
        page["properties"]["next_cursor"],
        json!({"type":["integer","null"],"format":"int64"})
    );
    let int64 = json!({"type":"integer","format":"int64"});
    for schema in ["Accepted", "Job"] {
        assert_eq!(
            spec["components"]["schemas"][schema]["properties"]["id"],
            int64
        );
    }
    let detail = &spec["paths"]["/api/v1/backtests/{id}"]["get"];
    let id = detail["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "id")
        .unwrap();
    assert_eq!(id["schema"], int64);
}

#[tokio::test]
async fn invalid_finance_ids_fail_on_first_and_followup_pages() {
    for cursor in [None, Some(7)] {
        for admin in [false, true] {
            for bad_page in [
                page(vec![legacy(0)], None),
                page(vec![bundle(i64::MIN)], None),
                page(vec![legacy(9)], Some(-1)),
            ] {
                let result = list_page(one_job_query(cursor), admin, |_, _| {
                    std::future::ready(Ok(bad_page))
                })
                .await;
                assert_eq!(result.err().unwrap().status, StatusCode::BAD_GATEWAY);
            }
        }
    }
}

#[tokio::test]
async fn accepted_ids_and_visible_details_keep_every_positive_int64_bit() {
    for id in [1_i64 << 52, (1_i64 << 53) + 1, i64::MAX] {
        let accepted = accepted_job(models::Accepted::new(id)).into_response();
        assert_eq!(accepted.status(), StatusCode::OK);
        let body = axum::body::to_bytes(accepted.into_body(), 64)
            .await
            .unwrap();
        assert_eq!(body, format!(r#"{{"id":{id}}}"#));
        for (job, admin) in [(legacy(id), false), (legacy(id), true), (bundle(id), true)] {
            let expected = json!(job);
            let detail = detail_response(job, admin);
            assert_eq!(detail.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(detail.into_body(), 4096)
                .await
                .unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), expected);
        }
        let hidden = detail_response(bundle(id), false);
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(hidden.into_body(), 256).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"error":{"code":"job_not_found", "message":"No such job"}})
        );
    }
    for invalid in [0, -1] {
        assert_eq!(
            accepted_job(models::Accepted::new(invalid))
                .into_response()
                .status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            detail_response(legacy(invalid), true).status(),
            StatusCode::BAD_GATEWAY
        );
    }
}

#[tokio::test]
async fn full_width_cursors_return_the_same_public_pages_with_or_without_private_jobs() {
    let first = 1_i64 << 52;
    let second = (1_i64 << 53) + 1;
    let last = i64::MAX;
    let visible = vec![
        (true, legacy(first)),
        (true, legacy(second)),
        (true, legacy(last)),
    ];
    let mixed = vec![
        (false, bundle(first - 1)),
        (true, legacy(first)),
        (false, bundle(first + 1)),
        (true, legacy(second)),
        (false, bundle(last - 1)),
        (true, legacy(last)),
    ];
    for jobs in [&visible, &mixed] {
        for (cursor, expected) in [
            (None, json!({"items":[legacy(first)], "next_cursor":first})),
            (
                Some(first),
                json!({"items":[legacy(second)], "next_cursor":second}),
            ),
            (
                Some(second),
                json!({"items":[legacy(last)], "next_cursor":null}),
            ),
            (Some(last), json!({"items":[], "next_cursor":null})),
        ] {
            let query = one_job_query(cursor);
            assert!(query.valid());
            assert_eq!(response_page(jobs, query, false).await, expected);
        }
    }
    let admin = response_page(&mixed, one_job_query(Some(last - 2)), true).await;
    assert_eq!(
        admin,
        json!({"items":[bundle(last-1)],"next_cursor":last-1})
    );
}

#[tokio::test]
async fn bounded_mixed_pages_cannot_distinguish_a_private_tail_from_a_later_public_job() {
    let mut jobs = vec![(true, legacy(7))];
    jobs.extend((8..309).map(|id| (false, bundle(id))));
    let private_tail = jobs.clone();
    jobs.push((true, legacy(309)));
    for (limit, cursor) in [(1, None), (1, Some(7)), (100, Some(8)), (100, Some(108))] {
        let query = ListQuery {
            limit: Some(limit),
            cursor,
        };
        assert_eq!(
            finance_page(&private_tail, query, None),
            finance_page(&jobs, query, None)
        );
    }
    for (history, next) in [(&private_tail, None), (&jobs, Some(7))] {
        assert_eq!(
            response_page(history, one_job_query(None), false).await,
            json!({"items":[legacy(7)], "next_cursor":next})
        );
    }
}

#[test]
fn cursor_parser_keeps_legacy_nonnegative_decimal_input() {
    let neighbors = [1_i64 << 52, 1_i64 << 53]
        .into_iter()
        .flat_map(|n| (n - 1)..=(n + 1));
    for id in [0, 7, i64::MAX].into_iter().chain(neighbors) {
        assert_eq!(continuation(&json!({"next_cursor":id})).cursor, Some(id));
    }
}

#[test]
fn invalid_negative_cursors_and_their_neighbors_are_rejected() {
    for value in [i64::MIN, -(1_i64 << 52), -(1_i64 << 51), -1] {
        for delta in [-1, 0, 1] {
            if let Some(negative) = value.checked_add(delta).filter(|n| *n < 0) {
                assert!(!one_job_query(Some(negative)).valid());
            }
        }
    }
}

#[tokio::test]
async fn public_cursor_remains_valid_after_unrelated_pages_and_for_other_readers() {
    let jobs = vec![(true, legacy(7)), (true, legacy(309))];
    let first = response_page(&jobs, one_job_query(None), false).await;
    for position in 10_000..14_097 {
        let others = [(true, legacy(position + 1)), (true, legacy(position + 2))];
        let page = response_page(&others, one_job_query(Some(position)), false).await;
        assert_eq!(page["next_cursor"].as_i64(), Some(position + 1));
    }
    assert_eq!(
        response_page(&jobs, one_job_query(None), false).await,
        first
    );
    assert_eq!(
        response_page(&jobs.clone(), continuation(&first), false).await,
        json!({"items":[legacy(309)],"next_cursor":null})
    );
}
