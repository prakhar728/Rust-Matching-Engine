# CLOB Implementation Plan (NEAR)

## Objective
Build a reusable on-chain CLOB core that can be integrated by:
- Spot DEX products
- Perpetuals products

The core should stay product-agnostic; product-specific logic should live in adapters/hooks.

## Principles
- Deterministic matching and integer-only math
- Clear separation: matching engine vs settlement/risk
- Strict invariants and tests before feature expansion
- V1 optimized for correctness, not feature breadth
- Build fast off-chain first, but keep message formats on-chain compatible from day 1

## Hybrid Architecture (NEAR)
### Off-chain Components
- Order gateway API (submit/cancel/query signed orders)
- Matching engine (price-time priority)
- Sequencer/batcher (builds settlement batches)
- Market data service (orderbook stream, fills, trades)

### On-chain Components
- Custody vault for deposits/withdrawals (NEP-141 based)
- Signature and order hash verification
- Nonce/replay protection and expiry checks
- Fill accounting (`filled_amount[order_hash]`)
- Cancel state (`cancelled[order_hash]`)
- Settlement execution and fee distribution
- Event emission for external indexers

### Trust and Security Model
- Users sign orders; operator cannot forge user intent.
- Contract validates all critical constraints at settlement time.
- Off-chain matcher is untrusted for correctness; on-chain checks are authoritative.
- Operator downtime affects liveness, not custody safety.

### Failure and Recovery
- If operator is down, users can still cancel signed intents on-chain by hash.
- Settlement tx failures do not mutate state partially (single-call atomicity).
- New operators can be introduced without changing user funds model.

### V1 Hybrid Constraints
- Single operator/sequencer in V1 for simplicity.
- Batch settlement supported, but no fraud-proof/dispute game in V1.
- Focus on correctness and deterministic state transitions before decentralizing sequencing.

## Phase -1: Web2-First Foundation (On-chain Compatible)
### Steps
1. Build off-chain services:
- Order API (submit/cancel/query)
- Matching engine
- Sequencer and append-only execution log
2. Freeze canonical signed order format (do not change after launch):
- Include `market_id`, `side`, `price`, `size`, `nonce`, `expiry`, `trader_pubkey`
- Define canonical serialization and hash spec
3. Enforce signatures from day 1:
- No server-trusted order creation
- Every order accepted only if signature verifies
4. Implement deterministic replay:
- Given same input stream, engine must reproduce identical fills
5. Create migration-ready state model:
- `filled_amount[order_hash]`
- `cancelled[order_hash]`
- per-user balance ledger abstraction (off-chain first)

### Exit Criteria
- Engine is production-usable off-chain and can be verified/replayed deterministically.
- Signed order schema and hashing rules are frozen for future NEAR contract verification.

## Phase 0: Scope Lock (V1)
### Steps
1. Define V1 features:
- Limit order placement
- Cancel order
- Matching by price-time priority
- Basic maker/taker fees
2. Define V1 non-goals:
- No margin engine
- No liquidation engine
- No advanced orders (stop, iceberg, post-only variants)
3. Write success criteria in `/Users/prakharojha/Desktop/me/personal/CLOB/logs/THOUGHT_LOG.md`.

### Exit Criteria
- One-page V1 scope is written and frozen.

## Phase 1: Protocol Design
### Steps
1. Define shared domain types:
- `MarketId`, `OrderId`, `Side`, `Price`, `Qty`, `Timestamp`, `Fill`
2. Define core interfaces:
- `place_limit_order`
- `cancel_order`
- `get_order`
- `get_l2_book`
3. Define adapter interface (spot/perp):
- `validate_order(...)`
- `on_fill(...)`
- `on_cancel(...)`
4. Finalize data model and storage keys.
5. Define exact signature scheme and canonical order hashing spec used by both Web2 engine and NEAR contract.

### Exit Criteria
- Interface and data model documented with no open design ambiguity.

## Phase 2: Workspace & Contract Skeleton
### Steps
1. Create Rust workspace and contract crates:
- `contracts/clob-core`
- `contracts/market-factory` (optional in V1 if needed)
2. Add base contract structure:
- state structs
- init/config methods
- empty method stubs
3. Add basic build/test commands to project notes.

### Exit Criteria
- Contracts compile and test harness runs with placeholder tests.

## Phase 3: Core Orderbook State
### Steps
1. Implement market config validation:
- tick size
- lot size
- fee bounds
2. Implement order storage:
- per-market bids/asks
- user ownership mapping
- active/canceled/filled status
3. Implement read APIs:
- best bid/ask
- order by id
- level-2 snapshot

### Exit Criteria
- Unit tests prove ordering correctness and state consistency.

## Phase 4: Matching Engine
### Steps
1. Implement matching loop with price-time priority.
2. Support partial fills and residual resting orders.
3. Implement deterministic sequencing and event emission.
4. Implement cancel flow with ownership checks.

### Exit Criteria
- Matching behavior is deterministic across repeated test runs.

## Phase 5: Product Adapters
### Steps
1. Implement `SpotAdapter` contract/module:
- token settlement hooks
- transfer/accounting integration points
2. Implement `PerpAdapter` interface skeleton:
- position update hooks
- funding/margin hooks (stubbed in V1)
3. Ensure core depends only on adapter trait/interface, not concrete logic.

### Exit Criteria
- Same core engine can be called from spot and perp adapter paths.

## Phase 6: Safety, Limits, and Economics
### Steps
1. Add validation guards:
- size/price bounds
- paused market switch
- invalid config rejection
2. Add fee accounting and recipient payout logic.
3. Add storage/gas-aware constraints for NEAR.
4. Add explicit invariant checks in tests.

### Exit Criteria
- Adversarial tests pass for malformed input and edge cases.

## Phase 7: Integration, Docs, and Testnet
### Steps
1. Add integration tests:
- place -> match -> settle -> cancel
2. Write operator docs:
- deploy flow
- market creation flow
- upgrade considerations
3. Publish first release entry in `/Users/prakharojha/Desktop/me/personal/CLOB/logs/RELEASE_LOG.md`.
4. Run testnet dry-run with one spot-like and one perp-like market config.

### Exit Criteria
- Testnet checklist complete and release notes recorded.

## Working Rhythm
1. Start every session with a short goal entry in `/Users/prakharojha/Desktop/me/personal/CLOB/logs/BUILD_LOG.md`.
2. Log key design decisions in `/Users/prakharojha/Desktop/me/personal/CLOB/logs/THOUGHT_LOG.md`.
3. Update `/Users/prakharojha/Desktop/me/personal/CLOB/logs/RELEASE_LOG.md` only when milestone-level changes are complete.

## Immediate Next Actions
1. Execute Phase -1 first (Web2 engine with signed orders and deterministic replay).
2. Finalize Phase 0 scope in writing.
3. Draft exact interfaces and hashing/signature spec for Phase 1.
4. Start Phase 2 scaffolding only after interface sign-off.
