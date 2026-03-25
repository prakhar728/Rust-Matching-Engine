// Sequencer: monotonic sequence ID assignment, idempotency, and event log.
//
// The sequencer is the single writer that imposes total order on all events.
// Every state change in the engine must go through here before it takes effect.
//
// Why does this matter?
//   In a distributed system you need a total order over events to guarantee
//   deterministic replay. In V1 we have a single-operator setup, so the
//   sequencer is just an atomic counter + append-only log. The architecture
//   is written so this can later be replaced with a decentralized sequencer
//   (e.g. a NEAR contract or a consensus layer) without changing the rest of
//   the engine.
//
// Key invariants (from phase_-1.md §7):
//   1. seq_ids are strictly increasing starting from 1, with no gaps.
//   2. Every state mutation produces at least one sequenced event.
//   3. Duplicate client_order_id submissions return the original result — no
//      new event is created (idempotency guarantee).
//   4. The event log is append-only — entries are never modified or deleted.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use serde::{Deserialize, Serialize};
use crate::events::{Event, SequencedEvent};
use crate::domain::order::{OrderHash, OrderId};

// ---------------------------------------------------------------------------
// Idempotency key
// ---------------------------------------------------------------------------

/// Key used to detect duplicate order submissions.
///
/// A (trader_id, client_order_id) pair must be unique per submission window.
/// If the same pair is submitted again, the sequencer returns the original
/// seq_id without creating a new event.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IdempotencyKey {
    pub trader_id: String,
    pub client_order_id: String,
}

/// The result of a previous accepted submission, returned on duplicate.
#[derive(Debug, Clone)]
pub struct IdempotencyRecord {
    /// The seq_id of the original OrderAccepted event.
    pub original_seq_id: u64,
    /// The server-assigned order ID — returned to the client on duplicate.
    pub order_id: OrderId,
    /// The canonical hash — returned to the client on duplicate.
    pub order_hash: OrderHash,
    /// When it was first accepted (epoch ms).
    pub accepted_at_ms: u64,
}

// ---------------------------------------------------------------------------
// Sequencer
// ---------------------------------------------------------------------------

/// The single-writer sequencer that assigns monotonic seq_ids to all events.
///
/// All state mutations in the engine must be routed through `append_event()`.
/// The sequencer:
///   1. Assigns the next seq_id (incrementing counter).
///   2. Stamps the event with the current wall-clock time.
///   3. Appends the SequencedEvent to the in-memory log.
///
/// The in-memory log (`self.log`) is the authoritative state during a session.
/// On restart, the log is rebuilt from the persistent store (Postgres) via
/// snapshot + replay (see `src/replay/mod.rs`).
pub struct Sequencer {
    /// Next seq_id to assign. Starts at 1, increments by 1 per event.
    /// Never reset — even across restarts (loaded from persisted log head).
    next_seq_id: u64,

    /// The append-only in-memory event log for the current session.
    /// Index 0 = earliest event. Entries are never mutated or removed.
    pub log: Vec<SequencedEvent>,

    /// Idempotency table: (trader_id, client_order_id) → original acceptance record.
    /// Used to detect and short-circuit duplicate order submissions.
    ///
    /// Phase -1: kept in memory, no expiry window (entries persist for session lifetime).
    /// Phase 1+: entries should expire after a configurable time window.
    idempotency: HashMap<IdempotencyKey, IdempotencyRecord>,
}

impl Default for Sequencer {
    fn default() -> Self {
        Self::new()
    }
}

impl Sequencer {
    /// Create a new sequencer starting from seq_id = 1.
    /// Used on a fresh start when there's no existing event log to resume from.
    pub fn new() -> Self {
        Self {
            next_seq_id: 1,
            log: Vec::new(),
            idempotency: HashMap::new(),
        }
    }

    /// Resume from an existing log (e.g. after crash recovery via replay).
    /// `resume_from_seq_id` should be log.last().seq_id + 1.
    pub fn resume(resume_from_seq_id: u64, log: Vec<SequencedEvent>) -> Self {
        // Rebuild idempotency table from the replayed log so duplicate detection
        // works correctly after restart.
        let mut idempotency = HashMap::new();
        for ev in &log {
            if let crate::events::Event::OrderAccepted { order } = &ev.event {
                let key = IdempotencyKey {
                    trader_id: order.trader_id.clone(),
                    client_order_id: order.client_order_id.clone(),
                };
                idempotency.insert(key, IdempotencyRecord {
                    original_seq_id: ev.seq_id,
                    order_id: order.order_id.clone(),
                    order_hash: order.order_hash,
                    accepted_at_ms: ev.timestamp_ms,
                });
            }
        }

        Self {
            next_seq_id: resume_from_seq_id,
            log,
            idempotency,
        }
    }

    /// Peek at the next seq_id without consuming it.
    ///
    /// Used by the engine to set `order.created_sequence` before calling
    /// `append_event()`, so the stored event contains the correct seq_id.
    pub fn peek_next_seq_id(&self) -> u64 {
        self.next_seq_id
    }

    /// Assign a seq_id, stamp with current time, and append to the log.
    ///
    /// This is the only way to add entries to the log. Returns a reference
    /// to the event just appended (useful to read back the assigned seq_id).
    ///
    /// Note: wall-clock `timestamp_ms` is for observability only. Matching
    /// decisions are made based on seq_id, not wall-clock time, so the engine
    /// remains deterministic even if system clocks drift.
    pub fn append_event(&mut self, event: Event) -> &SequencedEvent {
        let seq_id = self.next_seq_id;
        self.next_seq_id += 1;

        let timestamp_ms = current_time_ms();

        self.log.push(SequencedEvent {
            seq_id,
            timestamp_ms,
            event,
        });

        self.log.last().unwrap()
    }

    /// Check whether a (trader_id, client_order_id) pair has already been accepted.
    ///
    /// Returns `Some(record)` if this is a duplicate — the caller should return
    /// the original result without creating a new order.
    /// Returns `None` if this is a fresh submission.
    pub fn idempotency_check(
        &self,
        trader_id: &str,
        client_order_id: &str,
    ) -> Option<&IdempotencyRecord> {
        let key = IdempotencyKey {
            trader_id: trader_id.to_string(),
            client_order_id: client_order_id.to_string(),
        };
        self.idempotency.get(&key)
    }

    /// Register a successful order acceptance in the idempotency table.
    ///
    /// Must be called immediately after appending the OrderAccepted event,
    /// before returning to the caller. This ensures the next duplicate
    /// submission is correctly detected.
    pub fn register_accepted(
        &mut self,
        trader_id: &str,
        client_order_id: &str,
        seq_id: u64,
        order_id: OrderId,
        order_hash: OrderHash,
        accepted_at_ms: u64,
    ) {
        let key = IdempotencyKey {
            trader_id: trader_id.to_string(),
            client_order_id: client_order_id.to_string(),
        };
        self.idempotency.insert(key, IdempotencyRecord {
            original_seq_id: seq_id,
            order_id,
            order_hash,
            accepted_at_ms,
        });
    }

    /// Total number of events in the log.
    pub fn len(&self) -> usize {
        self.log.len()
    }

    pub fn is_empty(&self) -> bool {
        self.log.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Current wall-clock time as epoch milliseconds.
/// Isolated here so it's easy to mock in tests if needed.
fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::MarketId;
    use crate::events::Event;
    use crate::domain::order::{Order, OrderHash, OrderId, OrderStatus, Side, TimeInForce};

    fn pause_event() -> Event {
        Event::MarketPaused {
            market_id: MarketId("BTC-USDC".into()),
            triggered_by: "test".into(),
        }
    }

    #[test]
    fn seq_ids_start_at_one() {
        let mut seq = Sequencer::new();
        let ev = seq.append_event(pause_event());
        assert_eq!(ev.seq_id, 1);
    }

    #[test]
    fn seq_ids_are_strictly_increasing() {
        let mut seq = Sequencer::new();
        let e1 = seq.append_event(pause_event()).seq_id;
        let e2 = seq.append_event(pause_event()).seq_id;
        let e3 = seq.append_event(pause_event()).seq_id;
        assert_eq!(e1, 1);
        assert_eq!(e2, 2);
        assert_eq!(e3, 3);
    }

    #[test]
    fn seq_ids_have_no_gaps() {
        let mut seq = Sequencer::new();
        for _ in 0..100 {
            seq.append_event(pause_event());
        }
        // All seq_ids from 1..=100 must appear in order
        for (i, ev) in seq.log.iter().enumerate() {
            assert_eq!(ev.seq_id, (i + 1) as u64);
        }
    }

    #[test]
    fn peek_does_not_advance_counter() {
        let mut seq = Sequencer::new();
        assert_eq!(seq.peek_next_seq_id(), 1);
        assert_eq!(seq.peek_next_seq_id(), 1); // still 1
        seq.append_event(pause_event());
        assert_eq!(seq.peek_next_seq_id(), 2); // now advanced
    }

    #[test]
    fn idempotency_check_returns_none_for_new_key() {
        let seq = Sequencer::new();
        assert!(seq.idempotency_check("trader-1", "order-abc").is_none());
    }

    #[test]
    fn idempotency_check_returns_record_after_registration() {
        let mut seq = Sequencer::new();
        seq.register_accepted("trader-1", "order-abc", 5, OrderId("oid-1".into()), OrderHash([0u8; 32]), 1_000_000);

        let record = seq.idempotency_check("trader-1", "order-abc");
        assert!(record.is_some());
        assert_eq!(record.unwrap().original_seq_id, 5);
    }

    #[test]
    fn idempotency_is_keyed_by_trader_and_client_order_id() {
        let mut seq = Sequencer::new();
        seq.register_accepted("trader-1", "order-abc", 1, OrderId("oid-1".into()), OrderHash([0u8; 32]), 0);
        seq.register_accepted("trader-2", "order-abc", 2, OrderId("oid-2".into()), OrderHash([0u8; 32]), 0);

        // Same client_order_id but different trader_id — these are separate
        assert_eq!(seq.idempotency_check("trader-1", "order-abc").unwrap().original_seq_id, 1);
        assert_eq!(seq.idempotency_check("trader-2", "order-abc").unwrap().original_seq_id, 2);
        assert!(seq.idempotency_check("trader-3", "order-abc").is_none());
    }

    #[test]
    fn log_is_append_only() {
        let mut seq = Sequencer::new();
        seq.append_event(pause_event());
        seq.append_event(pause_event());
        seq.append_event(pause_event());

        // Log should have exactly 3 entries, in order
        assert_eq!(seq.len(), 3);
        assert_eq!(seq.log[0].seq_id, 1);
        assert_eq!(seq.log[1].seq_id, 2);
        assert_eq!(seq.log[2].seq_id, 3);
    }

    #[test]
    fn resume_rebuilds_idempotency_from_existing_log() {
        // Simulate a crash-recovery scenario: we have an existing log from
        // a previous session with one accepted order.
    

        let order = Order {
            order_id: OrderId("oid-1".into()),
            client_order_id: "client-1".into(),
            order_hash: OrderHash([0u8; 32]),
            market_id: MarketId("BTC-USDC".into()),
            trader_id: "trader-1".into(),
            trader_pubkey: [0u8; 32],
            side: Side::Buy,
            price_ticks: 1000,
            orig_size_lots: 10,
            filled_size_lots: 0,
            time_in_force: TimeInForce::Gtc,
            nonce: 1,
            expiry_ts_ms: 0,
            status: OrderStatus::Open,
            created_sequence: 1,
            last_update_sequence: 1,
            accepted_at_ms: 1_000_000,
        };

        let prior_log = vec![SequencedEvent {
            seq_id: 1,
            timestamp_ms: 1_000_000,
            event: Event::OrderAccepted { order },
        }];

        // Resume from seq_id 2 (next after the existing log)
        let seq = Sequencer::resume(2, prior_log);

        // Idempotency should be rebuilt: trader-1 / client-1 is already known
        let record = seq.idempotency_check("trader-1", "client-1");
        assert!(record.is_some());
        assert_eq!(record.unwrap().original_seq_id, 1);

        // Next seq_id should pick up where the log left off
        assert_eq!(seq.peek_next_seq_id(), 2);
    }
}
