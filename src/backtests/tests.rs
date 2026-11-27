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

fn one_job_query(cursor: Option<i64>) -> Pagination {
    Pagination {
        limit: Some(1),
        cursor,
    }
}

async fn public_body(result: ScannedPage) -> Value {
    let response = result.into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn continuation(body: &Value) -> Pagination {
    let token = body["next_cursor"].as_i64().expect("integer cursor");
    assert!((0..=cursor::MAX_SAFE_CURSOR).contains(&token));
    let uri = format!("/api/v1/backtests?limit=1&cursor={token}")
        .parse()
        .unwrap();
    Query::<ListQuery>::try_from_uri(&uri)
        .unwrap()
        .0
        .pagination()
        .unwrap()
}

fn opaque_continuation(body: &Value) -> Pagination {
    let token = body["next_cursor"].as_i64().expect("integer cursor");
    assert!((cursor::FIRST_HANDLE..=cursor::MAX_SAFE_CURSOR).contains(&token));
    continuation(body)
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const LOOKAHEAD_BUDGET: Duration = Duration::from_millis(7500);

#[tokio::test]
async fn public_backtest_cursor_only_refers_to_visible_jobs_across_private_pages() {
    let read = |_, cursor| async move {
        let page = match cursor {
            None => page(vec![bundle(1), bundle(3)], Some(3)),
            Some(3) => page(vec![legacy(7), bundle(11)], Some(11)),
            Some(7) => page(vec![bundle(11), bundle(12)], Some(12)),
            Some(11) => page(vec![bundle(12), legacy(13)], Some(13)),
            Some(12) => page(vec![legacy(13), bundle(17)], Some(17)),
            Some(17) => page(vec![bundle(19)], None),
            _ => panic!("unexpected upstream cursor: {cursor:?}"),
        };
        Ok(page)
    };

    let first = public_page(one_job_query(None), LOOKAHEAD_BUDGET, read)
        .await
        .ok()
        .unwrap();
    let first = public_body(first).await;
    assert_eq!(first, json!({"items":[legacy(7)], "next_cursor":7}));
    for query in [continuation(&first), one_job_query(Some(7))] {
        let second = public_page(query, LOOKAHEAD_BUDGET, read)
            .await
            .ok()
            .unwrap();
        assert_eq!(
            public_body(second).await,
            json!({"items":[legacy(13)], "next_cursor":null})
        );
    }
    let private_only = public_page(one_job_query(Some(17)), LOOKAHEAD_BUDGET, read)
        .await
        .ok()
        .unwrap();
    assert_eq!(
        public_body(private_only).await,
        json!({"items":[], "next_cursor":null})
    );
}

#[tokio::test]
async fn public_backtest_page_rejects_invalid_upstream_continuations() {
    for bad_page in [
        page(vec![bundle(11)], Some(11)),
        page(vec![bundle(13)], Some(12)),
        page(vec![], Some(13)),
    ] {
        let result = public_page(one_job_query(Some(11)), LOOKAHEAD_BUDGET, |_, cursor| {
            let page = bad_page.clone();
            async move {
                assert_eq!(cursor, Some(11));
                Ok(page)
            }
        })
        .await;
        assert_eq!(result.err().unwrap().status, StatusCode::BAD_GATEWAY);
    }
}

fn finance_page(jobs: &[models::Job], limit: Option<u32>, cursor: Option<i64>) -> models::Page {
    let limit = limit.unwrap_or(DEFAULT_PAGE_LIMIT) as usize;
    let mut jobs: Vec<_> = jobs
        .iter()
        .filter(|job| job.id > cursor.unwrap_or(0))
        .take(limit + 1)
        .cloned()
        .collect();
    let cursor = if jobs.len() > limit {
        jobs.pop();
        jobs.last().map(|job| job.id)
    } else {
        None
    };
    page(jobs, cursor)
}

#[tokio::test(start_paused = true)]
async fn slow_private_backtest_history_keeps_the_public_response_and_bounds_reads() {
    for (private_count, expected_reads) in [(0, 1), (30, 3), (1000, 4)] {
        let mut jobs = vec![legacy(7)];
        jobs.extend((8..8 + private_count).map(bundle));
        let mut reads = Vec::new();
        let result = tokio::time::timeout(
            REQUEST_TIMEOUT,
            public_page(one_job_query(None), LOOKAHEAD_BUDGET, |limit, cursor| {
                reads.push((limit, cursor));
                let reply = finance_page(&jobs, limit, cursor);
                async move {
                    tokio::time::sleep(Duration::from_millis(520)).await;
                    Ok(reply)
                }
            }),
        )
        .await;
        let actual = public_body(
            result
                .expect("public list must finish before HTTP deadline")
                .ok()
                .unwrap(),
        )
        .await;
        assert_eq!(actual["items"], json!([legacy(7)]), "{private_count}");
        if private_count == 1000 {
            assert_eq!(opaque_continuation(&actual).cursor, Some(208));
        } else {
            assert!(actual["next_cursor"].is_null(), "{private_count}");
        }
        assert_eq!(reads.len(), expected_reads, "{private_count}");
        assert_eq!(reads[0], (Some(1), None));
        if private_count > 0 {
            assert_eq!(reads[1], (Some(1), Some(7)));
            assert_eq!(reads[2], (Some(100), Some(8)));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn public_backtest_lookahead_time_budget_keeps_confirmed_jobs() {
    let start = Instant::now();
    let mut reads = Vec::new();
    let result = tokio::time::timeout(
        REQUEST_TIMEOUT,
        public_page(one_job_query(None), LOOKAHEAD_BUDGET, |_, cursor| {
            reads.push(cursor);
            async move {
                let (delay, reply) = match cursor {
                    None => (Duration::from_millis(520), page(vec![legacy(7)], Some(7))),
                    Some(7) => (Duration::from_secs(3), page(vec![bundle(8)], Some(8))),
                    Some(8) => (Duration::from_secs(3), page(vec![bundle(108)], Some(108))),
                    Some(108) => (Duration::from_secs(3), page(vec![legacy(109)], None)),
                    _ => panic!("unexpected upstream cursor: {cursor:?}"),
                };
                tokio::time::sleep(delay).await;
                Ok(reply)
            }
        }),
    )
    .await
    .unwrap()
    .ok()
    .unwrap();
    assert_eq!(reads, [None, Some(7), Some(8), Some(108)]);
    assert_eq!(start.elapsed(), LOOKAHEAD_BUDGET);
    let actual = public_body(result).await;
    assert_eq!(actual["items"], json!([legacy(7)]));
    assert_eq!(opaque_continuation(&actual).cursor, Some(108));
    let resumed = public_page(
        continuation(&actual),
        LOOKAHEAD_BUDGET,
        |_, cursor| async move {
            assert_eq!(cursor, Some(108));
            Ok(page(vec![legacy(109)], None))
        },
    )
    .await
    .ok()
    .unwrap();
    assert_eq!(
        public_body(resumed).await,
        json!({"items":[legacy(109)],"next_cursor":null})
    );
}

#[tokio::test]
async fn failed_backtest_lookahead_keeps_the_successful_initial_page() {
    let result = public_page(
        one_job_query(None),
        LOOKAHEAD_BUDGET,
        |_, cursor| async move {
            match cursor {
                None => Ok(page(vec![legacy(7)], Some(7))),
                Some(7) => Err(Fault::invalid_response()),
                _ => panic!("unexpected upstream cursor: {cursor:?}"),
            }
        },
    )
    .await
    .ok()
    .unwrap();
    let actual = public_body(result).await;
    assert_eq!(actual["items"], json!([legacy(7)]));
    assert_eq!(opaque_continuation(&actual).cursor, Some(7));
}

#[tokio::test]
async fn anonymous_cursor_reaches_the_public_job_after_301_private_jobs() {
    let mut jobs = vec![legacy(7)];
    jobs.extend((8..309).map(bundle));
    jobs.push(legacy(309));
    let read = |limit, cursor| {
        let reply = finance_page(&jobs, limit, cursor);
        async move { Ok(reply) }
    };
    let first = public_page(one_job_query(None), LOOKAHEAD_BUDGET, read)
        .await
        .ok()
        .unwrap();
    let first = public_body(first).await;
    assert_eq!(first["items"], json!([legacy(7)]));
    let issued = first["next_cursor"].as_i64().unwrap();
    assert!(!(8..309).contains(&issued));
    let next = opaque_continuation(&first);
    assert_eq!(next.cursor, Some(208));
    let tampered = ListQuery {
        limit: Some(1),
        cursor: Some(issued ^ 1),
    }
    .pagination()
    .err()
    .unwrap()
    .into_response();
    assert_eq!(tampered.status(), StatusCode::BAD_REQUEST);

    // A legacy numeric cursor starts within the same gap and must also remain resumable.
    for mut query in [next, one_job_query(Some(7))] {
        let mut public_ids = Vec::new();
        for _ in 0..3 {
            let result = public_page(query, LOOKAHEAD_BUDGET, read)
                .await
                .ok()
                .unwrap();
            let body = public_body(result).await;
            let items: Vec<models::Job> = serde_json::from_value(body["items"].clone()).unwrap();
            assert!(items.iter().all(is_legacy));
            public_ids.extend(items.into_iter().map(|job| job.id));
            if body["next_cursor"].is_null() {
                break;
            }
            assert!(!(8..309).contains(&body["next_cursor"].as_i64().unwrap()));
            assert!(public_ids.is_empty(), "the final public job is the tail");
            query = opaque_continuation(&body);
        }
        assert_eq!(public_ids, [309]);
    }
}

#[tokio::test]
async fn empty_anonymous_page_can_continue_across_multiple_scan_windows() {
    let mut jobs: Vec<_> = (1..1001).map(bundle).collect();
    jobs.push(legacy(1001));
    let read = |limit, cursor| {
        let reply = finance_page(&jobs, limit, cursor);
        async move { Ok(reply) }
    };
    let mut query = one_job_query(None);
    for expected_position in [301, 602, 903] {
        let page = public_page(query, LOOKAHEAD_BUDGET, read)
            .await
            .ok()
            .unwrap();
        let body = public_body(page).await;
        assert_eq!(body["items"], json!([]));
        query = opaque_continuation(&body);
        assert_eq!(query.cursor, Some(expected_position));
    }
    let tail = public_page(query, LOOKAHEAD_BUDGET, read)
        .await
        .ok()
        .unwrap();
    assert_eq!(
        public_body(tail).await,
        json!({"items":[legacy(1001)],"next_cursor":null})
    );
}

#[test]
fn openapi_preserves_the_baseline_numeric_cursor_contract() {
    let spec = crate::openapi();
    let operation = &spec["paths"]["/api/v1/backtests"]["get"];
    let parameters = operation["parameters"].as_array().unwrap();
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
}

#[tokio::test]
async fn upstream_ids_in_the_reserved_namespace_fail_on_first_and_followup_pages() {
    let invalid_id = cursor::FIRST_HANDLE;
    for bad_page in [
        page(vec![legacy(invalid_id)], None),
        page(vec![bundle(invalid_id)], None),
        page(vec![legacy(7)], Some(invalid_id)),
    ] {
        assert_eq!(
            validate_page_ids(&bad_page).err().unwrap().status,
            StatusCode::BAD_GATEWAY
        );
        let first = public_page(one_job_query(None), LOOKAHEAD_BUDGET, |_, _| {
            std::future::ready(Ok(bad_page.clone()))
        })
        .await;
        assert_eq!(first.err().unwrap().status, StatusCode::BAD_GATEWAY);
        let followup = public_page(one_job_query(None), LOOKAHEAD_BUDGET, |_, cursor| {
            let reply = if cursor.is_none() {
                page(vec![legacy(7)], Some(7))
            } else {
                bad_page.clone()
            };
            std::future::ready(Ok(reply))
        })
        .await;
        assert_eq!(followup.err().unwrap().status, StatusCode::BAD_GATEWAY);
    }
}
