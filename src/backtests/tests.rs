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

#[tokio::test]
async fn public_backtest_cursor_only_refers_to_visible_jobs_across_private_pages() {
    let read = |cursor| async move {
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

    let first = public_page(None, 1, read).await.ok().unwrap();
    assert_eq!(first, page(vec![legacy(7)], Some(7)));
    let second = public_page(first.next_cursor.flatten(), 1, read)
        .await
        .ok()
        .unwrap();
    assert_eq!(second, page(vec![legacy(13)], None));
    let private_only = public_page(Some(17), 1, read).await.ok().unwrap();
    assert_eq!(private_only, page(vec![], None));
}

#[tokio::test]
async fn public_backtest_page_rejects_invalid_upstream_continuations() {
    for bad_page in [
        page(vec![bundle(11)], Some(11)),
        page(vec![bundle(13)], Some(12)),
        page(vec![], Some(13)),
    ] {
        let result = public_page(Some(11), 1, |cursor| {
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
