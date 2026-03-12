// Price-time priority matching engine.
//
// This module owns one job: given an incoming order and a book, produce fills.
// It does not write to the event log, does not touch the sequencer, and does
// not insert the residual into the book. Those are the engine's responsibilities.
//
// Matching rules (from phase_-1.md §5):
//   1. Buy  order → match against asks, lowest ask price first.
//      Crossable if ask_price <= buy_price.
//   2. Sell order → match against bids, highest bid price first.
//      Crossable if bid_price >= sell_price.
//   3. Within a price level, FIFO: earliest accepted order matches first.
//   4. Fill price is always the MAKER's price (resting order).
//   5. Self-trade prevention (STP) mode: Cancel Taker.
//      If taker.trader_id == maker.trader_id → cancel the remaining taker
//      quantity. The resting maker order is left untouched on the book.
//   6. Partial fills: if taker.remaining < maker.remaining, the maker is
//      partially filled and stays on the book. Taker is done.
//
// Borrow safety:
//   `ask_prices_asc()` and `orders_at_price()` return owned Vecs, releasing
//   the immutable borrow before we take mutable borrows to apply fills.
//   This lets us safely alternate reads and writes on the same OrderBook.

use crate::domain::market::MarketId;
use crate::domain::order::{Fill, Order, OrderHash, OrderId, OrderStatus, Side};
use crate::engine::orderbook::OrderBook;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run price-time priority matching for `taker` against `book`.
///
/// Mutates:
///   - `taker.filled_size_lots` and `taker.status` as fills accumulate.
///   - Maker orders inside `book` (filled_size_lots, status, last_update_sequence).
///   - Fully filled makers are removed from the book's price-level queues.
///
/// Does NOT:
///   - Insert the taker into the book (engine does that if residual remains).
///   - Append events to the log (engine does that).
///   - Touch the state store (engine does that).
///
/// `seq_id`  — the sequence number of the OrderAccepted event for this taker.
///             Stamped on all fills produced by this match run.
/// `now_ms`  — current epoch milliseconds, stamped on fills for observability.
pub fn match_order(
    book: &mut OrderBook,
    taker: &mut Order,
    seq_id: u64,
    now_ms: u64,
) -> Vec<Fill> {
    let mut fills = Vec::new();

    match taker.side {
        Side::Buy  => match_buy(book, taker, seq_id, now_ms, &mut fills),
        Side::Sell => match_sell(book, taker, seq_id, now_ms, &mut fills),
    }

    fills
}

// ---------------------------------------------------------------------------
// Side-specific entry points
// ---------------------------------------------------------------------------

/// Match a buy order against the ask side (lowest ask first).
fn match_buy(
    book: &mut OrderBook,
    taker: &mut Order,
    seq_id: u64,
    now_ms: u64,
    fills: &mut Vec<Fill>,
) {
    // Snapshot crossable prices: asks at or below our limit price, ascending.
    // Snapshot is owned (Vec<u64>) so no borrow is held during the loop body.
    let prices: Vec<u64> = book
        .ask_prices_asc()
        .into_iter()
        .take_while(|&p| p <= taker.price_ticks) // only prices we're willing to pay
        .collect();

    for price in prices {
        if taker.remaining_size_lots() == 0 { break; }
        if taker.is_closed() { break; } // STP may have cancelled us

        match_at_price_level(book, taker, price, Side::Sell, seq_id, now_ms, fills);
    }
}

/// Match a sell order against the bid side (highest bid first).
fn match_sell(
    book: &mut OrderBook,
    taker: &mut Order,
    seq_id: u64,
    now_ms: u64,
    fills: &mut Vec<Fill>,
) {
    // Snapshot crossable prices: bids at or above our limit price, descending.
    let prices: Vec<u64> = book
        .bid_prices_desc()
        .into_iter()
        .take_while(|&p| p >= taker.price_ticks) // only prices we're willing to accept
        .collect();

    for price in prices {
        if taker.remaining_size_lots() == 0 { break; }
        if taker.is_closed() { break; }

        match_at_price_level(book, taker, price, Side::Buy, seq_id, now_ms, fills);
    }
}

// ---------------------------------------------------------------------------
// Core matching loop (one price level)
// ---------------------------------------------------------------------------

/// Process all fills possible at a single price level.
///
/// Iterates the maker queue (FIFO). For each maker:
///   1. STP check — cancel taker if same trader.
///   2. Compute fill quantity.
///   3. Build and record the Fill.
///   4. Update both taker and maker order state.
///   5. Remove fully filled maker from the queue.
fn match_at_price_level(
    book: &mut OrderBook,
    taker: &mut Order,
    price: u64,
    maker_side: Side,
    seq_id: u64,
    now_ms: u64,
    fills: &mut Vec<Fill>,
) {
    // Snapshot the queue at this price level (owned Vec, no borrow held).
    // We must re-validate each maker below because the queue may have changed
    // since the snapshot was taken (e.g. if we're walking multiple levels).
    let maker_ids = book.orders_at_price(price, maker_side);

    for maker_id in maker_ids {
        if taker.remaining_size_lots() == 0 { break; }

        // ── Step 1: Read the maker (immutable borrow, short scope) ──────────
        // Extract everything we need before releasing the borrow.
        let maker_snapshot = {
            let maker = match book.get(&maker_id) {
                Some(m) => m,
                None => continue, // order removed between snapshot and now (shouldn't happen in V1)
            };

            if maker.is_closed() {
                continue; // stale entry in our snapshot — skip it
            }

            MakerSnapshot {
                order_id: maker.order_id.clone(),
                order_hash: maker.order_hash,
                trader_id: maker.trader_id.clone(),
                market_id: maker.market_id.clone(),
                remaining: maker.remaining_size_lots(),
            }
        }; // immutable borrow released here

        // ── Step 2: Self-trade prevention (STP) ─────────────────────────────
        // Cancel Taker mode: drop the entire remaining taker quantity.
        // The resting maker is left completely untouched on the book.
        if maker_snapshot.trader_id == taker.trader_id {
            taker.status = OrderStatus::CancelledStp;
            taker.last_update_sequence = seq_id;
            break; // stop matching — taker is gone
        }

        // ── Step 3: Compute fill quantity ────────────────────────────────────
        // Fill as much as both sides have available.
        let fill_qty = taker.remaining_size_lots().min(maker_snapshot.remaining);

        // ── Step 4: Build the fill record ────────────────────────────────────
        // Fill price is always the MAKER's price (price-time priority rule).
        let fill = Fill {
            market_id: maker_snapshot.market_id,
            taker_order_id: taker.order_id.clone(),
            taker_order_hash: taker.order_hash,
            taker_trader_id: taker.trader_id.clone(),
            maker_order_id: maker_snapshot.order_id,
            maker_order_hash: maker_snapshot.order_hash,
            maker_trader_id: maker_snapshot.trader_id,
            price_ticks: price, // maker's price
            size_lots: fill_qty,
            sequence_id: seq_id,
            executed_at_ms: now_ms,
        };
        fills.push(fill);

        // ── Step 5: Update taker ─────────────────────────────────────────────
        taker.filled_size_lots += fill_qty;
        taker.last_update_sequence = seq_id;
        taker.status = if taker.remaining_size_lots() == 0 {
            OrderStatus::Filled
        } else {
            OrderStatus::PartiallyFilled
        };

        // ── Step 6: Update maker (mutable borrow, separate scope) ────────────
        let maker_fully_filled = {
            let maker = book.get_mut(&maker_id).unwrap();
            maker.filled_size_lots += fill_qty;
            maker.last_update_sequence = seq_id;
            maker.status = if maker.remaining_size_lots() == 0 {
                OrderStatus::Filled
            } else {
                OrderStatus::PartiallyFilled
            };
            maker.is_closed()
        }; // mutable borrow released

        // ── Step 7: Remove fully filled maker from price-level queue ─────────
        // Partially filled makers stay in the queue (they still have remaining qty).
        if maker_fully_filled {
            book.remove_from_queue(&maker_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helper
// ---------------------------------------------------------------------------

/// A snapshot of the maker fields we need before releasing the immutable borrow.
/// Avoids holding a reference across a mutable borrow boundary.
struct MakerSnapshot {
    order_id: OrderId,
    order_hash: OrderHash,
    trader_id: String,
    market_id: MarketId,
    remaining: u64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::MarketId;
    use crate::domain::order::{Order, OrderHash, OrderId, OrderStatus, Side, TimeInForce};
    use crate::engine::orderbook::OrderBook;

    static SEQ: u64 = 99;
    static NOW: u64 = 1_000_000;

    /// Build an Order for testing. `trader` lets us control STP scenarios.
    fn make_order(id: &str, trader: &str, side: Side, price: u64, size: u64) -> Order {
        Order {
            order_id: OrderId(id.into()),
            client_order_id: format!("c-{id}"),
            order_hash: OrderHash([0u8; 32]),
            market_id: MarketId("BTC-USDC".into()),
            trader_id: trader.into(),
            trader_pubkey: [0u8; 32],
            side,
            price_ticks: price,
            orig_size_lots: size,
            filled_size_lots: 0,
            time_in_force: TimeInForce::Gtc,
            nonce: 1,
            expiry_ts_ms: 0,
            status: OrderStatus::Open,
            created_sequence: 1,
            last_update_sequence: 1,
            accepted_at_ms: 0,
        }
    }

    // ── Basic match tests ────────────────────────────────────────────────────

    #[test]
    fn buy_fully_matches_single_ask() {
        let mut book = OrderBook::new();
        // Resting ask: sell 10 lots at 51_000
        book.insert(make_order("maker", "trader-A", Side::Sell, 51_000, 10));

        // Incoming buy: buy 10 lots willing to pay up to 52_000
        let mut taker = make_order("taker", "trader-B", Side::Buy, 52_000, 10);

        let fills = match_order(&mut book, &mut taker, SEQ, NOW);

        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].price_ticks, 51_000); // fill at maker's price
        assert_eq!(fills[0].size_lots, 10);
        assert_eq!(taker.status, OrderStatus::Filled);
        assert_eq!(taker.remaining_size_lots(), 0);

        // Maker should be fully filled and gone from the book
        assert!(book.best_ask().is_none());
    }

    #[test]
    fn sell_fully_matches_single_bid() {
        let mut book = OrderBook::new();
        book.insert(make_order("maker", "trader-A", Side::Buy, 50_000, 10));

        let mut taker = make_order("taker", "trader-B", Side::Sell, 49_000, 10);

        let fills = match_order(&mut book, &mut taker, SEQ, NOW);

        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].price_ticks, 50_000); // fill at maker's price
        assert_eq!(fills[0].size_lots, 10);
        assert_eq!(taker.status, OrderStatus::Filled);
        assert!(book.best_bid().is_none());
    }

    #[test]
    fn no_match_when_prices_dont_cross() {
        let mut book = OrderBook::new();
        // Ask is higher than buy limit — no match
        book.insert(make_order("maker", "trader-A", Side::Sell, 55_000, 10));

        let mut taker = make_order("taker", "trader-B", Side::Buy, 50_000, 10);

        let fills = match_order(&mut book, &mut taker, SEQ, NOW);

        assert!(fills.is_empty());
        assert_eq!(taker.status, OrderStatus::Open);
        assert_eq!(taker.remaining_size_lots(), 10);
        // Maker should still be on the book
        assert_eq!(book.best_ask(), Some(55_000));
    }

    // ── Partial fill tests ────────────────────────────────────────────────────

    #[test]
    fn taker_larger_than_maker_leaves_taker_partial() {
        let mut book = OrderBook::new();
        book.insert(make_order("maker", "trader-A", Side::Sell, 51_000, 5));

        // Taker wants 10, maker only has 5 → taker partially filled
        let mut taker = make_order("taker", "trader-B", Side::Buy, 52_000, 10);

        let fills = match_order(&mut book, &mut taker, SEQ, NOW);

        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].size_lots, 5);
        assert_eq!(taker.status, OrderStatus::PartiallyFilled);
        assert_eq!(taker.remaining_size_lots(), 5); // 5 left for the book
    }

    #[test]
    fn maker_larger_than_taker_leaves_maker_partial() {
        let mut book = OrderBook::new();
        book.insert(make_order("maker", "trader-A", Side::Sell, 51_000, 20));

        // Taker only wants 5
        let mut taker = make_order("taker", "trader-B", Side::Buy, 52_000, 5);

        let fills = match_order(&mut book, &mut taker, SEQ, NOW);

        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].size_lots, 5);
        assert_eq!(taker.status, OrderStatus::Filled);

        // Maker partially filled, still on the book with 15 remaining
        let maker = book.get(&OrderId("maker".into())).unwrap();
        assert_eq!(maker.status, OrderStatus::PartiallyFilled);
        assert_eq!(maker.remaining_size_lots(), 15);
        assert_eq!(book.best_ask(), Some(51_000)); // still there
    }

    // ── Multi-level and multi-order tests ─────────────────────────────────────

    #[test]
    fn buy_walks_multiple_price_levels() {
        let mut book = OrderBook::new();
        // Three ask levels
        book.insert(make_order("a1", "trader-A", Side::Sell, 51_000, 5));
        book.insert(make_order("a2", "trader-A", Side::Sell, 52_000, 5));
        book.insert(make_order("a3", "trader-A", Side::Sell, 53_000, 5));

        // Taker wants 15 and is willing to pay up to 53_000
        let mut taker = make_order("taker", "trader-B", Side::Buy, 53_000, 15);

        let fills = match_order(&mut book, &mut taker, SEQ, NOW);

        assert_eq!(fills.len(), 3); // one fill per price level
        assert_eq!(fills[0].price_ticks, 51_000); // cheapest first
        assert_eq!(fills[1].price_ticks, 52_000);
        assert_eq!(fills[2].price_ticks, 53_000);
        assert_eq!(taker.status, OrderStatus::Filled);
        assert!(book.best_ask().is_none()); // all makers consumed
    }

    #[test]
    fn buy_stops_at_limit_price() {
        let mut book = OrderBook::new();
        book.insert(make_order("a1", "trader-A", Side::Sell, 51_000, 5));
        book.insert(make_order("a2", "trader-A", Side::Sell, 52_000, 5)); // above limit

        // Taker willing to pay up to 51_000 only
        let mut taker = make_order("taker", "trader-B", Side::Buy, 51_000, 10);

        let fills = match_order(&mut book, &mut taker, SEQ, NOW);

        // Only matched the 51_000 ask
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].price_ticks, 51_000);
        assert_eq!(fills[0].size_lots, 5);
        assert_eq!(taker.status, OrderStatus::PartiallyFilled);
        assert_eq!(taker.remaining_size_lots(), 5);
        // The 52_000 ask is still on the book
        assert_eq!(book.best_ask(), Some(52_000));
    }

    #[test]
    fn fifo_priority_at_same_price() {
        let mut book = OrderBook::new();
        // Two orders at the same price — first inserted should match first
        book.insert(make_order("first",  "trader-A", Side::Sell, 51_000, 5));
        book.insert(make_order("second", "trader-B", Side::Sell, 51_000, 5));

        // Taker wants 5 — should only match "first"
        let mut taker = make_order("taker", "trader-C", Side::Buy, 51_000, 5);

        let fills = match_order(&mut book, &mut taker, SEQ, NOW);

        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].maker_order_id, OrderId("first".into()));
        assert_eq!(taker.status, OrderStatus::Filled);

        // "second" should still be on the book
        let ids = book.orders_at_price(51_000, Side::Sell);
        assert_eq!(ids, vec![OrderId("second".into())]);
    }

    // ── Self-trade prevention (STP) ───────────────────────────────────────────

    #[test]
    fn stp_cancels_taker_when_same_trader() {
        let mut book = OrderBook::new();
        // Maker and taker are the same trader
        book.insert(make_order("maker", "trader-A", Side::Sell, 51_000, 10));

        let mut taker = make_order("taker", "trader-A", Side::Buy, 52_000, 10);

        let fills = match_order(&mut book, &mut taker, SEQ, NOW);

        // No fills — STP cancels the taker
        assert!(fills.is_empty());
        assert_eq!(taker.status, OrderStatus::CancelledStp);

        // Maker is untouched
        let maker = book.get(&OrderId("maker".into())).unwrap();
        assert_eq!(maker.status, OrderStatus::Open);
        assert_eq!(maker.remaining_size_lots(), 10);
    }

    #[test]
    fn stp_only_applies_to_self_trade_not_next_maker() {
        let mut book = OrderBook::new();
        // First maker: same trader as taker → STP triggers, matching stops
        // Second maker: different trader (would be a valid match if STP didn't trigger)
        book.insert(make_order("stp-maker",   "trader-A", Side::Sell, 51_000, 5));
        book.insert(make_order("other-maker", "trader-B", Side::Sell, 52_000, 5));

        let mut taker = make_order("taker", "trader-A", Side::Buy, 53_000, 10);

        let fills = match_order(&mut book, &mut taker, SEQ, NOW);

        // STP fires on first encounter — whole taker cancelled, no fills produced
        assert!(fills.is_empty());
        assert_eq!(taker.status, OrderStatus::CancelledStp);
    }

    // ── Fill correctness ──────────────────────────────────────────────────────

    #[test]
    fn fill_records_correct_trader_ids() {
        let mut book = OrderBook::new();
        book.insert(make_order("maker", "maker-trader", Side::Sell, 51_000, 10));

        let mut taker = make_order("taker", "taker-trader", Side::Buy, 51_000, 10);
        let fills = match_order(&mut book, &mut taker, SEQ, NOW);

        assert_eq!(fills[0].taker_trader_id, "taker-trader");
        assert_eq!(fills[0].maker_trader_id, "maker-trader");
        assert_eq!(fills[0].sequence_id, SEQ);
        assert_eq!(fills[0].executed_at_ms, NOW);
    }

    #[test]
    fn total_filled_qty_matches_sum_of_fills() {
        let mut book = OrderBook::new();
        book.insert(make_order("m1", "trader-A", Side::Sell, 51_000, 3));
        book.insert(make_order("m2", "trader-A", Side::Sell, 51_000, 4));
        book.insert(make_order("m3", "trader-A", Side::Sell, 52_000, 5));

        let mut taker = make_order("taker", "trader-B", Side::Buy, 53_000, 12);
        let fills = match_order(&mut book, &mut taker, SEQ, NOW);

        let total: u64 = fills.iter().map(|f| f.size_lots).sum();
        assert_eq!(total, taker.filled_size_lots);
        assert_eq!(total, 12);
    }
}
