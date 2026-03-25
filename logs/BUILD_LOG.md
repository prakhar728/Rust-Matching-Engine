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

## 2026-03-08 (continued)
### Goal
- Implement in-memory orderbook and matching engine.

### Work Done
- Wrote `src/engine/orderbook.rs`:
  - BTreeMap<price, VecDeque<OrderId>> for bids and asks (price-time priority structure)
  - `insert`, `cancel` (idempotent), `remove_from_queue`
  - `best_bid` / `best_ask` — O(1) BTreeMap first/last key
  - `ask_prices_asc` / `bid_prices_desc` return owned Vecs for borrow-safe matching
  - `l2_bids` / `l2_asks` — aggregated depth snapshots for REST API
- Wrote `src/engine/matching.rs`:
  - `match_order` entry point dispatches to buy/sell path
  - Price-time priority: sorted prices, FIFO queue per level
  - STP Cancel Taker: taker set to CancelledStp, maker untouched
  - MakerSnapshot pattern to avoid simultaneous borrows on OrderBook
  - Partial fills supported: both taker-partial and maker-partial

### Commands Run
- `cargo test`

### Result
- Pass — 59/59 tests green

### Next
- `src/replay/mod.rs` — deterministic rebuild from event log
- `src/engine/mod.rs` — Engine struct tying orderbook + sequencer + state together

## 2026-03-12
### Goal
- Implement StateStore, Engine coordinator, and deterministic replay.

### Work Done
- Wrote `src/db/mod.rs`:
  - `StateStore` — NEAR-compatible in-memory state (filled_amounts, cancelled, nonces, balances)
  - `apply_fill` — increments filled_amounts for both sides atomically
  - `check_and_update_nonce` — strictly monotonic, rejects nonce <= last accepted
  - `checksum` — deterministic SHA-256 fingerprint for replay verification (sorted iteration)
  - 11 tests green
- Wrote `src/engine/mod.rs`:
  - `Engine` — central coordinator: books + markets + sequencer + state + order_market_index + clock
  - `process_order` — full 12-step pipeline (validate → sig → trader_id → market → tick/lot → idempotency → nonce → sequence → match → fills → STP cancel → insert residual)
  - `process_cancel` — ownership check, idempotent
  - Clock injection via `Box<dyn Fn() -> u64>` for deterministic tests
  - O(1) order lookup via `order_market_index`
  - 15 tests green
- Wrote `src/replay/mod.rs`:
  - `replay(events)` — single-pass event log walker; rebuilds OrderBook + StateStore
  - Gap detection (`SeqIdGap` error) and unknown-order guard (`UnknownOrder` error)
  - OrderAccepted: record order + advance nonce (direct set, no validation)
  - Fill: update filled_size_lots on both sides + StateStore.apply_fill
  - OrderCancelled: set Cancelled/CancelledStp status + StateStore.cancel_order
  - Book construction from working set in created_sequence order (time priority preserved)
  - `verify_replay` — runs twice, asserts checksums match (determinism smoke test)
  - 11 tests green
- Added `#[derive(Debug)]` to `OrderBook` and `StateStore` (needed for `ReplayResult: Debug`)

### Commands Run
- `cargo test`

### Result
- Pass — 98/98 tests green, 0 failures
- Warnings only: dead code (expected — engine/replay not yet wired to API layer)

### Next
- `src/api/rest.rs` — POST /v1/orders, DELETE /v1/orders/{id}, GET /v1/orders/{id}, GET /v1/books/{market_id}
- `src/api/ws.rs` — WebSocket stream (L3 feed, trades, private order channel)
- `src/risk/mod.rs` — pre-trade checks, rate limits, circuit breakers
- `src/snapshot/mod.rs` — periodic snapshot + load_latest_snapshot
- `src/ops/admin.rs` — pause/resume market, circuit breaker toggle

## 2026-03-12 (continued)
### Goal
- Implement REST API layer with axum.

### Work Done
- Added dependencies: `axum = "0.7"`, `tokio = { version = "1", features = ["full"] }`, `tower-http = "0.5"`, dev-dep `tower` for test `oneshot`
- Updated `Engine::clock` field to `Box<dyn Fn() -> u64 + Send + Sync>` so `Engine` is `Send` and can be held in `Arc<Mutex<Engine>>`
- Added `Engine::list_markets()` — sorted Vec<&MarketConfig> for GET /v1/markets
- Added `Engine::get_fills(market_id, from_seq, limit)` — filters event log Fill events for GET /v1/trades
- Wrote `src/api/rest.rs`:
  - `AppState = Arc<Mutex<Engine>>`
  - `ApiError { code, message }` — uniform JSON error envelope
  - `engine_err_to_response` — maps every EngineError variant to the right HTTP status + code
  - `POST /v1/orders` → 201 Created or 200 OK (duplicate)
  - `DELETE /v1/orders/:order_id` — auth via `X-Trader-Id` header → 200 OK
  - `POST /v1/orders/cancel-by-client-id` — resolves via idempotency table → 200 OK
  - `GET /v1/orders/:order_id` — full order state → 200 OK
  - `GET /v1/books/:market_id?depth=N` — L2 snapshot, bids desc / asks asc → 200 OK
  - `GET /v1/trades/:market_id?limit=N&from_seq=N` — fills from event log → 200 OK
  - `GET /v1/markets` — sorted market list → 200 OK
  - `build_router(state)` — assembles all routes (cancel-by-client-id registered before :order_id to prevent capture)
  - 13 integration tests (tower::ServiceExt::oneshot — no real TCP listener)
- Updated `src/main.rs` — tokio::main, registers BTC-USDC + ETH-USDC, binds to 0.0.0.0:8080

### Commands Run
- `cargo test`
- `cargo build`

### Result
- Pass — 111/111 tests green, 0 failures, binary compiles clean
- Warnings only: dead code (risk/snapshot/ops/ws not yet written)

### Next
- `src/api/ws.rs` — WebSocket stream: book.l3.{market_id}, trades.{market_id}, orders.{trader_id}
- `src/risk/mod.rs` — pre-trade checks, rate limits, circuit breakers
- `src/snapshot/mod.rs` — create_snapshot, load_latest_snapshot
- `src/ops/admin.rs` — pause_market, resume_market, set_circuit_breaker

## 2026-03-12 (Phase -1 complete)
### Goal
- Implement WebSocket feed, risk layer, snapshot/recovery, and admin ops to complete Phase -1.

### Work Done
- Updated `api/mod.rs` — added `SharedState { engine: Mutex<Engine>, events: broadcast::Sender<Arc<WsEnvelope>> }` and `AppState = Arc<SharedState>`; added `WsEnvelope` with `from_sequenced()` fan-out logic (book.l3, trades, orders channels); `publish_since(seq_before)` re-locks engine briefly to snapshot new events
- Updated `api/rest.rs` — migrated to `SharedState`; mutating handlers now capture `seq_before`, release lock, then call `publish_since` so WS clients receive events without holding the engine lock during I/O
- Added `axum = { features = ["ws"] }` to Cargo.toml
- Wrote `api/ws.rs`:
  - `GET /v1/stream` WebSocket upgrade handler
  - Per-connection task: `tokio::select!` over ws recv + broadcast recv
  - Client → Server: `{ action: subscribe/unsubscribe/ping, channel, from_seq? }`
  - Server → Client: `{ type: event/subscribed/unsubscribed/pong/error/disconnected }`
  - `from_seq` reconnect: replays matching events from engine log, then switches to live broadcast
  - Lag handling: sends `{ type: disconnected, reason: lagged }` and closes on `RecvError::Lagged`
  - 10 unit tests green (message parsing, serialization, fan-out channel counts)
- Wrote `risk/mod.rs`:
  - `RiskChecker` with clock injection, per-trader rolling window rate limits, per-market max order size, and price band circuit breaker
  - `MarketRiskConfig`, `RiskConfig` — configurable per-market overrides
  - `update_reference_price(market_id, price_ticks)` — called after each fill
  - 9 tests green (rate limit, rollover, per-trader isolation, size limit, price band in/out, no-reference skip)
- Wrote `snapshot/mod.rs`:
  - `Snapshot { schema_version, snapshot_seq, state_checksum, events, markets }` — JSON serialized
  - `create_snapshot(path, events, markets, checksum)` — writes to disk
  - `load_snapshot(path)` — deserializes, verifies checksum, replays events
  - `find_latest_snapshot(dir)` — scans dir for `snapshot_{seq}.json`, returns highest
  - Uses `tempfile` dev-dep for test isolation
  - 5 tests green (roundtrip, checksum mismatch, version mismatch, find_latest, empty dir)
- Wrote `ops/admin.rs`:
  - `pause_market(engine, market_id, triggered_by)` — validates state, updates config, sequences MarketPaused event
  - `resume_market(engine, market_id, triggered_by)` — symmetric resume, sequences MarketResumed event
  - `cancel_all_orders(engine, market_id)` — iterates sequencer log for open orders, sequences AdminForce cancel events for each
  - `AdminError` — MarketNotFound, AlreadyInState, Settled
  - 8 tests green
- Updated `main.rs` — uses `SharedState::new(engine)`, merges REST + WS routers

### Commands Run
- `cargo test`
- `cargo build`

### Result
- Pass — 146/146 tests green, 0 failures, binary compiles clean
- Phase -1 core implementation complete

### What's Done (Phase -1 summary)
| Module | What it does |
|---|---|
| `domain/order.rs` | SignedOrder, canonical hash, ed25519 verify, Order, Fill |
| `domain/market.rs` | MarketConfig, tick/lot validation, MarketStatus |
| `events/mod.rs` | Event enum, SequencedEvent, CancelReason |
| `sequencer/mod.rs` | Monotonic seq_ids, idempotency table, resume |
| `engine/orderbook.rs` | BTreeMap+VecDeque price-time book, L2 snapshots |
| `engine/matching.rs` | Price-time matching, STP, partial fills |
| `engine/mod.rs` | Engine coordinator, 12-step process_order pipeline |
| `db/mod.rs` | StateStore (NEAR-compatible filled_amounts/cancelled/nonces) |
| `replay/mod.rs` | Deterministic rebuild from event log, gap detection |
| `api/mod.rs` | SharedState, WsEnvelope fan-out, broadcast publish |
| `api/rest.rs` | 7 REST endpoints, axum handlers, event publishing |
| `api/ws.rs` | WebSocket stream, subscribe/unsubscribe, from_seq replay |
| `risk/mod.rs` | Rate limits, order size limits, price band circuit breaker |
| `snapshot/mod.rs` | JSON snapshots, checksum verification, load+replay |
| `ops/admin.rs` | pause/resume market, cancel_all_orders (AdminForce) |

### Phase 0 next steps
- Wire RiskChecker into REST `post_order` handler (currently bypassed)
- Add admin REST endpoints (POST /admin/markets/:id/pause, etc.)
- Connect snapshot creation to a periodic background task
- PostgreSQL event log persistence (replace in-memory Sequencer log)
- Load test to validate ≥10k orders/sec throughput target
