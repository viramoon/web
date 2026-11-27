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
    assert_eq!(first, page(vec![legacy(7)], Some(7)));
    let second = public_page(
        one_job_query(first.next_cursor.flatten()),
        LOOKAHEAD_BUDGET,
        read,
    )
    .await
    .ok()
    .unwrap();
    assert_eq!(second, page(vec![legacy(13)], None));
    let private_only = public_page(one_job_query(Some(17)), LOOKAHEAD_BUDGET, read)
        .await
        .ok()
        .unwrap();
    assert_eq!(private_only, page(vec![], None));
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
        let response = result
            .expect("public list must finish before HTTP deadline")
            .map(Json)
            .into_response();
        assert_eq!(response.status(), StatusCode::OK, "{private_count}");
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let actual: models::Page = serde_json::from_slice(&body).unwrap();
        assert_eq!(actual, page(vec![legacy(7)], None), "{private_count}");
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
    assert_eq!(result, page(vec![legacy(7)], None));
    assert_eq!(reads, [None, Some(7), Some(8), Some(108)]);
    assert_eq!(start.elapsed(), LOOKAHEAD_BUDGET);
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
    assert_eq!(result, page(vec![legacy(7)], None));
}
