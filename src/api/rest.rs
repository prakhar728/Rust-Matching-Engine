// REST API — axum HTTP handlers for the CLOB engine.
//
// All handlers share AppState = Arc<SharedState>, which contains both the
// matching engine (behind a Mutex) and the broadcast channel for WS events.
//
// Every mutating handler follows the same three-step pattern:
//   1. Lock engine, capture `seq_before` (last committed seq_id).
//   2. Run the engine operation, drop the lock.
//   3. Call `state.publish_since(seq_before)` — publishes all newly-appended
//      events to the WS broadcast channel.
//
// Authentication model (Phase -1):
//   Orders are self-authenticating — each carries a trader_id (hex pubkey) and
//   an ed25519 signature the engine verifies on every POST.
//   Cancel requests authenticate via the `X-Trader-Id` header.
//
// Error format:
//   { "code": "SNAKE_CASE_CODE", "message": "human readable" }
//
// Endpoints:
//   POST   /v1/orders                     — submit a signed order
//   DELETE /v1/orders/:order_id           — cancel by server-assigned order_id
//   POST   /v1/orders/cancel-by-client-id — cancel by (trader_id, client_order_id)
//   GET    /v1/orders/:order_id           — query order state
//   GET    /v1/books/:market_id           — L2 depth snapshot (?depth=N, default 20)
//   GET    /v1/trades/:market_id          — fills from the event log (?limit=N&from_seq=N)
//   GET    /v1/markets                    — list all registered markets

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};

use crate::api::{AppState, SharedState};
use crate::domain::market::MarketConfig;
use crate::domain::order::{Fill, Order, OrderId, OrderStatus, SignedOrder};
use crate::engine::{CancelError, Engine, EngineError, OrderResult};
use crate::ops::admin::{self, AdminError};
use crate::risk::RiskError;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Every API error is returned as JSON: `{ "code": "...", "message": "..." }`
#[derive(Debug, Serialize)]
struct ApiError {
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

struct ErrorResponse(StatusCode, ApiError);

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

/// Map every EngineError variant to the appropriate HTTP status + error code.
fn engine_err_to_response(e: EngineError) -> ErrorResponse {
    use crate::domain::order::OrderError;
    match e {
        EngineError::Order(ref oe) => {
            let (status, code) = match oe {
                OrderError::InvalidSignature => (StatusCode::UNAUTHORIZED, "INVALID_SIGNATURE"),
                OrderError::InvalidPublicKey => (StatusCode::BAD_REQUEST, "INVALID_PUBLIC_KEY"),
                OrderError::TraderIdMismatch => (StatusCode::BAD_REQUEST, "TRADER_ID_MISMATCH"),
                OrderError::MarketNotFound(_) => (StatusCode::NOT_FOUND, "MARKET_NOT_FOUND"),
                OrderError::MarketNotActive(_) => (StatusCode::CONFLICT, "MARKET_HALTED"),
                OrderError::InvalidTick { .. } => (StatusCode::BAD_REQUEST, "INVALID_TICK_SIZE"),
                OrderError::InvalidLot { .. } => (StatusCode::BAD_REQUEST, "INVALID_LOT_SIZE"),
                OrderError::Expired { .. } => (StatusCode::BAD_REQUEST, "ORDER_EXPIRED"),
                OrderError::ZeroPrice => (StatusCode::BAD_REQUEST, "ZERO_PRICE"),
                OrderError::ZeroSize => (StatusCode::BAD_REQUEST, "ZERO_SIZE"),
                OrderError::UnsupportedSchemaVersion(_) => {
                    (StatusCode::BAD_REQUEST, "UNSUPPORTED_SCHEMA_VERSION")
                }
                OrderError::EmptyField(_) => (StatusCode::BAD_REQUEST, "EMPTY_FIELD"),
                OrderError::NonceInvalid { .. } => (StatusCode::BAD_REQUEST, "NONCE_INVALID"),
                OrderError::DuplicateClientOrderId(_) => {
                    (StatusCode::CONFLICT, "DUPLICATE_CLIENT_ORDER_ID")
                }
            };
            ErrorResponse(status, ApiError::new(code, e.to_string()))
        }
        EngineError::Nonce(ref ne) => ErrorResponse(
            StatusCode::BAD_REQUEST,
            ApiError::new("NONCE_INVALID", ne.to_string()),
        ),
    }
}

fn risk_err_to_response(e: RiskError) -> ErrorResponse {
    match e {
        RiskError::RateLimitExceeded { .. } => ErrorResponse(
            StatusCode::TOO_MANY_REQUESTS,
            ApiError::new("RATE_LIMIT_EXCEEDED", e.to_string()),
        ),
        RiskError::OrderTooLarge { .. } => ErrorResponse(
            StatusCode::BAD_REQUEST,
            ApiError::new("ORDER_TOO_LARGE", e.to_string()),
        ),
        RiskError::PriceOutsideBand { .. } => ErrorResponse(
            StatusCode::BAD_REQUEST,
            ApiError::new("PRICE_OUTSIDE_BAND", e.to_string()),
        ),
    }
}

fn admin_err_to_response(e: AdminError) -> ErrorResponse {
    match e {
        AdminError::MarketNotFound(_) => ErrorResponse(
            StatusCode::NOT_FOUND,
            ApiError::new("MARKET_NOT_FOUND", e.to_string()),
        ),
        AdminError::AlreadyInState(_, _) => ErrorResponse(
            StatusCode::CONFLICT,
            ApiError::new("ALREADY_IN_STATE", e.to_string()),
        ),
        AdminError::Settled(_) => ErrorResponse(
            StatusCode::CONFLICT,
            ApiError::new("MARKET_SETTLED", e.to_string()),
        ),
    }
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct OrderAcceptedResponse {
    order_id: String,
    order_hash: String,
    status: OrderStatus,
    seq_id: u64,
    fill_count: usize,
    filled_qty: u64,
    is_duplicate: bool,
}

impl From<OrderResult> for OrderAcceptedResponse {
    fn from(r: OrderResult) -> Self {
        let filled_qty = r.fills.iter().map(|f| f.size_lots).sum();
        Self {
            order_id: r.order_id.0,
            order_hash: r.order_hash.to_hex(),
            status: r.final_status,
            seq_id: r.seq_id,
            fill_count: r.fills.len(),
            filled_qty,
            is_duplicate: r.is_duplicate,
        }
    }
}

#[derive(Serialize)]
struct CancelledResponse {
    order_id: String,
    seq_id: Option<u64>,
    was_already_closed: bool,
}

#[derive(Serialize)]
struct OrderStateResponse {
    order_id: String,
    client_order_id: String,
    order_hash: String,
    market_id: String,
    trader_id: String,
    side: crate::domain::order::Side,
    price_ticks: u64,
    orig_size_lots: u64,
    filled_size_lots: u64,
    remaining_size_lots: u64,
    status: OrderStatus,
    created_sequence: u64,
    last_update_sequence: u64,
    accepted_at_ms: u64,
}

impl From<&Order> for OrderStateResponse {
    fn from(o: &Order) -> Self {
        Self {
            order_id: o.order_id.0.clone(),
            client_order_id: o.client_order_id.clone(),
            order_hash: o.order_hash.to_hex(),
            market_id: o.market_id.0.clone(),
            trader_id: o.trader_id.clone(),
            side: o.side,
            price_ticks: o.price_ticks,
            orig_size_lots: o.orig_size_lots,
            filled_size_lots: o.filled_size_lots,
            remaining_size_lots: o.remaining_size_lots(),
            status: o.status,
            created_sequence: o.created_sequence,
            last_update_sequence: o.last_update_sequence,
            accepted_at_ms: o.accepted_at_ms,
        }
    }
}

#[derive(Serialize)]
struct BookLevel {
    price_ticks: u64,
    total_qty_lots: u64,
    order_count: usize,
}

#[derive(Serialize)]
struct BookResponse {
    market_id: String,
    bids: Vec<BookLevel>,
    asks: Vec<BookLevel>,
}

#[derive(Serialize)]
struct TradeResponse {
    sequence_id: u64,
    market_id: String,
    price_ticks: u64,
    size_lots: u64,
    taker_order_id: String,
    taker_trader_id: String,
    maker_order_id: String,
    maker_trader_id: String,
    executed_at_ms: u64,
}

impl From<&Fill> for TradeResponse {
    fn from(f: &Fill) -> Self {
        Self {
            sequence_id: f.sequence_id,
            market_id: f.market_id.0.clone(),
            price_ticks: f.price_ticks,
            size_lots: f.size_lots,
            taker_order_id: f.taker_order_id.0.clone(),
            taker_trader_id: f.taker_trader_id.clone(),
            maker_order_id: f.maker_order_id.0.clone(),
            maker_trader_id: f.maker_trader_id.clone(),
            executed_at_ms: f.executed_at_ms,
        }
    }
}

#[derive(Serialize)]
struct MarketResponse {
    market_id: String,
    base_asset: String,
    quote_asset: String,
    tick_size: u64,
    lot_size: u64,
    fee_bps_maker: u64,
    fee_bps_taker: u64,
    status: crate::domain::market::MarketStatus,
}

impl From<&MarketConfig> for MarketResponse {
    fn from(m: &MarketConfig) -> Self {
        Self {
            market_id: m.id.0.clone(),
            base_asset: m.base_asset.clone(),
            quote_asset: m.quote_asset.clone(),
            tick_size: m.tick_size,
            lot_size: m.lot_size,
            fee_bps_maker: m.fee_bps_maker,
            fee_bps_taker: m.fee_bps_taker,
            status: m.status,
        }
    }
}

// ---------------------------------------------------------------------------
// Query param / request body types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BookQuery {
    #[serde(default = "default_depth")]
    depth: usize,
}

fn default_depth() -> usize {
    20
}

#[derive(Deserialize)]
struct TradesQuery {
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    from_seq: u64,
}

fn default_limit() -> usize {
    100
}

#[derive(Deserialize)]
struct CancelByClientIdRequest {
    trader_id: String,
    client_order_id: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /v1/orders
async fn post_order(
    State(state): State<AppState>,
    Json(signed): Json<SignedOrder>,
) -> Result<(StatusCode, Json<OrderAcceptedResponse>), ErrorResponse> {
    // ── Pre-trade risk check (runs outside the engine lock) ───────────────
    {
        let mut risk = state.risk.lock().await;
        risk.check(&signed.trader_id, &signed.market_id.0, signed.price_ticks, signed.size_lots)
            .map_err(risk_err_to_response)?;
    }

    // ── Engine call ───────────────────────────────────────────────────────
    let (result, seq_before) = {
        let mut engine = state.engine.lock().await;
        let seq_before = engine.sequencer.peek_next_seq_id().saturating_sub(1);
        let result = engine.process_order(signed).map_err(engine_err_to_response)?;
        (result, seq_before)
    };

    // ── Update reference prices for any fills produced ────────────────────
    if !result.fills.is_empty() {
        let mut risk = state.risk.lock().await;
        for fill in &result.fills {
            risk.update_reference_price(&fill.market_id.0, fill.price_ticks);
        }
    }

    // Lock released — now safe to publish and persist.
    state.publish_since(seq_before);
    state.persist_since(seq_before).await;

    let status = if result.is_duplicate { StatusCode::OK } else { StatusCode::CREATED };
    Ok((status, Json(OrderAcceptedResponse::from(result))))
}

/// DELETE /v1/orders/:order_id
async fn delete_order(
    State(state): State<AppState>,
    Path(order_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<CancelledResponse>, ErrorResponse> {
    let trader_id = headers
        .get("X-Trader-Id")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            ErrorResponse(
                StatusCode::BAD_REQUEST,
                ApiError::new("MISSING_TRADER_ID", "X-Trader-Id header is required"),
            )
        })?
        .to_string();

    let oid = OrderId(order_id);
    let (result, seq_before) = {
        let mut engine = state.engine.lock().await;
        let seq_before = engine.sequencer.peek_next_seq_id().saturating_sub(1);
        let result = engine.process_cancel(&oid, &trader_id).map_err(|e| match e {
            CancelError::NotFound(id) => ErrorResponse(
                StatusCode::NOT_FOUND,
                ApiError::new("ORDER_NOT_FOUND", format!("order {id} not found")),
            ),
            CancelError::Unauthorized(tid) => ErrorResponse(
                StatusCode::FORBIDDEN,
                ApiError::new(
                    "UNAUTHORIZED_CANCEL",
                    format!("trader {tid} does not own this order"),
                ),
            ),
        })?;
        (result, seq_before)
    };

    state.publish_since(seq_before);
    state.persist_since(seq_before).await;

    Ok(Json(CancelledResponse {
        order_id: result.order_id.0,
        seq_id: result.seq_id,
        was_already_closed: result.was_already_closed,
    }))
}

/// POST /v1/orders/cancel-by-client-id
async fn cancel_by_client_id(
    State(state): State<AppState>,
    Json(body): Json<CancelByClientIdRequest>,
) -> Result<Json<CancelledResponse>, ErrorResponse> {
    let (result, seq_before) = {
        let mut engine = state.engine.lock().await;
        let seq_before = engine.sequencer.peek_next_seq_id().saturating_sub(1);

        let order_id = engine
            .sequencer
            .idempotency_check(&body.trader_id, &body.client_order_id)
            .map(|rec| rec.order_id.clone())
            .ok_or_else(|| {
                ErrorResponse(
                    StatusCode::NOT_FOUND,
                    ApiError::new("ORDER_NOT_FOUND", "no order found for this client_order_id"),
                )
            })?;

        let result = engine.process_cancel(&order_id, &body.trader_id).map_err(|e| match e {
            CancelError::NotFound(id) => ErrorResponse(
                StatusCode::NOT_FOUND,
                ApiError::new("ORDER_NOT_FOUND", format!("order {id} not found")),
            ),
            CancelError::Unauthorized(tid) => ErrorResponse(
                StatusCode::FORBIDDEN,
                ApiError::new(
                    "UNAUTHORIZED_CANCEL",
                    format!("trader {tid} does not own this order"),
                ),
            ),
        })?;
        (result, seq_before)
    };

    state.publish_since(seq_before);
    state.persist_since(seq_before).await;

    Ok(Json(CancelledResponse {
        order_id: result.order_id.0,
        seq_id: result.seq_id,
        was_already_closed: result.was_already_closed,
    }))
}

/// GET /v1/orders/:order_id
async fn get_order(
    State(state): State<AppState>,
    Path(order_id): Path<String>,
) -> Result<Json<OrderStateResponse>, ErrorResponse> {
    let engine = state.engine.lock().await;
    let order = engine.get_order(&OrderId(order_id.clone())).ok_or_else(|| {
        ErrorResponse(
            StatusCode::NOT_FOUND,
            ApiError::new("ORDER_NOT_FOUND", format!("order {order_id} not found")),
        )
    })?;
    Ok(Json(OrderStateResponse::from(order)))
}

/// GET /v1/books/:market_id?depth=N
async fn get_book(
    State(state): State<AppState>,
    Path(market_id): Path<String>,
    Query(params): Query<BookQuery>,
) -> Result<Json<BookResponse>, ErrorResponse> {
    let depth = params.depth.min(200);
    let engine = state.engine.lock().await;
    let (bids, asks) = engine.l2_snapshot(&market_id, depth).ok_or_else(|| {
        ErrorResponse(
            StatusCode::NOT_FOUND,
            ApiError::new("MARKET_NOT_FOUND", format!("market {market_id} not found")),
        )
    })?;

    Ok(Json(BookResponse {
        market_id,
        bids: bids
            .into_iter()
            .map(|l| BookLevel {
                price_ticks: l.price,
                total_qty_lots: l.total_qty,
                order_count: l.order_count,
            })
            .collect(),
        asks: asks
            .into_iter()
            .map(|l| BookLevel {
                price_ticks: l.price,
                total_qty_lots: l.total_qty,
                order_count: l.order_count,
            })
            .collect(),
    }))
}

/// GET /v1/trades/:market_id?limit=N&from_seq=N
async fn get_trades(
    State(state): State<AppState>,
    Path(market_id): Path<String>,
    Query(params): Query<TradesQuery>,
) -> Result<Json<Vec<TradeResponse>>, ErrorResponse> {
    let limit = params.limit.min(1000);
    let engine = state.engine.lock().await;

    if engine.best_prices(&market_id).is_none() {
        return Err(ErrorResponse(
            StatusCode::NOT_FOUND,
            ApiError::new("MARKET_NOT_FOUND", format!("market {market_id} not found")),
        ));
    }

    let fills: Vec<TradeResponse> = engine
        .get_fills(&market_id, params.from_seq, limit)
        .into_iter()
        .map(TradeResponse::from)
        .collect();

    Ok(Json(fills))
}

/// GET /v1/markets
async fn list_markets(State(state): State<AppState>) -> Json<Vec<MarketResponse>> {
    let engine = state.engine.lock().await;
    let markets: Vec<MarketResponse> = engine.list_markets().iter().map(|m| (*m).into()).collect();
    Json(markets)
}

// ---------------------------------------------------------------------------
// Admin handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AdminActionRequest {
    triggered_by: String,
}

#[derive(Serialize)]
struct AdminPauseResumeResponse {
    market_id: String,
    seq_id: u64,
}

#[derive(Serialize)]
struct AdminCancelAllResponse {
    market_id: String,
    cancelled_count: usize,
}

/// POST /v1/admin/markets/:market_id/pause
async fn admin_pause_market(
    State(state): State<AppState>,
    Path(market_id): Path<String>,
    Json(body): Json<AdminActionRequest>,
) -> Result<Json<AdminPauseResumeResponse>, ErrorResponse> {
    let (seq_id, seq_before) = {
        let mut engine = state.engine.lock().await;
        let seq_before = engine.sequencer.peek_next_seq_id().saturating_sub(1);
        let seq_id = admin::pause_market(&mut engine, &market_id, &body.triggered_by)
            .map_err(admin_err_to_response)?;
        (seq_id, seq_before)
    };
    state.publish_since(seq_before);
    state.persist_since(seq_before).await;
    Ok(Json(AdminPauseResumeResponse { market_id, seq_id }))
}

/// POST /v1/admin/markets/:market_id/resume
async fn admin_resume_market(
    State(state): State<AppState>,
    Path(market_id): Path<String>,
    Json(body): Json<AdminActionRequest>,
) -> Result<Json<AdminPauseResumeResponse>, ErrorResponse> {
    let (seq_id, seq_before) = {
        let mut engine = state.engine.lock().await;
        let seq_before = engine.sequencer.peek_next_seq_id().saturating_sub(1);
        let seq_id = admin::resume_market(&mut engine, &market_id, &body.triggered_by)
            .map_err(admin_err_to_response)?;
        (seq_id, seq_before)
    };
    state.publish_since(seq_before);
    state.persist_since(seq_before).await;
    Ok(Json(AdminPauseResumeResponse { market_id, seq_id }))
}

/// POST /v1/admin/markets/:market_id/cancel-all
async fn admin_cancel_all(
    State(state): State<AppState>,
    Path(market_id): Path<String>,
) -> Result<Json<AdminCancelAllResponse>, ErrorResponse> {
    let (count, seq_before) = {
        let mut engine = state.engine.lock().await;
        let seq_before = engine.sequencer.peek_next_seq_id().saturating_sub(1);
        let count = admin::cancel_all_orders(&mut engine, &market_id)
            .map_err(admin_err_to_response)?;
        (count, seq_before)
    };
    state.publish_since(seq_before);
    state.persist_since(seq_before).await;
    Ok(Json(AdminCancelAllResponse { market_id, cancelled_count: count }))
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the axum router with all REST routes and the shared state.
///
/// The `cancel-by-client-id` route MUST appear before `/:order_id` so axum
/// doesn't capture the literal string "cancel-by-client-id" as an order ID.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/orders", post(post_order))
        .route("/v1/orders/cancel-by-client-id", post(cancel_by_client_id))
        .route("/v1/orders/:order_id", delete(delete_order).get(get_order))
        .route("/v1/books/:market_id", get(get_book))
        .route("/v1/trades/:market_id", get(get_trades))
        .route("/v1/markets", get(list_markets))
        // Admin endpoints — privileged, no auth in Phase 0
        .route("/v1/admin/markets/:market_id/pause", post(admin_pause_market))
        .route("/v1/admin/markets/:market_id/resume", post(admin_resume_market))
        .route("/v1/admin/markets/:market_id/cancel-all", post(admin_cancel_all))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    use crate::domain::market::{MarketConfig, MarketId, MarketStatus};
    use crate::domain::order::{Side, TimeInForce, CURRENT_SCHEMA_VERSION};
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    fn test_state() -> AppState {
        let mut engine = Engine::with_clock(
            Box::new(|| 1_000_000u64) as Box<dyn Fn() -> u64 + Send + Sync>,
        );
        engine.add_market(MarketConfig {
            id: MarketId("BTC-USDC".into()),
            base_asset: "BTC".into(),
            quote_asset: "USDC".into(),
            tick_size: 1_000,
            lot_size: 1,
            fee_bps_maker: 10,
            fee_bps_taker: 20,
            status: MarketStatus::Active,
        });
        SharedState::new(engine)
    }

    fn make_app(state: AppState) -> Router {
        build_router(state)
    }

    fn signed_order(
        key: &SigningKey,
        market_id: &str,
        side: Side,
        price: u64,
        size: u64,
        nonce: u64,
        client_order_id: &str,
    ) -> SignedOrder {
        let pubkey = key.verifying_key().to_bytes();
        let mut order = SignedOrder {
            schema_version: CURRENT_SCHEMA_VERSION,
            client_order_id: client_order_id.to_string(),
            market_id: MarketId(market_id.to_string()),
            trader_id: hex::encode(pubkey),
            side,
            price_ticks: price,
            size_lots: size,
            time_in_force: TimeInForce::Gtc,
            nonce,
            expiry_ts_ms: 0,
            created_at_ms: 1_000_000,
            salt: 0,
            trader_pubkey: pubkey,
            signature: [0u8; 64],
        };
        let hash = order.canonical_hash();
        order.signature = key.sign(&hash.0).to_bytes();
        order
    }

    async fn post_order_json(app: &Router, body: &SignedOrder) -> axum::response::Response {
        let json = serde_json::to_vec(body).unwrap();
        app.clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/orders")
                    .header("content-type", "application/json")
                    .body(Body::from(json))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap()
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn post_order_returns_201_on_new_order() {
        let key = SigningKey::generate(&mut OsRng);
        let order = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 1, "cid-1");
        let app = make_app(test_state());
        let resp = post_order_json(&app, &order).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = body_json(resp).await;
        assert_eq!(body["status"], "open");
        assert_eq!(body["is_duplicate"], false);
        assert!(!body["order_id"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn post_order_duplicate_returns_200_with_flag() {
        let key = SigningKey::generate(&mut OsRng);
        let order = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 1, "cid-dup");
        let state = test_state();
        let app = make_app(state);
        let r1 = post_order_json(&app, &order).await;
        assert_eq!(r1.status(), StatusCode::CREATED);
        let r2 = post_order_json(&app, &order).await;
        assert_eq!(r2.status(), StatusCode::OK);
        let body = body_json(r2).await;
        assert_eq!(body["is_duplicate"], true);
    }

    #[tokio::test]
    async fn post_order_invalid_signature_returns_401() {
        let key = SigningKey::generate(&mut OsRng);
        let mut order =
            signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 1, "cid-badsig");
        order.signature[0] ^= 0xff;
        let app = make_app(test_state());
        let resp = post_order_json(&app, &order).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn post_order_unknown_market_returns_404() {
        let key = SigningKey::generate(&mut OsRng);
        let order = signed_order(&key, "UNKNOWN-MKT", Side::Buy, 1_000, 1, 1, "cid-nomarket");
        let app = make_app(test_state());
        let resp = post_order_json(&app, &order).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_order_returns_order_state() {
        let key = SigningKey::generate(&mut OsRng);
        let order = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 1, "cid-get");
        let state = test_state();
        let app = make_app(state);
        let post_body = body_json(post_order_json(&app, &order).await).await;
        let order_id = post_body["order_id"].as_str().unwrap().to_string();

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/orders/{order_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["order_id"], order_id);
        assert_eq!(body["status"], "open");
        assert_eq!(body["price_ticks"], 50_000_000);
    }

    #[tokio::test]
    async fn get_order_unknown_id_returns_404() {
        let app = make_app(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/orders/does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_order_cancels_and_returns_200() {
        let key = SigningKey::generate(&mut OsRng);
        let order = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 1, "cid-cancel");
        let trader_id = hex::encode(key.verifying_key().to_bytes());
        let state = test_state();
        let app = make_app(state);

        let post_body = body_json(post_order_json(&app, &order).await).await;
        let order_id = post_body["order_id"].as_str().unwrap().to_string();

        let del_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/v1/orders/{order_id}"))
                    .header("X-Trader-Id", &trader_id)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(del_resp.status(), StatusCode::OK);
        let body = body_json(del_resp).await;
        assert_eq!(body["was_already_closed"], false);
        assert!(body["seq_id"].as_u64().is_some());
    }

    #[tokio::test]
    async fn delete_order_wrong_trader_returns_403() {
        let key = SigningKey::generate(&mut OsRng);
        let order = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 1, "cid-unauth");
        let state = test_state();
        let app = make_app(state);
        let post_body = body_json(post_order_json(&app, &order).await).await;
        let order_id = post_body["order_id"].as_str().unwrap().to_string();

        let del_resp = app
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/v1/orders/{order_id}"))
                    .header("X-Trader-Id", "wrong-trader")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del_resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn get_book_returns_bids_and_asks() {
        let key = SigningKey::generate(&mut OsRng);
        let buy = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 5, 1, "cid-bid");
        let key2 = SigningKey::generate(&mut OsRng);
        let sell = signed_order(&key2, "BTC-USDC", Side::Sell, 51_000_000, 3, 1, "cid-ask");

        let state = test_state();
        let app = make_app(state);
        post_order_json(&app, &buy).await;
        post_order_json(&app, &sell).await;

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/books/BTC-USDC?depth=5")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["market_id"], "BTC-USDC");
        assert_eq!(body["bids"][0]["price_ticks"], 50_000_000);
        assert_eq!(body["asks"][0]["price_ticks"], 51_000_000);
    }

    #[tokio::test]
    async fn get_book_unknown_market_returns_404() {
        let app = make_app(test_state());
        let resp = app
            .oneshot(
                Request::builder().uri("/v1/books/NOEXIST").body(Body::empty()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_markets_returns_list() {
        let app = make_app(test_state());
        let resp = app
            .oneshot(
                Request::builder().uri("/v1/markets").body(Body::empty()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let markets = body.as_array().unwrap();
        assert_eq!(markets.len(), 1);
        assert_eq!(markets[0]["market_id"], "BTC-USDC");
    }

    #[tokio::test]
    async fn get_trades_returns_fills_after_match() {
        let seller = SigningKey::generate(&mut OsRng);
        let buyer = SigningKey::generate(&mut OsRng);
        let sell = signed_order(&seller, "BTC-USDC", Side::Sell, 50_000_000, 2, 1, "cid-sell");
        let buy = signed_order(&buyer, "BTC-USDC", Side::Buy, 50_000_000, 2, 1, "cid-buy");
        let state = test_state();
        let app = make_app(state);
        post_order_json(&app, &sell).await;
        post_order_json(&app, &buy).await;

        let resp = app
            .oneshot(
                Request::builder().uri("/v1/trades/BTC-USDC").body(Body::empty()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let trades = body.as_array().unwrap();
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0]["price_ticks"], 50_000_000);
        assert_eq!(trades[0]["size_lots"], 2);
    }

    #[tokio::test]
    async fn cancel_by_client_id_works() {
        let key = SigningKey::generate(&mut OsRng);
        let order = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 1, "cid-cbci");
        let trader_id = hex::encode(key.verifying_key().to_bytes());
        let state = test_state();
        let app = make_app(state);
        post_order_json(&app, &order).await;

        let cancel_body = serde_json::json!({
            "trader_id": trader_id,
            "client_order_id": "cid-cbci"
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/orders/cancel-by-client-id")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&cancel_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["was_already_closed"], false);
    }
}
