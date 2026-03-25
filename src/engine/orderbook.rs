// In-memory order book for a single market.
//
// Data layout:
//   bids: BTreeMap<price, VecDeque<OrderId>>  — all resting buy orders
//   asks: BTreeMap<price, VecDeque<OrderId>>  — all resting sell orders
//   orders: HashMap<OrderId, Order>           — full order state by ID
//
// Price-time priority is enforced by the combination of:
//   Price → BTreeMap gives us O(log n) sorted access.
//           Asks: iterate forward  (ascending)  → lowest ask first (best for buyers).
//           Bids: iterate backward (descending) → highest bid first (best for sellers).
//   Time  → VecDeque per price level is FIFO.
//           Orders are pushed to the back on insert (newest = back).
//           Orders are matched from the front (oldest = highest priority).
//
// Borrow safety note for the matching loop:
//   `ask_prices_asc()` and `orders_at_price()` both return owned Vec<_>,
//   which releases the borrow before the caller does any mutation.
//   This lets the matching loop safely alternate between reads and writes.

use std::collections::{BTreeMap, HashMap, VecDeque};
use serde::{Deserialize, Serialize};
use crate::domain::order::{Order, OrderId, OrderStatus, Side};

// ---------------------------------------------------------------------------
// Level-2 snapshot type
// ---------------------------------------------------------------------------

/// A single aggregated price level for the L2 (depth-of-book) API.
///
/// L2 collapses all orders at the same price into one row.
/// L3 (order-by-order) is the full `orders` HashMap — used for replay and feeds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceLevel {
    /// The price of this level in tick units.
    pub price: u64,
    /// Sum of `remaining_size_lots` across all open orders at this price.
    pub total_qty: u64,
    /// Number of individual orders resting at this price.
    pub order_count: usize,
}

// ---------------------------------------------------------------------------
// OrderBook
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct OrderBook {
    /// Buy-side price levels. Natural BTreeMap iteration is ascending;
    /// we use `.rev()` wherever we need highest bid first.
    bids: BTreeMap<u64, VecDeque<OrderId>>,

    /// Sell-side price levels. Natural ascending iteration gives best ask first.
    asks: BTreeMap<u64, VecDeque<OrderId>>,

    /// Full order state. Orders are kept here permanently for audit — even
    /// after filling or cancellation. Matching reads and writes this map.
    orders: HashMap<OrderId, Order>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            orders: HashMap::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Writes
    // -----------------------------------------------------------------------

    /// Insert a resting order into the book.
    ///
    /// Called by the engine after matching, if the incoming order has residual
    /// quantity left. The order must have status Open or PartiallyFilled.
    pub fn insert(&mut self, order: Order) {
        let id = order.order_id.clone();
        let price = order.price_ticks;
        let side = order.side;

        self.orders.insert(id.clone(), order);

        // Push to the back of the price-level queue (newest = lowest priority)
        match side {
            Side::Buy => self.bids.entry(price).or_insert_with(VecDeque::new).push_back(id),
            Side::Sell => self.asks.entry(price).or_insert_with(VecDeque::new).push_back(id),
        }
    }

    /// Cancel an order: set its status to Cancelled and remove from the queue.
    ///
    /// Returns `true` if the order was found and cancelled.
    /// Returns `false` if not found or already closed (idempotent).
    pub fn cancel(&mut self, id: &OrderId) -> bool {
        self.cancel_with_status(id, OrderStatus::Cancelled)
    }

    /// Cancel an order with a specific final status.
    ///
    /// Used by replay to set `CancelledStp` correctly — in the live engine,
    /// STP status is set by the matching loop before the order ever touches
    /// the book, but during replay we insert first and cancel second.
    pub fn cancel_with_status(&mut self, id: &OrderId, status: OrderStatus) -> bool {
        let order = match self.orders.get_mut(id) {
            Some(o) => o,
            None => return false,
        };

        if order.is_closed() {
            return false; // already done — idempotent cancel is fine
        }

        order.status = status;
        self.remove_from_queue_inner(id);

        true
    }

    /// Remove an order from its price-level queue without changing its status.
    ///
    /// Used by the matching engine when an order is fully filled (status is
    /// already set to Filled by the caller before this is called).
    /// Also used internally by `cancel()`.
    pub fn remove_from_queue(&mut self, id: &OrderId) {
        self.remove_from_queue_inner(id);
    }

    // -----------------------------------------------------------------------
    // Reads
    // -----------------------------------------------------------------------

    /// Look up a full order by ID.
    pub fn get(&self, id: &OrderId) -> Option<&Order> {
        self.orders.get(id)
    }

    /// Mutable access to an order by ID — used by the matching loop to apply fills.
    pub fn get_mut(&mut self, id: &OrderId) -> Option<&mut Order> {
        self.orders.get_mut(id)
    }

    /// Best (highest) bid price currently resting in the book.
    pub fn best_bid(&self) -> Option<u64> {
        self.bids.keys().next_back().copied()
    }

    /// Best (lowest) ask price currently resting in the book.
    pub fn best_ask(&self) -> Option<u64> {
        self.asks.keys().next().copied()
    }

    /// All ask prices in ascending order (best ask first).
    ///
    /// Returns an owned Vec so the borrow is released before the caller
    /// does any mutations — required for borrow safety in the match loop.
    pub fn ask_prices_asc(&self) -> Vec<u64> {
        self.asks.keys().copied().collect()
    }

    /// All bid prices in descending order (best bid first).
    pub fn bid_prices_desc(&self) -> Vec<u64> {
        self.bids.keys().rev().copied().collect()
    }

    /// Snapshot of order IDs at a price level, in FIFO order (oldest first).
    ///
    /// Returns an owned Vec — borrow released before the caller mutates.
    /// Callers must re-check order state after getting this snapshot, since
    /// the underlying queue may have changed between the snapshot and processing.
    pub fn orders_at_price(&self, price: u64, side: Side) -> Vec<OrderId> {
        let queue = match side {
            Side::Buy => self.bids.get(&price),
            Side::Sell => self.asks.get(&price),
        };
        queue.map(|q| q.iter().cloned().collect()).unwrap_or_default()
    }

    /// Total number of orders tracked (open + closed, for audit).
    pub fn order_count(&self) -> usize {
        self.orders.len()
    }

    // -----------------------------------------------------------------------
    // L2 snapshots (for the REST API)
    // -----------------------------------------------------------------------

    /// Aggregated bid-side depth, best (highest) price first, up to `depth` levels.
    pub fn l2_bids(&self, depth: usize) -> Vec<PriceLevel> {
        self.bids
            .iter()
            .rev() // highest price first
            .take(depth)
            .map(|(price, queue)| {
                let total_qty = queue
                    .iter()
                    .filter_map(|id| self.orders.get(id))
                    .map(|o| o.remaining_size_lots())
                    .sum();
                PriceLevel {
                    price: *price,
                    total_qty,
                    order_count: queue.len(),
                }
            })
            .collect()
    }

    /// Aggregated ask-side depth, best (lowest) price first, up to `depth` levels.
    pub fn l2_asks(&self, depth: usize) -> Vec<PriceLevel> {
        self.asks
            .iter() // lowest price first
            .take(depth)
            .map(|(price, queue)| {
                let total_qty = queue
                    .iter()
                    .filter_map(|id| self.orders.get(id))
                    .map(|o| o.remaining_size_lots())
                    .sum();
                PriceLevel {
                    price: *price,
                    total_qty,
                    order_count: queue.len(),
                }
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Remove an order from its price-level queue. Cleans up the price level
    /// entry if the queue becomes empty. Does not change the order's status.
    fn remove_from_queue_inner(&mut self, id: &OrderId) {
        // Look up the order's price and side so we know which map to update.
        // We can't hold the borrow while mutating, so we copy the values out.
        let (price, side) = match self.orders.get(id) {
            Some(o) => (o.price_ticks, o.side),
            None => return,
        };

        let queue = match side {
            Side::Buy => self.bids.get_mut(&price),
            Side::Sell => self.asks.get_mut(&price),
        };

        if let Some(q) = queue {
            // O(n) in queue length — acceptable for Phase -1.
            // For high-throughput production, use an indexed structure.
            q.retain(|oid| oid != id);
            if q.is_empty() {
                match side {
                    Side::Buy => { self.bids.remove(&price); }
                    Side::Sell => { self.asks.remove(&price); }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::order::{Order, OrderHash, OrderId, OrderStatus, Side, TimeInForce};
    use crate::domain::market::MarketId;

    /// Build a minimal Order for testing — all boilerplate, focus on the fields
    /// that vary per test (side, price, size).
    fn make_order(id: &str, side: Side, price: u64, size: u64) -> Order {
        Order {
            order_id: OrderId(id.into()),
            client_order_id: format!("client-{id}"),
            order_hash: OrderHash([0u8; 32]),
            market_id: MarketId("BTC-USDC".into()),
            trader_id: "trader-1".into(),
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

    #[test]
    fn empty_book_has_no_best_prices() {
        let book = OrderBook::new();
        assert!(book.best_bid().is_none());
        assert!(book.best_ask().is_none());
    }

    #[test]
    fn insert_buy_sets_best_bid() {
        let mut book = OrderBook::new();
        book.insert(make_order("o1", Side::Buy, 50_000, 10));
        assert_eq!(book.best_bid(), Some(50_000));
        assert!(book.best_ask().is_none());
    }

    #[test]
    fn insert_sell_sets_best_ask() {
        let mut book = OrderBook::new();
        book.insert(make_order("o1", Side::Sell, 51_000, 5));
        assert_eq!(book.best_ask(), Some(51_000));
        assert!(book.best_bid().is_none());
    }

    #[test]
    fn best_bid_is_highest_price() {
        let mut book = OrderBook::new();
        book.insert(make_order("o1", Side::Buy, 49_000, 10));
        book.insert(make_order("o2", Side::Buy, 50_000, 10)); // better bid
        book.insert(make_order("o3", Side::Buy, 48_000, 10));
        assert_eq!(book.best_bid(), Some(50_000));
    }

    #[test]
    fn best_ask_is_lowest_price() {
        let mut book = OrderBook::new();
        book.insert(make_order("o1", Side::Sell, 52_000, 5));
        book.insert(make_order("o2", Side::Sell, 51_000, 5)); // better ask
        book.insert(make_order("o3", Side::Sell, 53_000, 5));
        assert_eq!(book.best_ask(), Some(51_000));
    }

    #[test]
    fn cancel_removes_order_and_updates_best_bid() {
        let mut book = OrderBook::new();
        book.insert(make_order("o1", Side::Buy, 50_000, 10));
        book.insert(make_order("o2", Side::Buy, 49_000, 10));

        assert!(book.cancel(&OrderId("o1".into())));
        // best bid should now be the lower one
        assert_eq!(book.best_bid(), Some(49_000));
    }

    #[test]
    fn cancel_is_idempotent() {
        let mut book = OrderBook::new();
        book.insert(make_order("o1", Side::Buy, 50_000, 10));

        assert!(book.cancel(&OrderId("o1".into())));
        // Second cancel on an already-cancelled order returns false
        assert!(!book.cancel(&OrderId("o1".into())));
    }

    #[test]
    fn cancel_nonexistent_order_returns_false() {
        let mut book = OrderBook::new();
        assert!(!book.cancel(&OrderId("ghost".into())));
    }

    #[test]
    fn orders_at_price_returns_fifo_order() {
        let mut book = OrderBook::new();
        // Insert three orders at the same price — they should come back in insert order
        book.insert(make_order("first",  Side::Sell, 51_000, 5));
        book.insert(make_order("second", Side::Sell, 51_000, 5));
        book.insert(make_order("third",  Side::Sell, 51_000, 5));

        let ids = book.orders_at_price(51_000, Side::Sell);
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0], OrderId("first".into()));
        assert_eq!(ids[1], OrderId("second".into()));
        assert_eq!(ids[2], OrderId("third".into()));
    }

    #[test]
    fn remove_from_queue_cleans_empty_price_level() {
        let mut book = OrderBook::new();
        book.insert(make_order("o1", Side::Sell, 51_000, 5));

        // Manually set to Filled so is_closed() passes, then remove from queue
        book.get_mut(&OrderId("o1".into())).unwrap().status = OrderStatus::Filled;
        book.remove_from_queue(&OrderId("o1".into()));

        // Price level should be gone
        assert_eq!(book.ask_prices_asc(), Vec::<u64>::new());
        assert!(book.best_ask().is_none());
    }

    #[test]
    fn l2_bids_aggregates_correctly() {
        let mut book = OrderBook::new();
        book.insert(make_order("o1", Side::Buy, 50_000, 10));
        book.insert(make_order("o2", Side::Buy, 50_000, 5));  // same price level
        book.insert(make_order("o3", Side::Buy, 49_000, 20)); // different level

        let bids = book.l2_bids(10);
        assert_eq!(bids.len(), 2);
        // Best bid first
        assert_eq!(bids[0].price, 50_000);
        assert_eq!(bids[0].total_qty, 15);   // 10 + 5
        assert_eq!(bids[0].order_count, 2);
        assert_eq!(bids[1].price, 49_000);
        assert_eq!(bids[1].total_qty, 20);
    }

    #[test]
    fn l2_asks_aggregates_correctly() {
        let mut book = OrderBook::new();
        book.insert(make_order("o1", Side::Sell, 51_000, 8));
        book.insert(make_order("o2", Side::Sell, 51_000, 2));  // same level
        book.insert(make_order("o3", Side::Sell, 52_000, 15));

        let asks = book.l2_asks(10);
        assert_eq!(asks.len(), 2);
        // Best ask (lowest price) first
        assert_eq!(asks[0].price, 51_000);
        assert_eq!(asks[0].total_qty, 10); // 8 + 2
        assert_eq!(asks[1].price, 52_000);
        assert_eq!(asks[1].total_qty, 15);
    }

    #[test]
    fn l2_depth_limits_levels() {
        let mut book = OrderBook::new();
        for price in [50_000, 49_000, 48_000, 47_000, 46_000] {
            book.insert(make_order(&price.to_string(), Side::Buy, price, 1));
        }
        let bids = book.l2_bids(3);
        assert_eq!(bids.len(), 3);
        assert_eq!(bids[0].price, 50_000); // best bid first
    }

    #[test]
    fn ask_prices_are_ascending() {
        let mut book = OrderBook::new();
        book.insert(make_order("o1", Side::Sell, 53_000, 1));
        book.insert(make_order("o2", Side::Sell, 51_000, 1));
        book.insert(make_order("o3", Side::Sell, 52_000, 1));

        let prices = book.ask_prices_asc();
        assert_eq!(prices, vec![51_000, 52_000, 53_000]);
    }

    #[test]
    fn bid_prices_are_descending() {
        let mut book = OrderBook::new();
        book.insert(make_order("o1", Side::Buy, 48_000, 1));
        book.insert(make_order("o2", Side::Buy, 50_000, 1));
        book.insert(make_order("o3", Side::Buy, 49_000, 1));

        let prices = book.bid_prices_desc();
        assert_eq!(prices, vec![50_000, 49_000, 48_000]);
    }
}
