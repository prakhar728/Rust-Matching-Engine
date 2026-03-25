# `src/pg/mod.rs` Flow

## Why this file exists

`pg/mod.rs` is the Postgres-backed event persistence layer.

It is not the live engine state. It is the durable event log used to rebuild in-memory state after restart.

## Block flow

```mermaid
flowchart TD
    A[Engine mutation succeeds] --> B[REST layer calls persist_since]
    B --> C[Shared state gathers new SequencedEvent values]
    C --> D[PgStore.append_events]
    D --> E[Insert events into sequenced_events table]
    F[Process restarts] --> G[main.rs connects PgStore]
    G --> H[PgStore.load_all_events]
    H --> I[Engine.restore_from_events]
    I --> J[Rebuild in-memory state]
```

## Type / function guide

### `PgStore`

What it does:

- wraps a Postgres connection pool

Why we need it:

- persistence concerns should stay separate from engine logic

### `connect(database_url)`

What it does:

- opens a Postgres pool

Why we need it:

- bootstrap needs a store object before migrations or reads/writes can happen

### `migrate()`

What it does:

- ensures the `sequenced_events` table exists

Why we need it:

- the service should be able to start cleanly without manual table creation first

### `append_events(events)`

Block flow:

```mermaid
flowchart TD
    A[Batch of new sequenced events] --> B[Begin SQL transaction]
    B --> C[Serialize each event to JSON]
    C --> D[Insert row into sequenced_events]
    D --> E[Ignore duplicates on seq_id conflict]
    E --> F[Commit transaction]
```

Why we need it:

- durable append-only persistence for crash recovery

### `load_all_events()`

Block flow:

```mermaid
flowchart TD
    A[Startup restore] --> B[Select all event payloads ordered by seq_id]
    B --> C[Deserialize JSON payloads into SequencedEvent]
    C --> D[Return ordered event list]
```

Why we need it:

- engine state is rebuilt by replay, not by reading live mutable rows

### `event_type_tag(event)`

What it does:

- derives a stable string tag for the event type

Why we need it:

- useful metadata for the persisted event rows and operational inspection
