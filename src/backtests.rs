use crate::{AppState, ErrorBody, api_error, finance::Fault, research};
use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use finance_client::{
    apis::{jobs_api, research_api},
    models,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::time::Duration;
use tokio::time::{Instant, timeout_at};

const DEFAULT_PAGE_LIMIT: u32 = 20;
const MAX_PAGE_LIMIT: u32 = 100;
const MAX_PUBLIC_PAGE_READS: usize = 4;

mod cursor;

#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum Submission {
    Legacy(LegacySubmission),
    Bundle(BundleSubmission),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacySubmission {
    strategy: String,
    instrument: String,
    start: String,
    end: String,
    params: Map<String, Value>,
}

impl LegacySubmission {
    fn into_request(self) -> Option<models::JobRequest> {
        if self
            .params
            .keys()
            .any(|k| !matches!(k.as_str(), "fast_period" | "slow_period" | "shares"))
        {
            return None;
        }
        let params =
            serde_json::from_value::<models::StrategyParams>(Value::Object(self.params)).ok()?;
        Some(models::JobRequest::new(
            self.end,
            self.instrument,
            params,
            self.start,
            self.strategy,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BundleSubmission {
    strategy: models::BundleStrategy,
    start: String,
    end: String,
    source: Source,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Source {
    Inline {
        format: models::StrategyFormat,
        content: String,
    },
    Registered {
        id: String,
        version: String,
    },
}

impl BundleSubmission {
    fn into_request(self) -> Option<models::BundleSubmission> {
        let source = match self.source {
            Source::Inline { format, content } => {
                if content.len() > research::MAX_DOCUMENT_BYTES {
                    return None;
                }
                let source = models::StrategySourceOneOf::new(
                    content,
                    format,
                    models::strategy_source_one_of::Kind::Inline,
                );
                models::StrategySource::StrategySourceOneOf(Box::new(source))
            }
            Source::Registered { id, version } => {
                let source = models::StrategySourceOneOf1::new(
                    id,
                    models::strategy_source_one_of_1::Kind::Registered,
                    version,
                );
                models::StrategySource::StrategySourceOneOf1(Box::new(source))
            }
        };
        Some(models::BundleSubmission::new(
            self.end,
            source,
            self.start,
            self.strategy,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListQuery {
    limit: Option<u32>,
    cursor: Option<i64>,
}

struct Pagination {
    limit: Option<u32>,
    cursor: Option<i64>,
}

#[derive(Debug)]
enum InvalidPagination {
    Parameters,
    Cursor,
}

impl IntoResponse for InvalidPagination {
    fn into_response(self) -> Response {
        match self {
            Self::Parameters => invalid("Invalid pagination"),
            Self::Cursor => api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid or expired backtest cursor",
            ),
        }
    }
}

impl ListQuery {
    fn pagination(self) -> Result<Pagination, InvalidPagination> {
        let limit = self.limit.unwrap_or(DEFAULT_PAGE_LIMIT);
        if !(1..=MAX_PAGE_LIMIT).contains(&limit) {
            return Err(InvalidPagination::Parameters);
        }
        Ok(Pagination {
            limit: self.limit,
            cursor: self.cursor.map(cursor::resolve).transpose()?,
        })
    }
}

fn invalid(message: &str) -> Response {
    api_error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_request", message)
}

fn lookup_key(headers: &HeaderMap) -> Option<&str> {
    let mut keys = headers.get_all("Idempotency-Key").iter();
    keys.next().and_then(|v| v.to_str().ok()).filter(|v| {
        (1..=128).contains(&v.len())
            && v.bytes().all(|c| c.is_ascii_graphic())
            && keys.next().is_none()
    })
}

fn accepted_job(accepted: models::Accepted) -> Result<Json<models::Accepted>, Fault> {
    if accepted.id > 0 {
        Ok(Json(accepted))
    } else {
        Err(Fault::invalid_response())
    }
}

#[utoipa::path(post, path = "/api/v1/backtests", security(("adminBearer" = []), ("adminSession" = [])),
    params(("Idempotency-Key" = String, Header, description="Stable key, 1..128 visible ASCII characters"),
        ("Origin" = Option<String>, Header, description="Configured HTTPS origin with the session cookie"),
        ("X-CSRF-Token" = Option<String>, Header, description="Required with the session cookie")),
    request_body = ref("#/components/schemas/SubmitRequest"),
    responses((status=202,body=ref("#/components/schemas/Accepted")), (status=401,body=ErrorBody),
    (status=403,body=ErrorBody),(status=404,body=ErrorBody),(status=409,body=ErrorBody),(status=413,body=ErrorBody),
    (status=422,body=ErrorBody),(status=429,body=ErrorBody),(status=502,body=ErrorBody),
    (status=503,body=ErrorBody),(status=504,body=ErrorBody)))]
pub(crate) async fn submit(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<Submission>, JsonRejection>,
) -> Response {
    let Some(key) = lookup_key(&headers) else {
        return invalid("One Idempotency-Key with 1..128 visible ASCII characters is required");
    };
    let input = match body {
        Ok(Json(Submission::Legacy(input))) => {
            let Some(input) = input.into_request() else {
                return invalid("Invalid backtest submission");
            };
            models::SubmitRequest::JobRequest(Box::new(input))
        }
        Ok(Json(Submission::Bundle(input))) => {
            let Some(input) = input.into_request() else {
                return research::document_too_large();
            };
            models::SubmitRequest::BundleSubmission(Box::new(input))
        }
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return e.into_response(),
        Err(_) => return invalid("Invalid backtest submission"),
    };
    s.finance
        .write(|cfg| {
            let request = input.clone();
            async move { jobs_api::submit(&cfg, key, request).await }
        })
        .await
        .and_then(accepted_job)
        .map(|job| (StatusCode::ACCEPTED, job))
        .into_response()
}

#[utoipa::path(get, path = "/api/v1/backtests/by-key", security(("adminBearer" = []), ("adminSession" = [])),
    params(("Idempotency-Key" = String, Header, description="Exact attempted submission key")),
    responses((status=200,body=ref("#/components/schemas/Accepted")),(status=401,body=ErrorBody),
    (status=403,body=ErrorBody),(status=404,body=ErrorBody),(status=422,body=ErrorBody),
    (status=502,body=ErrorBody),(status=503,body=ErrorBody),(status=504,body=ErrorBody)))]
pub(crate) async fn by_key(State(s): State<AppState>, headers: HeaderMap) -> Response {
    let Some(key) = lookup_key(&headers) else {
        return invalid("One Idempotency-Key with 1..128 visible ASCII characters is required");
    };
    s.finance
        .read(|cfg| async move { jobs_api::by_key(&cfg, key).await })
        .await
        .and_then(accepted_job)
        .into_response()
}

#[utoipa::path(get, path = "/api/v1/backtests/datasets",
    responses((status=200,body=ref("#/components/schemas/DatasetList")),(status=502,body=ErrorBody),
        (status=503,body=ErrorBody),(status=504,body=ErrorBody)))]
pub(crate) async fn datasets(State(s): State<AppState>) -> Response {
    s.finance
        .read(|cfg| async move { research_api::datasets(&cfg).await })
        .await
        .map(Json)
        .into_response()
}

fn is_legacy(job: &models::Job) -> bool {
    matches!(job.request.as_ref(), models::JobRequestView::JobRequest(_))
}

#[utoipa::path(get, path = "/api/v1/backtests/{id}", params(("id" = i64, Path)),
    description = "Legacy jobs are public. StrategyBundle jobs are visible to administrators.",
    security((), ("adminBearer" = []), ("adminSession" = [])),
    responses((status=200,body=ref("#/components/schemas/Job")),(status=404,body=ErrorBody),
    (status=422,body=ErrorBody),(status=502,body=ErrorBody),(status=503,body=ErrorBody),(status=504,body=ErrorBody)))]
pub(crate) async fn detail(
    State(s): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<i64>, PathRejection>,
) -> Response {
    let Ok(Path(id)) = path else {
        return invalid("Invalid backtest id");
    };
    s.finance
        .read(|cfg| async move { jobs_api::detail(&cfg, id).await })
        .await
        .map(|job| {
            detail_response(
                job,
                s.write_access.authorize_stream(&headers, false).is_ok(),
            )
        })
        .into_response()
}

fn detail_response(job: models::Job, administrator: bool) -> Response {
    if !administrator && !is_legacy(&job) {
        return api_error(StatusCode::NOT_FOUND, "job_not_found", "No such job");
    }
    if job.id <= 0 {
        return Fault::invalid_response().into_response();
    }
    Json(job).into_response()
}

#[utoipa::path(get, path = "/api/v1/backtests",
    description = "Legacy jobs are public. StrategyBundle jobs are visible to administrators. Anonymous pages can also return a negative continuation when the visibility of the remaining jobs is unknown. Continue until next_cursor is null, even if items is empty.",
    security((), ("adminBearer" = []), ("adminSession" = [])),
    params(("limit" = Option<u32>, Query, description="1..100, default 20"),
        ("cursor" = Option<i64>, Query, description="Exclusive nonnegative int64 job position, or a continuation returned by an anonymous list")),
    responses((status=200,body=ref("#/components/schemas/Page")),(status=400,body=ErrorBody),(status=422,body=ErrorBody),
    (status=500,body=ErrorBody),(status=502,body=ErrorBody),(status=503,body=ErrorBody),(status=504,body=ErrorBody)))]
pub(crate) async fn list(
    State(s): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return invalid("Invalid pagination parameters");
    };
    let administrator = s.write_access.authorize_stream(&headers, false).is_ok();
    if administrator && query.cursor.is_some_and(|id| id < 0) {
        return InvalidPagination::Parameters.into_response();
    }
    let p = match query.pagination() {
        Ok(p) => p,
        Err(error) => return error.into_response(),
    };
    if administrator {
        return s
            .finance
            .read(|cfg| async move { jobs_api::list(&cfg, p.limit, p.cursor).await })
            .await
            .and_then(|page| {
                validate_page_ids(&page)?;
                Ok(Json(page))
            })
            .into_response();
    }

    let lookahead_budget = Duration::from_millis(s.config.server.request_timeout_ms) / 2;
    public_page(p, lookahead_budget, |limit, cursor| {
        let finance = &s.finance;
        async move {
            finance
                .read(|cfg| async move { jobs_api::list(&cfg, limit, cursor).await })
                .await
        }
    })
    .await
    .map(ScannedPage::into_response)
    .into_response()
}

struct ScannedPage {
    items: Vec<models::Job>,
    next: Continuation,
}

enum Continuation {
    End,
    PublicId(i64),
    Pending(i64),
}

impl ScannedPage {
    fn into_response(self) -> Response {
        let next_cursor = match self.next {
            Continuation::End => Ok(None),
            Continuation::PublicId(id) => Ok(Some(id)),
            Continuation::Pending(position) => cursor::issue(position).map(Some),
        };
        match next_cursor {
            Ok(next_cursor) => Json(models::Page {
                items: self.items,
                next_cursor: Some(next_cursor),
            })
            .into_response(),
            Err(()) => api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Could not issue a backtest cursor",
            ),
        }
    }
}

fn validate_page_ids(page: &models::Page) -> Result<(), Fault> {
    if page.items.iter().all(|job| job.id > 0) && page.next_cursor.flatten().is_none_or(|id| id > 0)
    {
        Ok(())
    } else {
        Err(Fault::invalid_response())
    }
}

async fn public_page<Fut>(
    query: Pagination,
    lookahead_budget: Duration,
    mut read: impl FnMut(Option<u32>, Option<i64>) -> Fut,
) -> Result<ScannedPage, Fault>
where
    Fut: Future<Output = Result<models::Page, Fault>> + Send,
{
    let limit = query.limit.unwrap_or(DEFAULT_PAGE_LIMIT) as usize;
    let mut upstream_limit = query.limit;
    let mut cursor = query.cursor;
    let mut items: Vec<models::Job> = Vec::with_capacity(limit);
    let deadline = Instant::now() + lookahead_budget;
    for index in 0..MAX_PUBLIC_PAGE_READS {
        let page = if index == 0 {
            read(upstream_limit, cursor).await?
        } else {
            if Instant::now() >= deadline {
                break;
            }
            // The initial page succeeded. Optional lookahead can only add public results.
            let Ok(Ok(page)) = timeout_at(deadline, read(upstream_limit, cursor)).await else {
                break;
            };
            page
        };
        validate_page_ids(&page)?;
        let next = page.next_cursor.flatten();
        let last = page.items.last().map(|job| job.id);
        let ordered = page
            .items
            .iter()
            .scan(cursor.unwrap_or(0), |previous, job| {
                let advances = job.id > *previous;
                *previous = job.id;
                Some(advances)
            })
            .all(|advances| advances);
        if !ordered || next.is_some_and(|next| last != Some(next)) {
            if index == 0 {
                return Err(Fault::invalid_response());
            }
            break;
        }
        for job in page.items {
            let public = is_legacy(&job);
            if public && items.len() == limit {
                return Ok(ScannedPage {
                    next: Continuation::PublicId(items.last().expect("nonempty public page").id),
                    items,
                });
            }
            cursor = Some(job.id);
            if public {
                items.push(job);
            } else {
                upstream_limit = Some(MAX_PAGE_LIMIT);
            }
        }
        if next.is_none() {
            return Ok(ScannedPage {
                items,
                next: Continuation::End,
            });
        }
    }
    Ok(ScannedPage {
        items,
        next: Continuation::Pending(cursor.ok_or_else(Fault::invalid_response)?),
    })
}

#[cfg(test)]
mod tests;
