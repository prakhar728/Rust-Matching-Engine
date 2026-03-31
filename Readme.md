# Rust Matching Engine

I wrote a matching engine in Rust, wanted to get a good hands on how things work, through the process of making and designing it I also came across some good and worthy concepts. This repo is a demonstration of how an order-book code will be in a development stage, this implements Limit Orders only at this point, and is nowhere near scaling production ready. 

What's under the hood:
- Price-time priority matching
- ed25519-signed orders
- event-sourced persistence
- REST + WebSocket API

![CI](https://github.com/prakhar728/CLOB/actions/workflows/ci.yml/badge.svg)

Also, if you notice, the repo has no frontend. This was done purposefully since my aim is not to build an exchange but to work on the backend capabilities and study it while building.

---

## What it does

Accepts signed limit orders over HTTP, matches them using price-time priority, persists every event to Postgres, and streams fills to connected WebSocket clients in real time.

```
POST /v1/orders  →  RiskChecker  →  Engine (Mutex)  →  Sequencer  →  OrderBook
                                                              ↓
                                                       Postgres event log
                                                              ↓
                                                       WS broadcast channel
```

- **Matching**: BTreeMap per side, VecDeque per price level. Taker walks the book, maker gets exact price, taker gets price improvement.
- **Auth**: Every order carries an ed25519 signature over a canonical SHA-256 hash. The engine verifies on intake — no trust in the transport layer.
- **Sequencing**: Monotonic seq_id assigned to every event. Deterministic replay from the event log rebuilds identical state.
- **Risk**: Per-trader rate limiting (100 orders/sec default), max order size, and price-band circuit breaker — all checked before the engine lock.
- **Persistence**: Append-only `sequenced_events` table in Postgres. On restart, the engine replays the log to rebuild in-memory state. Periodic snapshots bound replay time to the last 60 seconds of activity.

---

## Benchmarks

Measured on an Apple M-series MacBook Pro. Criterion framework, release profile. ed25519 signing excluded from timing (pre-generated outside the loop).

| Scenario | Throughput | Notes |
|---|---|---|
| Resting orders (no match) | **~42.9k orders/sec** | Pure intake: validation + sequencing + book insertion |
| 1:1 matching (full fills) | **~38.4k orders/sec** | Price crossing, fill recording, StateStore updates |
| Sweep (1 order clears N levels) | **~42.7k orders/sec** | Inner fill loop is tight, negligible per-level overhead |

These numbers measure the engine in isolation — no HTTP, no JSON, no network. Real HTTP throughput under concurrent load is not yet benchmarked.

Run benchmarks yourself:
```bash
cargo bench
```

---

## Quick start

Requires Docker and Docker Compose.

```bash
git clone https://github.com/prakhar728/CLOB.git
cd CLOB
docker compose up --build
```

Postgres starts, the engine connects, the `sequenced_events` table is created, and the API is live on `http://localhost:8080`.

```bash
curl http://localhost:8080/v1/markets
```

To stop:
```bash
docker compose down
```

To reset the database:
```bash
docker compose down -v
```

---

## Try it with the CLI

The `clob` binary ships in the same repo. Build it:

```bash
cargo build --release --bin clob
```

Generate a keypair:
```bash
./target/release/clob keygen --output trader.key
# trader_id: <your hex pubkey printed here>
```

Place an order:
```bash
./target/release/clob place \
  --market BTC-USDC \
  --side buy \
  --price 49000000 \
  --size 10 \
  --key trader.key
```

Check the book:
```bash
./target/release/clob book BTC-USDC
```

Cancel an order:
```bash
./target/release/clob cancel <order_id> --key trader.key
```

---

## API reference

### Orders

| Method | Path | Description |
|---|---|---|
| `POST` | `/v1/orders` | Submit a signed limit order |
| `GET` | `/v1/orders/:order_id` | Get order status |
| `DELETE` | `/v1/orders/:order_id` | Cancel by server order ID (requires `X-Trader-Id` header) |
| `POST` | `/v1/orders/cancel-by-client-id` | Cancel by client order ID |

### Market data

| Method | Path | Description |
|---|---|---|
| `GET` | `/v1/books/:market_id` | L2 depth snapshot (`?depth=N`, default 20) |
| `GET` | `/v1/trades/:market_id` | Recent fills (`?limit=N&from_seq=N`) |
| `GET` | `/v1/markets` | List all registered markets |

### Admin

Requires `X-Admin-Key` header matching the `ADMIN_KEY` environment variable.

| Method | Path | Description |
|---|---|---|
| `POST` | `/v1/admin/markets/:market_id/pause` | Halt order intake for a market |
| `POST` | `/v1/admin/markets/:market_id/resume` | Resume a paused market |
| `POST` | `/v1/admin/markets/:market_id/cancel-all` | Cancel all open orders in a market |

### WebSocket

```
GET /v1/ws
```

Streams all engine events (order accepted, fill, cancel) as JSON. Every state change is published in real time.

### Error format

```json
{ "code": "SNAKE_CASE_CODE", "message": "human readable description" }
```

---

## Submitting a signed order

Orders are self-authenticating. Generate an ed25519 keypair, construct a `SignedOrder`, compute a canonical SHA-256 hash over the fields, sign the hash, and submit. The engine verifies the signature on every intake — use the `clob` CLI or replicate the signing logic in any language.

```json
{
  "schema_version": 1,
  "client_order_id": "my-unique-id",
  "market_id": "BTC-USDC",
  "trader_id": "<64-char hex pubkey>",
  "side": "buy",
  "price_ticks": 49000000,
  "size_lots": 10,
  "time_in_force": "GTC",
  "nonce": 1,
  "expiry_ts_ms": 0,
  "created_at_ms": 1700000000000,
  "salt": 42,
  "trader_pubkey": "<32-byte pubkey as 64-char hex>",
  "signature": "<64-byte ed25519 signature as 128-char hex>"
}
```

`price_ticks` and `size_lots` are integers — no floating point in the matching path. For BTC-USDC with `tick_size=1000`, a price of $49,000 is `49000000`.

---

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `DATABASE_URL` | — | Postgres connection string. If unset, runs without persistence. |
| `ADMIN_KEY` | — | Secret for `X-Admin-Key` on admin endpoints. |
| `RUST_LOG` | `info` | Log level. |

---

## Design decisions

**Why event sourcing?**
The event log is the source of truth. The in-memory order book is a derived view rebuilt by replaying events on startup. This gives crash recovery, deterministic replay for debugging, and a complete audit trail with no extra work.

**Why a frozen canonical hash?**
The canonical byte layout of `SignedOrder` is fixed. Any field change invalidates the signature. The engine can verify order integrity without trusting the transport layer or the database — the signature is the proof.

**Why a single engine mutex?**
The matching core is intentionally single-threaded. Price-time priority requires a total ordering of all events — concurrent writes would require distributed consensus. The mutex is the simplest correct implementation. Vertical scaling (faster single core) is the right lever for this architecture.

**Why integer prices?**
Floating-point arithmetic is non-deterministic across platforms. Integer tick units give identical results everywhere — essential for a system where the same event log must replay to identical state on any machine.

---

## Project structure

```
src/
  api/          — axum HTTP handlers, WebSocket feed, shared state
  domain/       — Order, Fill, Market types, canonical hashing, ed25519 verification
  engine/       — OrderBook (BTreeMap + VecDeque), matching loop
  sequencer/    — Monotonic seq_id assignment, idempotency table, event log
  db/           — In-memory StateStore (nonces, fills, cancelled set)
  risk/         — Rate limits, order size checks, price band circuit breaker
  ops/          — Admin operations (pause, resume, cancel-all)
  replay/       — Rebuild engine state from an event log
  snapshot/     — Periodic snapshots for fast crash recovery
  pg/           — Postgres persistence (append events, load on startup)
  bin/cli.rs    — clob CLI binary
benches/
  engine.rs     — Criterion microbenchmarks
```
