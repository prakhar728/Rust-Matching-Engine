# Phase -1: Web2-First CLOB Requirements (On-chain Compatible)

## 1) Product Goal
- What this Web2 orderbook should enable:
- Target users/integrators (DEXs, perps, MM firms, retail):
- Success criteria for this phase:

## 2) Scope (In)
- Limit orders
- Cancel order
- Partial/full fills
- Price-time priority
- Maker/taker fee model
- Batch matching support
- Deterministic replay from event log

## 3) Scope (Out)
- Margin engine
- Liquidations
- Advanced order types (stop, iceberg, post-only variants)
- Fraud proofs / dispute game

## 4) Canonical Order Schema (Freeze Early)
- Required fields:
  - `market_id`
  - `side` (bid/ask)
  - `price` (integer ticks)
  - `size` (integer lots)
  - `nonce`
  - `expiry_ts`
  - `trader_id` or `trader_pubkey`
  - `salt` (optional but recommended)
- Serialization format:
- Hash function:
- Signature algorithm:
- Versioning strategy:

## 5) Matching Rules
- Matching priority:
- Tie-breaker rule:
- Partial fill behavior:
- Self-trade policy:
- Tick size / lot size enforcement:
- Rejection rules:

## 6) API Surface (Web2)
- `POST /orders`
- `DELETE /orders/{order_id}`
- `GET /orders/{order_id}`
- `GET /book/{market_id}`
- `GET /trades/{market_id}`
- `WS /stream` (book + trade updates)

For each endpoint, define:
- Request schema:
- Validation rules:
- Response schema:
- Error codes:

## 7) Sequencer and Event Log
- Sequencer source of truth:
- Event types (`order_accepted`, `order_rejected`, `order_matched`, `order_canceled`):
- Event ordering guarantees:
- Idempotency guarantees:
- Replay procedure to rebuild full state:

## 8) Risk and Controls (Web2)
- Pre-trade checks:
- Rate limits / abuse controls:
- Market pause controls:
- Circuit breaker logic:
- Operator/admin permissions:

## 9) Data Model
- In-memory structures for live matching:
- Persistent storage tables/collections:
- Key indexes:
- Archival policy:

## 10) Observability and Ops
- Metrics (latency, reject rate, match throughput):
- Logs and tracing requirements:
- Alert conditions:
- Recovery runbook:

## 11) Security Requirements
- Signature verification policy (mandatory from day 1):
- Key rotation approach:
- Replay protection model:
- Audit trail requirements:

## 12) NEAR Migration Readiness (Future)
- Keep exact same order hash on-chain:
- Maintain `filled_amount[order_hash]` semantics:
- Maintain `cancelled[order_hash]` semantics:
- Keep deterministic settlement constraints:
- Gaps to close before on-chain settlement:

## 13) Milestones and Exit Criteria
- M1: Signed order intake working
- M2: Deterministic matching + replay passing
- M3: Stable API + WS feeds
- M4: Ops baseline and incident runbook
- Exit criteria for Phase -1:

## 14) Open Questions
- 
