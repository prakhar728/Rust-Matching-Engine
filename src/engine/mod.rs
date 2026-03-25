// Engine — the central coordinator that wires all modules together.
//
// The Engine is the single entry point for all state-changing operations.
// It enforces the following pipeline for every order submission:
//
//   SignedOrder
//     │
//     ├─ 1. validate_fields()     — stateless shape check (schema, price > 0, etc.)
//     ├─ 2. verify_signature()    — ed25519 sig over canonical hash
//     ├─ 3. trader_id check       — hex(pubkey) must equal trader_id
//     ├─ 4. market check          — market exists and is Active
//     ├─ 5. tick/lot alignment    — price % tick_size == 0, size % lot_size == 0
//     ├─ 6. idempotency check     — duplicate client_order_id → return original result
//     ├─ 7. nonce policy          — nonce must be > last accepted nonce for this trader
//     ├─ 8. sequence OrderAccepted — assigns seq_id, appends to log
//     ├─ 9. match_order()         — price-time priority matching, STP
//     ├─ 10. sequence Fill events  — one per maker touched
//     ├─ 11. sequence OrderCancelled if STP fired
//     └─ 12. insert residual into book if any remaining qty
//
// All state mutations happen through the Sequencer (step 8+), so the event
// log is always consistent with the in-memory state.

pub mod matching;
pub mod orderbook;

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::db::{NonceError, StateStore};
use crate::domain::market::MarketConfig;
use crate::domain::order::{
    Fill, Order, OrderError, OrderHash, OrderId, OrderStatus, SignedOrder,
};
use crate::engine::matching::match_order;
use crate::engine::orderbook::OrderBook;
use crate::events::{CancelReason, Event, SequencedEvent};
use crate::sequencer::Sequencer;

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// Returned by `process_order` on success.
#[derive(Debug)]
pub struct OrderResult {
    /// Server-assigned order ID (UUIDv4).
    pub order_id: OrderId,
    /// Canonical SHA-256 hash of the order — the stable on-chain key.
    pub order_hash: OrderHash,
    /// Sequence number of the OrderAccepted event in the log.
    pub seq_id: u64,
    /// Fills produced immediately on intake (empty if order rested on the book).
    pub fills: Vec<Fill>,
    /// The order's status after this call (Open, PartiallyFilled, Filled, CancelledStp).
    pub final_status: OrderStatus,
    /// True if this was a duplicate submission — original result returned, no new state.
    pub is_duplicate: bool,
}

/// Returned by `process_cancel` on success.
#[derive(Debug)]
pub struct CancelResult {
    pub order_id: OrderId,
    /// Sequence number of the OrderCancelled event. None if already closed (idempotent).
    pub seq_id: Option<u64>,
    /// True if the order was already filled or cancelled before this call.
    pub was_already_closed: bool,
}

/// Errors from cancel operations.
#[derive(Debug, thiserror::Error)]
pub enum CancelError {
    #[error("order not found: {0}")]
    NotFound(String),
    #[error("trader '{0}' does not own this order")]
    Unauthorized(String),
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

pub struct Engine {
    /// One OrderBook per market (keyed by market_id string).
    books: HashMap<String, OrderBook>,

    /// Market configurations — loaded at startup, updated by admin operations.
    markets: HashMap<String, MarketConfig>,

    /// Global monotonic event sequencer + idempotency table.
    pub sequencer: Sequencer,

    /// NEAR-compatible state store (filled_amounts, cancelled, nonces, balances).
    pub state: StateStore,

    /// Fast O(1) lookup: order_id → market_id.
    /// Populated when an order is accepted; never cleaned up (orders kept for audit).
    order_market_index: HashMap<OrderId, String>,

    /// Provides current epoch milliseconds. Injected so tests can use a fixed clock
    /// and avoid any wall-clock dependency in state transitions.
    ///
    /// `Send + Sync` bounds are required so Engine can be placed in Arc<Mutex<Engine>>
    /// and shared across async axum handler tasks.
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    /// Create a new engine with the system clock.
    pub fn new() -> Self {
        Self::with_clock(Box::new(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64
        }) as Box<dyn Fn() -> u64 + Send + Sync>)
    }

    /// Create an engine with an injected clock — use this in tests for determinism.
    ///
    /// Example:
    /// ```ignore
    /// let engine = Engine::with_clock(Box::new(|| 1_000_000));
    /// ```
    pub fn with_clock(clock: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self {
            books: HashMap::new(),
            markets: HashMap::new(),
            sequencer: Sequencer::new(),
            state: StateStore::new(),
            order_market_index: HashMap::new(),
            clock,
        }
    }

    /// Register a market. Must be called before any orders for that market can be placed.
    /// Safe to call multiple times — overwrites the existing config (e.g. for status updates).
    pub fn add_market(&mut self, config: MarketConfig) {
        let id = config.id.0.clone();
        self.books.entry(id.clone()).or_default();
        self.markets.insert(id, config);
    }

    // -----------------------------------------------------------------------
    // Order submission
    // -----------------------------------------------------------------------

    /// Submit a signed order for validation, sequencing, and matching.
    ///
    /// See module-level comment for the full pipeline.
    /// Returns `OrderResult` on success, `OrderError` on any validation failure.
    pub fn process_order(&mut self, signed: SignedOrder) -> Result<OrderResult, EngineError> {
        let now_ms = (self.clock)();

        // ── 1. Stateless field validation ────────────────────────────────────
        signed.validate_fields(now_ms)?;

        // ── 2. Signature verification ─────────────────────────────────────────
        signed.verify_signature()?;

        // ── 3. trader_id must equal hex(trader_pubkey) ───────────────────────
        // This binds the human-readable identity to the cryptographic key.
        let expected_trader_id = hex::encode(signed.trader_pubkey);
        if signed.trader_id != expected_trader_id {
            return Err(EngineError::Order(OrderError::TraderIdMismatch));
        }

        // ── 4. Market must exist and be active ───────────────────────────────
        let market = self
            .markets
            .get(&signed.market_id.0)
            .ok_or_else(|| OrderError::MarketNotFound(signed.market_id.0.clone()))?;

        if !market.is_active() {
            return Err(EngineError::Order(OrderError::MarketNotActive(
                signed.market_id.0.clone(),
            )));
        }

        // ── 5. Tick and lot alignment ─────────────────────────────────────────
        market
            .validate_price(signed.price_ticks)
            .map_err(|_| EngineError::Order(OrderError::InvalidTick {
                price: signed.price_ticks,
                tick_size: market.tick_size,
            }))?;

        market
            .validate_size(signed.size_lots)
            .map_err(|_| EngineError::Order(OrderError::InvalidLot {
                size: signed.size_lots,
                lot_size: market.lot_size,
            }))?;

        // ── 6. Idempotency check ──────────────────────────────────────────────
        // If the same (trader_id, client_order_id) was already accepted, return
        // the original result without creating a new order.
        if let Some(record) = self
            .sequencer
            .idempotency_check(&signed.trader_id, &signed.client_order_id)
        {
            let status = self
                .find_order(&record.order_id)
                .map(|o| o.status)
                .unwrap_or(OrderStatus::Filled); // if not in book, assume filled

            return Ok(OrderResult {
                order_id: record.order_id.clone(),
                order_hash: record.order_hash,
                seq_id: record.original_seq_id,
                fills: vec![],
                final_status: status,
                is_duplicate: true,
            });
        }

        // ── 7. Nonce policy ───────────────────────────────────────────────────
        // Nonce must be strictly greater than the last accepted nonce.
        self.state
            .check_and_update_nonce(&signed.trader_id, signed.nonce)
            .map_err(EngineError::Nonce)?;

        // ── 8. Build Order and sequence the OrderAccepted event ───────────────
        let order_hash = signed.canonical_hash();
        let order_id = OrderId(Uuid::new_v4().to_string());

        // Peek the next seq_id so we can embed it in the Order before appending.
        // This means the stored order in the event already has the correct seq.
        let seq_id = self.sequencer.peek_next_seq_id();

        let mut order = Order {
            order_id: order_id.clone(),
            client_order_id: signed.client_order_id.clone(),
            order_hash,
            market_id: signed.market_id.clone(),
            trader_id: signed.trader_id.clone(),
            trader_pubkey: signed.trader_pubkey,
            side: signed.side,
            price_ticks: signed.price_ticks,
            orig_size_lots: signed.size_lots,
            filled_size_lots: 0,
            time_in_force: signed.time_in_force,
            nonce: signed.nonce,
            expiry_ts_ms: signed.expiry_ts_ms,
            status: OrderStatus::Open,
            created_sequence: seq_id,
            last_update_sequence: seq_id,
            accepted_at_ms: now_ms,
        };

        // Append the event — this is the point of no return.
        // After this, the order is in the log and must be processed to completion.
        self.sequencer
            .append_event(Event::OrderAccepted { order: order.clone() });

        // Register in idempotency table so duplicates are detected in future calls.
        self.sequencer.register_accepted(
            &signed.trader_id,
            &signed.client_order_id,
            seq_id,
            order_id.clone(),
            order_hash,
            now_ms,
        );

        // Track order → market mapping for O(1) lookup.
        self.order_market_index
            .insert(order_id.clone(), signed.market_id.0.clone());

        // ── 9. Run the matching engine ────────────────────────────────────────
        let book = self.books.get_mut(&signed.market_id.0).unwrap();
        let fills = match_order(book, &mut order, seq_id, now_ms);

        // ── 10. Sequence Fill events and update state store ───────────────────
        for fill in &fills {
            self.sequencer.append_event(Event::Fill(fill.clone()));
            self.state
                .apply_fill(fill.taker_order_hash.0, fill.maker_order_hash.0, fill.size_lots);
        }

        // ── 11. Handle STP cancel ─────────────────────────────────────────────
        if order.status == OrderStatus::CancelledStp {
            self.sequencer.append_event(Event::OrderCancelled {
                order_id: order_id.clone(),
                order_hash,
                reason: CancelReason::SelfTradePrevention,
            });
            self.state.cancel_order(order_hash.0);
        }

        // ── 12. Insert residual into the book ─────────────────────────────────
        // Only if the order has remaining quantity and was not STP-cancelled.
        let final_status = order.status;
        if !order.is_closed() && order.remaining_size_lots() > 0 {
            let book = self.books.get_mut(&signed.market_id.0).unwrap();
            book.insert(order);
        }

        Ok(OrderResult {
            order_id,
            order_hash,
            seq_id,
            fills,
            final_status,
            is_duplicate: false,
        })
    }

    // -----------------------------------------------------------------------
    // Order cancellation
    // -----------------------------------------------------------------------

    /// Cancel a resting order by ID.
    ///
    /// Ownership check: the requesting `trader_id` must match the order owner.
    /// Idempotent: cancelling an already-closed order returns Ok with `was_already_closed = true`.
    pub fn process_cancel(
        &mut self,
        order_id: &OrderId,
        trader_id: &str,
    ) -> Result<CancelResult, CancelError> {
        // Find which market this order belongs to.
        let market_id = self
            .order_market_index
            .get(order_id)
            .ok_or_else(|| CancelError::NotFound(order_id.0.clone()))?
            .clone();

        let book = self.books.get(&market_id).unwrap();

        let order = book
            .get(order_id)
            .ok_or_else(|| CancelError::NotFound(order_id.0.clone()))?;

        // Ownership check — only the order's creator can cancel it.
        if order.trader_id != trader_id {
            return Err(CancelError::Unauthorized(trader_id.to_string()));
        }

        // Idempotent — already closed is not an error.
        if order.is_closed() {
            return Ok(CancelResult {
                order_id: order_id.clone(),
                seq_id: None,
                was_already_closed: true,
            });
        }

        let order_hash = order.order_hash;

        // Sequence the cancel event.
        let seq_id = self.sequencer.peek_next_seq_id();
        self.sequencer.append_event(Event::OrderCancelled {
            order_id: order_id.clone(),
            order_hash,
            reason: CancelReason::TraderRequest,
        });

        // Update state store and book.
        self.state.cancel_order(order_hash.0);
        let book = self.books.get_mut(&market_id).unwrap();
        book.cancel(order_id);

        Ok(CancelResult {
            order_id: order_id.clone(),
            seq_id: Some(seq_id),
            was_already_closed: false,
        })
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    /// Look up an order by ID across all markets.
    pub fn get_order(&self, order_id: &OrderId) -> Option<&Order> {
        self.find_order(order_id)
    }

    /// Best bid and ask prices for a market. Returns None if market not found.
    pub fn best_prices(&self, market_id: &str) -> Option<(Option<u64>, Option<u64>)> {
        let book = self.books.get(market_id)?;
        Some((book.best_bid(), book.best_ask()))
    }

    /// Level-2 depth snapshot for a market.
    pub fn l2_snapshot(
        &self,
        market_id: &str,
        depth: usize,
    ) -> Option<(Vec<orderbook::PriceLevel>, Vec<orderbook::PriceLevel>)> {
        let book = self.books.get(market_id)?;
        Some((book.l2_bids(depth), book.l2_asks(depth)))
    }

    /// List all registered markets.
    pub fn list_markets(&self) -> Vec<&MarketConfig> {
        let mut markets: Vec<&MarketConfig> = self.markets.values().collect();
        // Sort by market id string for deterministic API output.
        markets.sort_by_key(|m| &m.id.0);
        markets
    }

    /// Pull Fill events from the event log for a given market.
    ///
    /// Returns fills in ascending seq_id order (oldest first).
    /// `limit` caps the number of fills returned (0 = no cap).
    /// `from_seq` only returns fills with seq_id >= from_seq (0 = all).
    pub fn get_fills(
        &self,
        market_id: &str,
        from_seq: u64,
        limit: usize,
    ) -> Vec<&crate::domain::order::Fill> {
        self.sequencer
            .log
            .iter()
            .filter_map(|se| {
                if se.seq_id < from_seq && from_seq > 0 {
                    return None;
                }
                if let crate::events::Event::Fill(fill) = &se.event {
                    if fill.market_id.0 == market_id {
                        return Some(fill);
                    }
                }
                None
            })
            .take(if limit == 0 { usize::MAX } else { limit })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Crash recovery
    // -----------------------------------------------------------------------

    /// Restore engine state from a persisted event log (e.g. loaded from Postgres).
    ///
    /// Steps:
    ///   1. Replay events → rebuild OrderBooks + StateStore.
    ///   2. Rebuild the order→market index from OrderAccepted events.
    ///   3. Restore the Sequencer with the full log (rebuilds idempotency table).
    ///
    /// Markets must already be registered via `add_market` before calling this.
    /// Returns `ReplayError` if there are gaps or unknown order references.
    pub fn restore_from_events(
        &mut self,
        events: Vec<SequencedEvent>,
    ) -> Result<(), crate::replay::ReplayError> {
        use crate::events::Event;
        use crate::replay::replay;

        if events.is_empty() {
            return Ok(());
        }

        // Rebuild books and state store from the event log.
        let result = replay(&events)?;
        self.books = result.books;
        self.state = result.state;

        // Rebuild O(1) order → market lookup.
        self.order_market_index.clear();
        for se in &events {
            if let Event::OrderAccepted { order } = &se.event {
                self.order_market_index
                    .insert(order.order_id.clone(), order.market_id.0.clone());
            }
        }

        // Restore the sequencer — rebuilds idempotency table internally.
        let next_seq_id = events.last().map(|e| e.seq_id + 1).unwrap_or(1);
        self.sequencer = crate::sequencer::Sequencer::resume(next_seq_id, events);

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn find_order(&self, order_id: &OrderId) -> Option<&Order> {
        let market_id = self.order_market_index.get(order_id)?;
        self.books.get(market_id)?.get(order_id)
    }
}

// ---------------------------------------------------------------------------
// EngineError — unified error type wrapping all failure sources
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("order validation failed: {0}")]
    Order(#[from] OrderError),

    #[error("nonce rejected: {0}")]
    Nonce(#[from] NonceError),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::{MarketConfig, MarketId, MarketStatus};
    use crate::domain::order::{Side, TimeInForce, CURRENT_SCHEMA_VERSION};
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    // ── Test helpers ─────────────────────────────────────────────────────────

    /// An active BTC-USDC market with tick=1_000, lot=1.
    fn btc_usdc() -> MarketConfig {
        MarketConfig {
            id: MarketId("BTC-USDC".into()),
            base_asset: "BTC".into(),
            quote_asset: "USDC".into(),
            tick_size: 1_000,
            lot_size: 1,
            fee_bps_maker: 10,
            fee_bps_taker: 20,
            status: MarketStatus::Active,
        }
    }

    /// Sign and return a valid SignedOrder.
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
            client_order_id: client_order_id.into(),
            market_id: MarketId(market_id.into()),
            trader_id: hex::encode(pubkey),
            side,
            price_ticks: price,
            size_lots: size,
            time_in_force: TimeInForce::Gtc,
            nonce,
            expiry_ts_ms: 0,
            created_at_ms: 1_000,
            salt: nonce,
            trader_pubkey: pubkey,
            signature: [0u8; 64],
        };
        let hash = order.canonical_hash();
        order.signature = key.sign(&hash.0).to_bytes();
        order
    }

    /// Engine with a fixed clock for deterministic tests.
    fn test_engine() -> Engine {
        Engine::with_clock(Box::new(|| 1_000_000))
    }

    // ── Basic order placement ─────────────────────────────────────────────────

    #[test]
    fn place_resting_order() {
        let mut engine = test_engine();
        engine.add_market(btc_usdc());

        let key = SigningKey::generate(&mut OsRng);
        let order = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 1, "c-1");

        let result = engine.process_order(order).unwrap();
        assert_eq!(result.final_status, OrderStatus::Open);
        assert!(result.fills.is_empty());
        assert!(!result.is_duplicate);

        // Best bid should now reflect the resting order
        assert_eq!(engine.best_prices("BTC-USDC"), Some((Some(50_000_000), None)));
    }

    #[test]
    fn two_orders_match_fully() {
        let mut engine = test_engine();
        engine.add_market(btc_usdc());

        let buyer = SigningKey::generate(&mut OsRng);
        let seller = SigningKey::generate(&mut OsRng);

        // Seller posts ask at 51_000_000
        let ask = signed_order(&seller, "BTC-USDC", Side::Sell, 51_000_000, 10, 1, "ask-1");
        let ask_result = engine.process_order(ask).unwrap();
        assert_eq!(ask_result.final_status, OrderStatus::Open);

        // Buyer crosses at 51_000_000 — should match
        let bid = signed_order(&buyer, "BTC-USDC", Side::Buy, 51_000_000, 10, 1, "bid-1");
        let bid_result = engine.process_order(bid).unwrap();

        assert_eq!(bid_result.final_status, OrderStatus::Filled);
        assert_eq!(bid_result.fills.len(), 1);
        assert_eq!(bid_result.fills[0].price_ticks, 51_000_000);
        assert_eq!(bid_result.fills[0].size_lots, 10);

        // Book should be empty after full match
        assert_eq!(engine.best_prices("BTC-USDC"), Some((None, None)));
    }

    #[test]
    fn partial_fill_residual_rests_on_book() {
        let mut engine = test_engine();
        engine.add_market(btc_usdc());

        let maker = SigningKey::generate(&mut OsRng);
        let taker = SigningKey::generate(&mut OsRng);

        // Resting ask: 10 lots
        let ask = signed_order(&maker, "BTC-USDC", Side::Sell, 51_000_000, 10, 1, "ask-1");
        engine.process_order(ask).unwrap();

        // Incoming buy: 6 lots — partially fills the ask, 4 lots remain
        let bid = signed_order(&taker, "BTC-USDC", Side::Buy, 52_000_000, 6, 1, "bid-1");
        let result = engine.process_order(bid).unwrap();

        assert_eq!(result.final_status, OrderStatus::Filled);
        assert_eq!(result.fills[0].size_lots, 6);

        // 4 lots of the ask should still be resting
        let l2 = engine.l2_snapshot("BTC-USDC", 5).unwrap();
        assert_eq!(l2.1[0].total_qty, 4); // asks side
    }

    // ── Validation rejection tests ────────────────────────────────────────────

    #[test]
    fn reject_invalid_signature() {
        let mut engine = test_engine();
        engine.add_market(btc_usdc());

        let key = SigningKey::generate(&mut OsRng);
        let mut order = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 1, "c-1");
        order.price_ticks = 49_000_000; // tamper after signing

        assert!(matches!(
            engine.process_order(order),
            Err(EngineError::Order(OrderError::InvalidSignature))
        ));
    }

    #[test]
    fn reject_expired_order() {
        // Fixed clock at 2_000_000 ms
        let mut engine = Engine::with_clock(Box::new(|| 2_000_000));
        engine.add_market(btc_usdc());

        let key = SigningKey::generate(&mut OsRng);
        let pubkey = key.verifying_key().to_bytes();
        let mut order = SignedOrder {
            schema_version: CURRENT_SCHEMA_VERSION,
            client_order_id: "c-1".into(),
            market_id: MarketId("BTC-USDC".into()),
            trader_id: hex::encode(pubkey),
            side: Side::Buy,
            price_ticks: 50_000_000,
            size_lots: 1,
            time_in_force: TimeInForce::Gtc,
            nonce: 1,
            expiry_ts_ms: 1_000_000, // already past the clock
            created_at_ms: 0,
            salt: 0,
            trader_pubkey: pubkey,
            signature: [0u8; 64],
        };
        let hash = order.canonical_hash();
        order.signature = key.sign(&hash.0).to_bytes();

        assert!(matches!(
            engine.process_order(order),
            Err(EngineError::Order(OrderError::Expired { .. }))
        ));
    }

    #[test]
    fn reject_unknown_market() {
        let mut engine = test_engine();
        // Don't call add_market — BTC-USDC doesn't exist

        let key = SigningKey::generate(&mut OsRng);
        let order = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 1, "c-1");

        assert!(matches!(
            engine.process_order(order),
            Err(EngineError::Order(OrderError::MarketNotFound(_)))
        ));
    }

    #[test]
    fn reject_paused_market() {
        let mut engine = test_engine();
        let mut market = btc_usdc();
        market.status = MarketStatus::Paused;
        engine.add_market(market);

        let key = SigningKey::generate(&mut OsRng);
        let order = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 1, "c-1");

        assert!(matches!(
            engine.process_order(order),
            Err(EngineError::Order(OrderError::MarketNotActive(_)))
        ));
    }

    #[test]
    fn reject_invalid_tick() {
        let mut engine = test_engine();
        engine.add_market(btc_usdc()); // tick_size = 1_000

        let key = SigningKey::generate(&mut OsRng);
        // 50_000_500 % 1_000 != 0
        let order = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_500, 1, 1, "c-1");

        assert!(matches!(
            engine.process_order(order),
            Err(EngineError::Order(OrderError::InvalidTick { .. }))
        ));
    }

    #[test]
    fn reject_nonce_reuse() {
        let mut engine = test_engine();
        engine.add_market(btc_usdc());

        let key = SigningKey::generate(&mut OsRng);
        let o1 = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 5, "c-1");
        engine.process_order(o1).unwrap();

        // Same nonce again
        let o2 = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 5, "c-2");
        assert!(matches!(
            engine.process_order(o2),
            Err(EngineError::Nonce(_))
        ));
    }

    // ── Idempotency ───────────────────────────────────────────────────────────

    #[test]
    fn duplicate_client_order_id_returns_original() {
        let mut engine = test_engine();
        engine.add_market(btc_usdc());

        let key = SigningKey::generate(&mut OsRng);
        let o1 = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 1, "idempotent-key");
        let r1 = engine.process_order(o1).unwrap();

        // Same client_order_id — different nonce to pass nonce check,
        // but idempotency triggers before nonce check
        let o2 = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 2, "idempotent-key");
        let r2 = engine.process_order(o2).unwrap();

        assert!(r2.is_duplicate);
        assert_eq!(r2.order_id, r1.order_id);
        assert_eq!(r2.order_hash, r1.order_hash);
        assert_eq!(r2.seq_id, r1.seq_id);
        // No new events should have been appended
        assert_eq!(engine.sequencer.len(), r1.seq_id as usize);
    }

    // ── Cancel ────────────────────────────────────────────────────────────────

    #[test]
    fn cancel_own_order() {
        let mut engine = test_engine();
        engine.add_market(btc_usdc());

        let key = SigningKey::generate(&mut OsRng);
        let order = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 1, "c-1");
        let trader_id = hex::encode(key.verifying_key().to_bytes());

        let placed = engine.process_order(order).unwrap();
        assert_eq!(engine.best_prices("BTC-USDC"), Some((Some(50_000_000), None)));

        let cancel = engine.process_cancel(&placed.order_id, &trader_id).unwrap();
        assert!(!cancel.was_already_closed);

        // Book should be empty
        assert_eq!(engine.best_prices("BTC-USDC"), Some((None, None)));
    }

    #[test]
    fn cancel_someone_elses_order_fails() {
        let mut engine = test_engine();
        engine.add_market(btc_usdc());

        let owner = SigningKey::generate(&mut OsRng);
        let attacker = SigningKey::generate(&mut OsRng);

        let order = signed_order(&owner, "BTC-USDC", Side::Buy, 50_000_000, 1, 1, "c-1");
        let placed = engine.process_order(order).unwrap();

        let attacker_id = hex::encode(attacker.verifying_key().to_bytes());
        assert!(matches!(
            engine.process_cancel(&placed.order_id, &attacker_id),
            Err(CancelError::Unauthorized(_))
        ));
    }

    #[test]
    fn cancel_is_idempotent() {
        let mut engine = test_engine();
        engine.add_market(btc_usdc());

        let key = SigningKey::generate(&mut OsRng);
        let trader_id = hex::encode(key.verifying_key().to_bytes());
        let order = signed_order(&key, "BTC-USDC", Side::Buy, 50_000_000, 1, 1, "c-1");
        let placed = engine.process_order(order).unwrap();

        engine.process_cancel(&placed.order_id, &trader_id).unwrap();
        let r2 = engine.process_cancel(&placed.order_id, &trader_id).unwrap();
        assert!(r2.was_already_closed);
    }

    // ── STP ───────────────────────────────────────────────────────────────────

    #[test]
    fn stp_cancels_taker_and_emits_event() {
        let mut engine = test_engine();
        engine.add_market(btc_usdc());

        // Same key for both maker and taker = same trader_id = self-trade
        let key = SigningKey::generate(&mut OsRng);

        let maker = signed_order(&key, "BTC-USDC", Side::Sell, 51_000_000, 5, 1, "maker");
        engine.process_order(maker).unwrap();

        let taker = signed_order(&key, "BTC-USDC", Side::Buy, 52_000_000, 5, 2, "taker");
        let result = engine.process_order(taker).unwrap();

        assert_eq!(result.final_status, OrderStatus::CancelledStp);
        assert!(result.fills.is_empty());

        // Maker untouched
        assert_eq!(engine.best_prices("BTC-USDC"), Some((None, Some(51_000_000))));
    }

    // ── Event log integrity ───────────────────────────────────────────────────

    #[test]
    fn event_log_seq_ids_have_no_gaps() {
        let mut engine = test_engine();
        engine.add_market(btc_usdc());

        let buyer = SigningKey::generate(&mut OsRng);
        let seller = SigningKey::generate(&mut OsRng);

        engine.process_order(signed_order(&seller, "BTC-USDC", Side::Sell, 51_000_000, 5, 1, "s1")).unwrap();
        engine.process_order(signed_order(&buyer,  "BTC-USDC", Side::Buy,  51_000_000, 5, 1, "b1")).unwrap();

        for (i, ev) in engine.sequencer.log.iter().enumerate() {
            assert_eq!(ev.seq_id, (i + 1) as u64, "gap at index {i}");
        }
    }
}
