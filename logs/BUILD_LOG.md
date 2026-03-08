# Build Log

## 2026-03-07
### Goal
- Implement Phase -1 domain layer: order types, canonical hashing, signature verification, market config.

### Work Done
- Added dependencies to `Cargo.toml`: serde, serde_json, sha2, ed25519-dalek, hex, thiserror, uuid, rand
- Wrote `src/domain/market.rs`:
  - `MarketId`, `MarketConfig`, `MarketStatus`, `MarketError`
  - `validate_price` and `validate_size` — tick/lot alignment checks
  - `is_active` — gate used by engine before accepting orders
- Wrote `src/domain/order.rs`:
  - `SignedOrder` — canonical client-submitted order (fields frozen for NEAR compatibility)
  - `canonical_hash()` — SHA-256 over fixed binary layout, field order documented and frozen
  - `verify_signature()` — ed25519 verify over the 32-byte hash
  - `validate_fields()` — stateless field checks (expiry, zeroes, schema version, non-empty)
  - `Order`, `Fill`, `OrderHash`, `OrderId`, `OrderStatus`, `Side`, `TimeInForce`, `OrderError`
  - Custom serde helpers for `[u8; 32]` and `[u8; 64]` (hex strings in JSON)

### Commands Run
- `cargo test`

### Result
- Pass — 20/20 tests green, 0 failures
- Warnings only: dead code (expected — types not yet used by engine)

### Next
- `src/events/mod.rs` — event enum (OrderPlaced, OrderCancelled, Fill) + SequencedEvent wrapper
- `src/sequencer/mod.rs` — monotonic seq_id assignment, idempotency check, append-only log
- `src/engine/orderbook.rs` — in-memory BTreeMap book, price-time priority structure

## 2026-03-08
### Goal
- Implement events and sequencer modules.

### Work Done
- Wrote `src/events/mod.rs`:
  - `Event` enum with full payload: OrderAccepted, OrderRejected, Fill, OrderCancelled, MarketPaused, MarketResumed
  - `CancelReason` enum: TraderRequest, Expired, SelfTradePrevention, AdminForce
  - `SequencedEvent` — wraps Event with seq_id and timestamp_ms
  - `Event::market_id()` helper for downstream filtering
- Wrote `src/sequencer/mod.rs`:
  - Monotonic seq_id counter starting at 1, no gaps
  - `append_event()` — assigns seq_id, wall-clock stamp, appends to log
  - `peek_next_seq_id()` — engine uses this to pre-set order.created_sequence
  - `idempotency_check()` + `register_accepted()` — duplicate submission detection keyed by (trader_id, client_order_id)
  - `resume()` — rebuilds idempotency table from prior event log for crash recovery

### Commands Run
- `cargo test`

### Result
- Pass — 32/32 tests green

### Next
- `src/engine/orderbook.rs` — in-memory BTreeMap book (price-time priority structure)
- `src/engine/matching.rs` — matching loop with STP, partial fills
