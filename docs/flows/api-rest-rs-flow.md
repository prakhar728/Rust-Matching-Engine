# `src/api/rest.rs` Flow

## Why this file exists

`rest.rs` is the HTTP request/response layer.

It does not implement matching itself. It translates HTTP requests into engine/admin/risk calls and translates internal results back into HTTP responses.

## High-level block flow

```mermaid
flowchart TD
    A[HTTP request] --> B[Axum extractors parse state/path/query/json/headers]
    B --> C[Handler runs]
    C --> D[Optional risk check]
    D --> E[Lock engine]
    E --> F[Read or mutate engine]
    F --> G[Unlock engine]
    G --> H{Mutation?}
    H -- Yes --> I[Publish new events to WebSocket layer]
    I --> J[Persist new events to Postgres]
    H -- No --> K[Build JSON response]
    J --> K
    K --> L[Send HTTP response]
```

## Shared design pattern for mutating handlers

```mermaid
flowchart TD
    A[Acquire engine lock] --> B[Record seq_before]
    B --> C[Call engine/admin mutation]
    C --> D[Release engine lock]
    D --> E[publish_since seq_before]
    E --> F[persist_since seq_before]
    F --> G[Return JSON response]
```

Why this pattern exists:

- engine mutations must be serialized
- publishing and persistence should happen after mutation succeeds
- slow I/O should not hold the engine lock longer than necessary

## Function guide

### `ApiError`

What it does:

- standard JSON error payload with `{ code, message }`

Why we need it:

- clients should not have to guess error response shape

### `ErrorResponse`

What it does:

- pairs an HTTP status with `ApiError`

Why we need it:

- lets handlers return domain failures as proper HTTP responses

### `engine_err_to_response(...)`

What it does:

- converts engine/domain errors into HTTP statuses and stable API error codes

Why we need it:

- the engine should not know about HTTP

### `risk_err_to_response(...)`

What it does:

- maps risk-layer failures to API responses

Why we need it:

- pre-trade checks fail before order processing, but clients still need consistent HTTP semantics

### `admin_err_to_response(...)`

What it does:

- maps admin operation failures to API responses

Why we need it:

- admin flows have different failure conditions from normal order flow

### `post_order(...)`

Block flow:

```mermaid
flowchart TD
    A[POST /v1/orders] --> B[Parse SignedOrder]
    B --> C[Run pre-trade risk check]
    C --> D[Lock engine]
    D --> E[Record seq_before]
    E --> F[engine.process_order]
    F --> G[Unlock engine]
    G --> H{Fills happened?}
    H -- Yes --> I[Update risk reference price]
    H -- No --> J[Publish new events]
    I --> J
    J --> K[Persist new events]
    K --> L[Return 201 new or 200 duplicate]
```

Why we need it:

- this is the main order-entry endpoint

### `delete_order(...)`

Block flow:

```mermaid
flowchart TD
    A[DELETE /v1/orders/:order_id] --> B[Read X-Trader-Id header]
    B --> C[Lock engine]
    C --> D[Record seq_before]
    D --> E[engine.process_cancel]
    E --> F[Unlock engine]
    F --> G[Publish new events]
    G --> H[Persist new events]
    H --> I[Return cancel result]
```

Why we need it:

- cancel by server-assigned `order_id`

### `cancel_by_client_id(...)`

Block flow:

```mermaid
flowchart TD
    A[POST /v1/orders/cancel-by-client-id] --> B[Parse trader_id and client_order_id]
    B --> C[Lock engine]
    C --> D[Find order_id via sequencer idempotency record]
    D --> E[Cancel that order]
    E --> F[Unlock engine]
    F --> G[Publish new events]
    G --> H[Persist new events]
    H --> I[Return cancel result]
```

Why we need it:

- clients often track their own client order IDs more easily than server order IDs

### `get_order(...)`

What it does:

- returns current state of a single order

Why we need it:

- direct lookup by order ID

### `get_book(...)`

What it does:

- returns an L2 order book snapshot for a market
- `depth` means how many price levels per side to return

Why we need it:

- clients need a fast snapshot API, especially for initial UI/bootstrap state

### `get_trades(...)`

What it does:

- returns fills from the event log, filtered by market and optional `from_seq`

Why we need it:

- clients need historical trade lookup and replay support

### `list_markets(...)`

What it does:

- returns market metadata and status

Why we need it:

- clients need to discover market config such as tick size and lot size

### `admin_pause_market(...)`

What it does:

- pauses trading for a market

Why we need it:

- operational control

### `admin_resume_market(...)`

What it does:

- resumes trading for a market

Why we need it:

- operational control

### `admin_cancel_all(...)`

What it does:

- cancels all open orders in a market

Why we need it:

- operational control during incidents or halts

### `build_router(state)`

What it does:

- wires all REST endpoints into the Axum router

Why we need it:

- a single place to see and maintain the REST surface
