//! Process-local market selection and subscription limit for the v1 administrator.

use std::sync::{Arc, RwLock};

use axum::{
    Extension, Json, Router,
    extract::{State, rejection::JsonRejection},
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use finance_client::models::SubscriptionDemandState;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use utoipa::{OpenApi, ToSchema};

use crate::{AppState, ErrorBody, api_error, market_types::InstrumentId, watchlists};

pub(crate) const DEPTH_FOCUS: &str = "/api/v1/market/depth-focus";
pub(crate) const SUBSCRIPTIONS: &str = "/api/v1/market/subscriptions";
const SUBSCRIPTION_LIMIT: usize = 8;

/// Browser sessions and bearer requests share the single v1 administrator's allowance.
pub struct MarketPreferences {
    focus: RwLock<Option<InstrumentId>>,
    slots: Arc<Semaphore>,
    // F15 replaces both fields. Serialize local mutations with their upstream PUTs.
    pub(crate) demand_updates: Mutex<()>,
}

impl Default for MarketPreferences {
    fn default() -> Self {
        Self {
            focus: RwLock::new(None),
            slots: Arc::new(Semaphore::new(SUBSCRIPTION_LIMIT)),
            demand_updates: Mutex::new(()),
        }
    }
}

impl MarketPreferences {
    pub(crate) fn focus(&self) -> Option<InstrumentId> {
        self.focus
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn update_focus(&self, request: DepthFocus) -> FocusSelection {
        let mut focus = self
            .focus
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *focus = match request {
            DepthFocus::Set { instrument_id } => Some(instrument_id),
            DepthFocus::Clear => None,
        };
        FocusSelection {
            instrument_id: focus.clone(),
        }
    }

    pub(crate) fn acquire(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.slots).try_acquire_owned().ok()
    }

    pub(crate) async fn forward_demand(
        &self,
        state: &AppState,
    ) -> Result<[(&'static str, HeaderValue); 1], Response> {
        let watchlist = watchlists::demand_ids(&state.db).await?;
        let focus = self.focus().map(|id| id.as_str().to_owned());
        let state = state
            .finance
            .put_subscription_demand(watchlist, focus)
            .await
            .map_err(IntoResponse::into_response)?;
        let value = match state {
            SubscriptionDemandState::Enabled => "enabled",
            SubscriptionDemandState::Disabled => "disabled",
        };
        Ok([(
            "x-finance-subscription-state",
            HeaderValue::from_static(value),
        )])
    }

    fn quota(&self) -> SubscriptionQuota {
        let available = self.slots.available_permits();
        SubscriptionQuota {
            limit: SUBSCRIPTION_LIMIT,
            active: SUBSCRIPTION_LIMIT - available,
            available,
        }
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum DepthFocus {
    Set { instrument_id: InstrumentId },
    Clear,
}

#[derive(Serialize, ToSchema)]
struct FocusSelection {
    #[schema(required)]
    instrument_id: Option<InstrumentId>,
}

#[derive(Serialize, ToSchema)]
struct SubscriptionQuota {
    limit: usize,
    active: usize,
    available: usize,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum FinanceLineState {
    FinanceContractPending,
}

#[derive(Serialize, ToSchema)]
struct Subscriptions {
    /// Allocated market subscriptions in this web process.
    web: SubscriptionQuota,
    /// Legacy W2a marker. Read the broker budget at /api/v1/market/subscriptions/budget.
    finance_lines: FinanceLineState,
}

#[utoipa::path(get, path = "/api/v1/market/depth-focus", operation_id = "getMarketDepthFocus",
    security(("adminSession" = []), ("adminBearer" = [])),
    responses((status = 200, body = FocusSelection), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 429, body = ErrorBody)))]
async fn get_focus(
    Extension(preferences): Extension<Arc<MarketPreferences>>,
) -> Json<FocusSelection> {
    Json(FocusSelection {
        instrument_id: preferences.focus(),
    })
}

#[utoipa::path(put, path = "/api/v1/market/depth-focus", operation_id = "setMarketDepthFocus",
    description = "Saves the focus before submitting the complete subscription demand to finance. Forwarding errors leave the saved focus in place.",
    params(("X-CSRF-Token" = Option<String>, Header, description = "Required for browser sessions")),
    request_body = DepthFocus, security(("adminSession" = []), ("adminBearer" = [])),
    responses((status = 200, body = FocusSelection,
        headers(("X-Finance-Subscription-State" = String, description = "Finance collection state: enabled or disabled"))),
        (status = 400, body = ErrorBody),
        (status = 401, body = ErrorBody), (status = 403, body = ErrorBody),
        (status = 413, body = ErrorBody), (status = 429, body = ErrorBody),
        (status = 502, body = ErrorBody), (status = 503, body = ErrorBody),
        (status = 504, body = ErrorBody)))]
async fn put_focus(
    State(state): State<AppState>,
    Extension(preferences): Extension<Arc<MarketPreferences>>,
    request: Result<Json<DepthFocus>, JsonRejection>,
) -> Result<impl IntoResponse, Response> {
    let Json(request) = request.map_err(|error| {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            api_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "Request body is too large",
            )
        } else {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid_market_request",
                "Invalid depth focus request",
            )
        }
    })?;
    let _update = preferences.demand_updates.lock().await;
    let selection = preferences.update_focus(request);
    let headers = preferences.forward_demand(&state).await?;
    Ok((headers, Json(selection)))
}

#[utoipa::path(get, path = "/api/v1/market/subscriptions", operation_id = "marketSubscriptions",
    security(("adminSession" = []), ("adminBearer" = [])),
    responses((status = 200, body = Subscriptions), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 429, body = ErrorBody)))]
async fn subscriptions(
    Extension(preferences): Extension<Arc<MarketPreferences>>,
) -> Json<Subscriptions> {
    Json(Subscriptions {
        web: preferences.quota(),
        finance_lines: FinanceLineState::FinanceContractPending,
    })
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route(DEPTH_FOCUS, get(get_focus).put(put_focus))
        .route(SUBSCRIPTIONS, get(subscriptions))
}

#[derive(OpenApi)]
#[openapi(
    paths(get_focus, put_focus, subscriptions),
    components(schemas(
        DepthFocus,
        FocusSelection,
        SubscriptionQuota,
        FinanceLineState,
        Subscriptions,
    ))
)]
pub(crate) struct MarketPreferencesApi;
