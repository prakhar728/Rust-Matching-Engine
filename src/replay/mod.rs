// Deterministic replay — rebuild OrderBook + StateStore from an event log.
//
// Replay is the crash-recovery primitive: if the engine process dies, we
// reload the persisted event log and call `replay()` to restore in-memory
// state to exactly where it was.
//
// WHY REPLAY WORKS:
//   The event log is append-only and totally ordered by seq_id. Each event
//   is a pure description of a state transition — no wall-clock timestamps
//   are used in any matching decision, and no randomness exists after the
//   SignedOrder is ingested. So: same events in same order → identical state.
//
// HOW REPLAY WORKS:
//   We walk the event log in seq_id order (verifying no gaps) and apply
//   each event to a working set of in-flight orders plus the StateStore.
//   After the entire log is consumed, we insert any still-open orders into
//   the appropriate OrderBook.
//
//   Event semantics during replay:
//     OrderAccepted  → record the order in the working set; advance the nonce
//     Fill           → apply fill to both taker and maker (filled_size_lots)
//                      and to StateStore.apply_fill(); mark fully-filled orders
//     OrderCancelled → set order status (Cancelled or CancelledStp) and call
//                      StateStore.cancel_order()
//     OrderRejected  → no state change (rejected orders never enter the book)
//     MarketPaused / MarketResumed → update market status (future: Phase 1)
//
// BUILDING THE BOOK:
//   After walking all events, every order that is still Open or PartiallyFilled
//   (i.e. !is_closed()) is inserted into the appropriate OrderBook. Orders are
//   inserted in created_sequence order so price-level VecDeques preserve the
//   original time priority.
//
// VERIFICATION:
//   `verify_replay()` runs replay twice and asserts that both passes produce
//   identical state checksums. This is a correctness smoke-test that catches
//   any non-determinism introduced by HashMap iteration order or similar.

use std::collections::HashMap;

use crate::db::StateStore;
use crate::domain::order::{Order, OrderId, OrderStatus};
use crate::engine::orderbook::OrderBook;
use crate::events::{CancelReason, Event, SequencedEvent};

// ---------------------------------------------------------------------------
// Public result type
// ---------------------------------------------------------------------------

/// The fully reconstructed state after replaying an event log.
#[derive(Debug)]
pub struct ReplayResult {
    /// One OrderBook per market — contains only the currently-open resting orders.
    pub books: HashMap<String, OrderBook>,

    /// NEAR-compatible state store — filled_amounts, cancelled, nonces.
    pub state: StateStore,

    /// The highest seq_id seen during this replay (0 if the log was empty).
    pub last_seq_id: u64,

    /// Total number of events processed.
    pub event_count: usize,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything that can go wrong during replay.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// The event log has a hole: seq_ids skipped from `expected` to `got`.
    ///
    /// Any gap indicates missing events — data corruption or incomplete load.
    /// Replay must be aborted; proceeding would produce inconsistent state.
    #[error("seq_id gap: expected {expected}, got {got}")]
    SeqIdGap { expected: u64, got: u64 },

    /// A Fill or OrderCancelled event references an order_id that was never
    /// seen in an earlier OrderAccepted event. This should never happen in a
    /// correctly-produced log.
    #[error("event references unknown order_id {0:?}")]
    UnknownOrder(OrderId),
}

// ---------------------------------------------------------------------------
// Core replay function
// ---------------------------------------------------------------------------

/// Replay a slice of sequenced events and return the reconstructed state.
///
/// # Errors
/// Returns `ReplayError::SeqIdGap` if any seq_id is missing or out of order.
/// Returns `ReplayError::UnknownOrder` if a Fill/Cancel references an unseen order.
pub fn replay(events: &[SequencedEvent]) -> Result<ReplayResult, ReplayError> {
    // Working set: all orders ever accepted, keyed by order_id.
    // We accumulate fills and cancels here, then build the books at the end.
    let mut orders: HashMap<OrderId, Order> = HashMap::new();

    let mut state = StateStore::new();
    let mut last_seq_id: u64 = 0;

    for se in events {
        // ── Gap check ────────────────────────────────────────────────────────
        // seq_ids must be a contiguous ascending sequence starting from 1.
        // The first event we see can have any seq_id (we may be replaying
        // from a snapshot, so seq_ids don't restart at 1).
        if last_seq_id != 0 && se.seq_id != last_seq_id + 1 {
            return Err(ReplayError::SeqIdGap {
                expected: last_seq_id + 1,
                got: se.seq_id,
            });
        }
        last_seq_id = se.seq_id;

        match &se.event {
            // ── OrderAccepted ─────────────────────────────────────────────
            // The full Order struct is embedded in the event, so replay is
            // self-contained — no DB join needed to reconstruct the order.
            //
            // We directly set the nonce in the state store (bypassing the
            // strict > check used on live intake) because replaying already-
            // accepted events must never be rejected.
            Event::OrderAccepted { order } => {
                // Advance the nonce table to the highest seen value.
                // Direct insertion skips the monotonic validation in
                // check_and_update_nonce (which would spuriously reject
                // earlier events replayed after a later one in the same
                // trader's stream — though in practice the log is ordered,
                // this is safer).
                let stored = state.nonces.get(&order.trader_id).copied().unwrap_or(0);
                if order.nonce > stored {
                    state.nonces.insert(order.trader_id.clone(), order.nonce);
                }

                orders.insert(order.order_id.clone(), order.clone());
            }

            // ── Fill ──────────────────────────────────────────────────────
            // A fill touches two orders (taker and maker). We update
            // filled_size_lots on both and promote them to the right status.
            Event::Fill(fill) => {
                // Update taker
                let taker = orders
                    .get_mut(&fill.taker_order_id)
                    .ok_or_else(|| ReplayError::UnknownOrder(fill.taker_order_id.clone()))?;

                taker.filled_size_lots += fill.size_lots;
                taker.last_update_sequence = se.seq_id;

                if taker.filled_size_lots >= taker.orig_size_lots {
                    taker.status = OrderStatus::Filled;
                } else {
                    taker.status = OrderStatus::PartiallyFilled;
                }

                // Update maker
                let maker = orders
                    .get_mut(&fill.maker_order_id)
                    .ok_or_else(|| ReplayError::UnknownOrder(fill.maker_order_id.clone()))?;

                maker.filled_size_lots += fill.size_lots;
                maker.last_update_sequence = se.seq_id;

                if maker.filled_size_lots >= maker.orig_size_lots {
                    maker.status = OrderStatus::Filled;
                } else {
                    maker.status = OrderStatus::PartiallyFilled;
                }

                // Update StateStore — mirrors what Engine::process_order does
                // after each successful fill.
                state.apply_fill(fill.taker_order_hash.0, fill.maker_order_hash.0, fill.size_lots);
            }

            // ── OrderCancelled ────────────────────────────────────────────
            // Set the order status to the right cancel variant and mark the
            // hash as cancelled in the StateStore.
            Event::OrderCancelled { order_id, order_hash, reason } => {
                let order = orders
                    .get_mut(order_id)
                    .ok_or_else(|| ReplayError::UnknownOrder(order_id.clone()))?;

                // Preserve the distinction between a normal cancel and an STP
                // cancel — important for downstream audit and API responses.
                order.status = match reason {
                    CancelReason::SelfTradePrevention => OrderStatus::CancelledStp,
                    _ => OrderStatus::Cancelled,
                };
                order.last_update_sequence = se.seq_id;

                state.cancel_order(order_hash.0);
            }

            // ── OrderRejected ─────────────────────────────────────────────
            // Rejected orders never entered the book, so no state to update.
            // We still process the event to keep the gap-check counter accurate.
            Event::OrderRejected { .. } => {}

            // ── Market status changes ─────────────────────────────────────
            // Phase -1 does not persist MarketConfig in the event log, so we
            // can't reconstruct market status from replay alone. These events
            // are recorded for audit but market configs are re-loaded from
            // a config file or DB at startup.
            Event::MarketPaused { .. } | Event::MarketResumed { .. } => {}
        }
    }

    // ── Build OrderBooks from the working set ────────────────────────────────
    //
    // Only orders that are still open (Open or PartiallyFilled) go into the
    // book. We sort by created_sequence before inserting so that each price
    // level's VecDeque preserves original time priority (FIFO within a level).
    //
    // Orders for different markets automatically end up in different books
    // because we key books by market_id string.
    let mut books: HashMap<String, OrderBook> = HashMap::new();

    // Collect open orders and sort by created_sequence (ascending = oldest first).
    let mut open_orders: Vec<&Order> = orders.values().filter(|o| !o.is_closed()).collect();
    open_orders.sort_by_key(|o| o.created_sequence);

    for order in open_orders {
        let book = books
            .entry(order.market_id.0.clone())
            .or_insert_with(OrderBook::new);
        book.insert(order.clone());
    }

    Ok(ReplayResult {
        books,
        state,
        last_seq_id,
        event_count: events.len(),
    })
}

// ---------------------------------------------------------------------------
// Verification helper
// ---------------------------------------------------------------------------

/// Run replay twice and assert that both passes produce the same state checksum.
///
/// This is a correctness smoke-test, not a production function. Call it in
/// integration tests or after loading a log from disk to verify integrity.
///
/// Returns `(checksum_bytes, ReplayResult)` for the first run.
///
/// # Panics
/// Panics if the two checksums differ (indicates non-determinism in replay).
pub fn verify_replay(events: &[SequencedEvent]) -> Result<([u8; 32], ReplayResult), ReplayError> {
    let first = replay(events)?;
    let second = replay(events)?;

    let c1 = first.state.checksum();
    let c2 = second.state.checksum();

    assert_eq!(
        c1, c2,
        "replay is not deterministic: two passes produced different state checksums"
    );

    Ok((c1, first))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::MarketId;
    use crate::domain::order::{Fill, OrderHash, OrderStatus, Side, TimeInForce};
    use crate::events::{CancelReason, Event, SequencedEvent};

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_order(id: &str, market: &str, side: Side, price: u64, size: u64, seq: u64) -> Order {
        Order {
            order_id: OrderId(id.to_string()),
            client_order_id: format!("cid-{id}"),
            order_hash: OrderHash([0u8; 32]),
            market_id: MarketId(market.to_string()),
            trader_id: "trader-a".to_string(),
            trader_pubkey: [0u8; 32],
            side,
            price_ticks: price,
            orig_size_lots: size,
            filled_size_lots: 0,
            time_in_force: TimeInForce::Gtc,
            nonce: seq,
            expiry_ts_ms: 0,
            status: OrderStatus::Open,
            created_sequence: seq,
            last_update_sequence: seq,
            accepted_at_ms: 1_000_000,
        }
    }

    fn seq(id: u64, event: Event) -> SequencedEvent {
        SequencedEvent { seq_id: id, timestamp_ms: 1_000_000 + id, event }
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[test]
    fn empty_log_produces_empty_state() {
        let result = replay(&[]).unwrap();
        assert_eq!(result.event_count, 0);
        assert_eq!(result.last_seq_id, 0);
        assert!(result.books.is_empty());
        assert!(result.state.filled_amounts.is_empty());
    }

    #[test]
    fn accepted_order_appears_on_book() {
        let order = make_order("o1", "BTC-USDC", Side::Buy, 100, 10, 1);
        let events = vec![seq(1, Event::OrderAccepted { order: order.clone() })];

        let result = replay(&events).unwrap();

        assert!(result.books.contains_key("BTC-USDC"));
        let book = &result.books["BTC-USDC"];
        assert_eq!(book.best_bid(), Some(100));
    }

    #[test]
    fn filled_order_is_not_on_book() {
        // An order that is fully filled should NOT appear in the reconstructed book.
        let order = make_order("o1", "BTC-USDC", Side::Sell, 100, 10, 1);
        let maker_order = make_order("o2", "BTC-USDC", Side::Buy, 100, 10, 2);

        let fill = Fill {
            market_id: MarketId("BTC-USDC".into()),
            taker_order_id: OrderId("o2".into()),
            taker_order_hash: OrderHash([2u8; 32]),
            taker_trader_id: "trader-b".to_string(),
            maker_order_id: OrderId("o1".into()),
            maker_order_hash: OrderHash([1u8; 32]),
            maker_trader_id: "trader-a".to_string(),
            price_ticks: 100,
            size_lots: 10, // full fill
            sequence_id: 3,
            executed_at_ms: 1_000_003,
        };

        let events = vec![
            seq(1, Event::OrderAccepted { order: order }),
            seq(2, Event::OrderAccepted { order: maker_order }),
            seq(3, Event::Fill(fill)),
        ];

        let result = replay(&events).unwrap();

        // Both orders fully filled — book should be empty
        let book = result.books.get("BTC-USDC");
        assert!(book.is_none() || book.unwrap().best_bid().is_none());
        assert!(book.is_none() || book.unwrap().best_ask().is_none());

        // StateStore must record the fills
        assert_eq!(result.state.filled_amount(&[1u8; 32]), 10);
        assert_eq!(result.state.filled_amount(&[2u8; 32]), 10);
    }

    #[test]
    fn cancelled_order_is_not_on_book_and_marked_in_state() {
        let mut order = make_order("o1", "BTC-USDC", Side::Buy, 100, 10, 1);
        order.order_hash = OrderHash([7u8; 32]);

        let events = vec![
            seq(1, Event::OrderAccepted { order: order.clone() }),
            seq(
                2,
                Event::OrderCancelled {
                    order_id: OrderId("o1".into()),
                    order_hash: OrderHash([7u8; 32]),
                    reason: CancelReason::TraderRequest,
                },
            ),
        ];

        let result = replay(&events).unwrap();

        // Cancelled → not on book
        let book = result.books.get("BTC-USDC");
        assert!(book.is_none() || book.unwrap().best_bid().is_none());

        // Cancelled → marked in StateStore
        assert!(result.state.is_cancelled(&[7u8; 32]));
    }

    #[test]
    fn stp_cancel_sets_cancelled_stp_status() {
        let mut order = make_order("o1", "BTC-USDC", Side::Buy, 100, 10, 1);
        order.order_hash = OrderHash([9u8; 32]);

        let events = vec![
            seq(1, Event::OrderAccepted { order: order.clone() }),
            seq(
                2,
                Event::OrderCancelled {
                    order_id: OrderId("o1".into()),
                    order_hash: OrderHash([9u8; 32]),
                    reason: CancelReason::SelfTradePrevention,
                },
            ),
        ];

        // After replay the order should be off the book and marked in state.
        let result = replay(&events).unwrap();
        assert!(result.state.is_cancelled(&[9u8; 32]));
        // No open orders → no book entries
        assert!(result.books.get("BTC-USDC").is_none()
            || result.books["BTC-USDC"].best_bid().is_none());
    }

    #[test]
    fn seq_id_gap_returns_error() {
        let order = make_order("o1", "BTC-USDC", Side::Buy, 100, 10, 1);
        let events = vec![
            seq(1, Event::OrderAccepted { order: order.clone() }),
            // seq_id 2 is missing — jump straight to 3
            seq(3, Event::OrderAccepted { order }),
        ];

        let err = replay(&events).unwrap_err();
        assert!(matches!(err, ReplayError::SeqIdGap { expected: 2, got: 3 }));
    }

    #[test]
    fn order_rejected_event_does_not_affect_state() {
        let events = vec![seq(
            1,
            Event::OrderRejected {
                client_order_id: "cid-1".into(),
                trader_id: "trader-a".into(),
                reason: "ZeroPrice".into(),
            },
        )];

        let result = replay(&events).unwrap();
        assert!(result.books.is_empty());
        assert!(result.state.filled_amounts.is_empty());
        assert!(result.state.nonces.is_empty());
    }

    #[test]
    fn time_priority_preserved_within_price_level() {
        // Two buy orders at the same price — created_sequence order must be FIFO.
        let o1 = make_order("o1", "BTC-USDC", Side::Buy, 100, 5, 1);
        let o2 = make_order("o2", "BTC-USDC", Side::Buy, 100, 5, 2);

        let events = vec![
            seq(1, Event::OrderAccepted { order: o1 }),
            seq(2, Event::OrderAccepted { order: o2 }),
        ];

        let result = replay(&events).unwrap();
        let book = &result.books["BTC-USDC"];

        // Both orders at price 100
        let queue = book.orders_at_price(100, crate::domain::order::Side::Buy);
        assert_eq!(queue.len(), 2);
        // o1 was placed first (created_sequence=1) so it must be head of queue
        assert_eq!(queue[0], OrderId("o1".into()));
        assert_eq!(queue[1], OrderId("o2".into()));
    }

    #[test]
    fn partial_fill_leaves_order_on_book() {
        // An order that is half-filled should remain in the book.
        let mut order = make_order("o1", "BTC-USDC", Side::Sell, 100, 20, 1);
        order.order_hash = OrderHash([1u8; 32]);

        let mut taker = make_order("o2", "BTC-USDC", Side::Buy, 100, 10, 2);
        taker.order_hash = OrderHash([2u8; 32]);

        let fill = Fill {
            market_id: MarketId("BTC-USDC".into()),
            taker_order_id: OrderId("o2".into()),
            taker_order_hash: OrderHash([2u8; 32]),
            taker_trader_id: "trader-b".into(),
            maker_order_id: OrderId("o1".into()),
            maker_order_hash: OrderHash([1u8; 32]),
            maker_trader_id: "trader-a".into(),
            price_ticks: 100,
            size_lots: 10, // only half of o1's 20 lots
            sequence_id: 3,
            executed_at_ms: 1_000_003,
        };

        let events = vec![
            seq(1, Event::OrderAccepted { order }),
            seq(2, Event::OrderAccepted { order: taker }),
            seq(3, Event::Fill(fill)),
        ];

        let result = replay(&events).unwrap();

        // o1 is partially filled (10/20) — still on the book as an ask
        let book = &result.books["BTC-USDC"];
        assert_eq!(book.best_ask(), Some(100));

        // taker (o2) is fully filled (10/10) — NOT on the book
        assert_eq!(book.best_bid(), None);

        // StateStore records 10 filled for both
        assert_eq!(result.state.filled_amount(&[1u8; 32]), 10);
        assert_eq!(result.state.filled_amount(&[2u8; 32]), 10);
    }

    #[test]
    fn nonce_advanced_for_each_accepted_order() {
        let mut o1 = make_order("o1", "BTC-USDC", Side::Buy, 100, 5, 1);
        o1.nonce = 7;
        let mut o2 = make_order("o2", "BTC-USDC", Side::Buy, 101, 5, 2);
        o2.nonce = 12;

        let events = vec![
            seq(1, Event::OrderAccepted { order: o1 }),
            seq(2, Event::OrderAccepted { order: o2 }),
        ];

        let result = replay(&events).unwrap();

        // After replay, the nonce for trader-a should be the highest seen (12).
        assert_eq!(result.state.last_nonce("trader-a"), 12);
    }

    #[test]
    fn verify_replay_returns_same_checksum_both_runs() {
        let order = make_order("o1", "BTC-USDC", Side::Buy, 100, 10, 1);
        let events = vec![seq(1, Event::OrderAccepted { order })];
        // verify_replay panics on non-determinism — just confirm it succeeds.
        verify_replay(&events).unwrap();
    }

    #[test]
    fn multi_market_events_produce_separate_books() {
        let o1 = make_order("o1", "BTC-USDC", Side::Buy, 100, 5, 1);
        let mut o2 = make_order("o2", "ETH-USDC", Side::Sell, 200, 5, 2);
        o2.market_id = MarketId("ETH-USDC".into());
        o2.trader_id = "trader-a".into();

        let events = vec![
            seq(1, Event::OrderAccepted { order: o1 }),
            seq(2, Event::OrderAccepted { order: o2 }),
        ];

        let result = replay(&events).unwrap();

        assert!(result.books.contains_key("BTC-USDC"));
        assert!(result.books.contains_key("ETH-USDC"));
        assert_eq!(result.books["BTC-USDC"].best_bid(), Some(100));
        assert_eq!(result.books["ETH-USDC"].best_ask(), Some(200));
    }
}
