# `src/main.rs` Flow

## Why this file exists

`main.rs` is the process bootstrapper. It wires together persistence, engine state, risk, routers, snapshots, and the HTTP server.

## Block Flow

```mermaid
flowchart TD
    A[Process starts] --> B[Optionally connect to Postgres]
    B --> C[Run Postgres migration]
    C --> D[Create Engine]
    D --> E[Register markets]
    E --> F{Postgres configured?}
    F -- Yes --> G[Load persisted events]
    G --> H[Replay events into Engine]
    F -- No --> I[Skip restore]
    H --> J[Create RiskChecker]
    I --> J
    J --> K[Create SharedState]
    K --> L[Start periodic snapshot task]
    L --> M[Build REST router]
    M --> N[Build WS router]
    N --> O[Merge routers]
    O --> P[Bind TCP listener]
    P --> Q[Serve Axum app]
```

## Function-by-function

### `main()`

What it does:

- optionally opens Postgres via `PgStore`
- creates and seeds the matching engine
- restores in-memory engine state from persisted events
- creates shared app state used by REST and WebSocket layers
- starts periodic snapshots
- builds the Axum app and starts listening on `0.0.0.0:8080`

Why we need it:

- every other file defines pieces of behavior
- `main()` is where those pieces are assembled into a running service

## Important decisions in this file

- Postgres is optional: no `DATABASE_URL` means the app still runs
- live state is rebuilt into memory on startup
- markets are hardcoded during bootstrap in the current phase
- snapshots are background maintenance, not part of request handling
