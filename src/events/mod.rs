// Event definitions for the append-only event log.
//
// Every state change in the engine is expressed as an Event, wrapped in a
// SequencedEvent (which adds a monotonic seq_id and timestamp). The log of
// SequencedEvents is the single source of truth — everything else (in-memory
// books, DB query tables, WS feeds) is derived from it.
//
// Why an event log?
//   - Deterministic replay: replay the same events → identical state.
//   - Audit trail: every action is permanently recorded with a sequence number.
//   - Recovery: crash → load latest snapshot → replay events since snapshot.
//   - On-chain readiness: settlement batches are just subsets of fill events.
//
// Event ordering rules (from phase_-1.md §7):
//   - Global total order by seq_id.
//   - No gaps: seq_ids are 1, 2, 3, ... with no holes.
//   - Consumers must process events in strictly ascending seq_id order.

use serde::{Deserialize, Serialize};
use crate::domain::market::MarketId;
use crate::domain::order::{Fill, Order, OrderHash, OrderId};

// ---------------------------------------------------------------------------
// Event payload types
// ---------------------------------------------------------------------------

/// Why an order was cancelled — recorded in the event log for audit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    /// The trader explicitly requested cancellation via the API.
    TraderRequest,
    /// The order expired before it could be matched (expiry_ts_ms passed).
    Expired,
    /// Self-trade prevention: the taker would have matched its own resting maker.
    /// The taker quantity is dropped; the maker stays on the book.
    SelfTradePrevention,
    /// An admin or circuit breaker cancelled the order.
    AdminForce,
}

// ---------------------------------------------------------------------------
// Event enum
// ---------------------------------------------------------------------------

/// All state-changing events produced by the engine.
///
/// Every event here corresponds to a mutation in the matching engine.
/// Non-mutating actions (e.g. querying the book) do not produce events.
///
/// Design note: we store the full Order/Fill payload in the event rather than
/// just IDs. This makes replay self-contained — no need to join against a DB
/// to reconstruct the book from the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// An order passed all validation checks and was accepted into the engine.
    /// The full Order struct is stored so replay can reconstruct exact state.
    OrderAccepted {
        order: Order,
    },

    /// An order failed validation and was not placed.
    /// Stored for audit — we want a complete record of all intake attempts.
    OrderRejected {
        /// The client's idempotency key (so they can correlate with their submit).
        client_order_id: String,
        /// Who submitted it.
        trader_id: String,
        /// Human-readable rejection reason (maps to API error code).
        reason: String,
    },

    /// A fill occurred between a taker and a maker.
    /// One Fill event is emitted per maker order touched in the match loop —
    /// a single aggressive order can produce multiple Fill events if it walks
    /// through several price levels.
    Fill(Fill),

    /// An order was removed from the book (trader cancel, expiry, or STP).
    OrderCancelled {
        order_id: OrderId,
        /// Stable hash key — used to mark cancelled[order_hash] = true in state.
        order_hash: OrderHash,
        reason: CancelReason,
    },

    /// A market was administratively paused. No new orders accepted.
    MarketPaused {
        market_id: MarketId,
        /// Admin user or system component that triggered the pause.
        triggered_by: String,
    },

    /// A previously paused market was resumed. Orders accepted again.
    MarketResumed {
        market_id: MarketId,
        triggered_by: String,
    },
}

impl Event {
    /// Returns the market_id this event belongs to, if applicable.
    /// Used by downstream consumers to filter events by market.
    pub fn market_id(&self) -> Option<&MarketId> {
        match self {
            Event::OrderAccepted { order } => Some(&order.market_id),
            Event::OrderRejected { .. } => None, // no market_id on rejected orders
            Event::Fill(fill) => Some(&fill.market_id),
            Event::OrderCancelled { .. } => None, // order_id lookup needed
            Event::MarketPaused { market_id, .. } => Some(market_id),
            Event::MarketResumed { market_id, .. } => Some(market_id),
        }
    }
}

// ---------------------------------------------------------------------------
// SequencedEvent
// ---------------------------------------------------------------------------

/// An event wrapped with a globally monotonic sequence number and a timestamp.
///
/// The seq_id is assigned by the Sequencer before any state mutation happens.
/// This ordering guarantee is what makes deterministic replay possible:
///   given the same list of SequencedEvents, the engine always reaches the same state.
///
/// Key invariant: seq_ids are strictly increasing with no gaps (1, 2, 3, ...).
/// A gap in seq_ids during replay indicates data corruption or a missing event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequencedEvent {
    /// Globally monotonic, starting from 1. Never repeats, never skips.
    pub seq_id: u64,

    /// Wall-clock timestamp in epoch milliseconds when the event was sequenced.
    /// Used for latency measurement and audit — NOT used in matching decisions.
    /// (Matching decisions depend only on seq_id, not wall-clock time.)
    pub timestamp_ms: u64,

    pub event: Event,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::MarketId;
    use crate::domain::order::{OrderHash, OrderId};

    #[test]
    fn event_serializes_to_json() {
        // Verify that serde round-trips correctly for a simple event.
        // This catches any missing serde annotations before they surface at runtime.
        let event = Event::MarketPaused {
            market_id: MarketId("BTC-USDC".into()),
            triggered_by: "admin".into(),
        };
        let json = serde_json::to_string(&event).expect("serialize failed");
        assert!(json.contains("market_paused"));
        assert!(json.contains("BTC-USDC"));
    }

    #[test]
    fn sequenced_event_has_correct_seq_id() {
        let event = SequencedEvent {
            seq_id: 42,
            timestamp_ms: 1_000_000,
            event: Event::MarketResumed {
                market_id: MarketId("ETH-USDC".into()),
                triggered_by: "ops".into(),
            },
        };
        assert_eq!(event.seq_id, 42);
    }

    #[test]
    fn order_cancelled_event_serializes() {
        let event = Event::OrderCancelled {
            order_id: OrderId("order-abc".into()),
            order_hash: OrderHash([1u8; 32]),
            reason: CancelReason::TraderRequest,
        };
        let json = serde_json::to_string(&event).expect("serialize failed");
        assert!(json.contains("order_cancelled"));
        assert!(json.contains("trader_request"));
    }
}
