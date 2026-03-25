# `src/api/ws.rs` Notes

These notes are meant to track both what the file does and the questions asked while reading it.

## What this file is

[`src/api/ws.rs`](/Users/prakharojha/Desktop/me/personal/CLOB/src/api/ws.rs) implements the WebSocket side of the API.

Main job:

- accept a WebSocket connection at `GET /v1/stream`
- let clients subscribe to channels
- receive live events from the app broadcast channel
- forward matching events to the connected client
- support replay from a given `sequence_id` with `from_seq`

## Main design choices

- One long-lived async task per WebSocket connection
- Shared pub/sub fanout using `tokio::sync::broadcast`
- Per-connection subscription filtering using a `HashSet<String>`
- Historical replay first, then live updates
- Sequence IDs used for ordering, replay, and client-side dedupe
- Slow consumers are disconnected if they lag too far behind

## Imports explained

### `use std::sync::Arc;`

`Arc` means atomically reference-counted shared ownership.

Why used here:

- `AppState` is shared across many async tasks
- `WsEnvelope` values are shared through the broadcast channel without copying the whole payload repeatedly

`Arc` does not make mutation safe by itself. Shared mutable state is protected separately with `Mutex` in `src/api/mod.rs`.

### `use axum::...`

Axum is the web framework.

In this file it provides:

- `Router`
- `State`
- `WebSocketUpgrade`
- `WebSocket`
- `Message`
- route wiring with `get`

Axum handles HTTP/WebSocket framework behavior. It runs on Tokio.

### `use tokio::sync::broadcast;`

This is a one-to-many channel.

Why used here:

- REST handlers publish events once
- every WebSocket connection subscribes with its own receiver
- each socket loop decides whether a given event should be forwarded to that client

This is the pub/sub mechanism for live updates.

### `use crate::api::{AppState, WsEnvelope};`

Use `crate::...` when importing from this crate's root.

Why not `api::...`:

- `crate::api::...` is the clear absolute path from the crate root
- `api::...` is not the normal crate-root import style inside Rust modules
- from this file, `super::...` would also work because `ws.rs` is inside the `api` module

## Shared state and mutation

Question: if many tasks share the same state, what happens when two things modify it at the same time?

Answer:

- `Arc` gives shared ownership
- `Mutex` controls mutation

In this codebase:

- many tasks can hold `AppState`
- only one task can hold `engine.lock().await` at a time

So if two tasks try to change the engine:

1. one acquires the mutex
2. the other waits
3. once the first releases the lock, the second proceeds

Important distinction:

- `Arc` = shared ownership
- `Mutex` = exclusive mutable access

## WebSocket handler

```rust
async fn ws_handler(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> Response {
    let rx = state.events.subscribe();
    ws.on_upgrade(move |socket| handle_socket(socket, rx, state))
}
```

What it does:

- handles `GET /v1/stream`
- creates a fresh broadcast receiver for this connection
- upgrades the HTTP request into a WebSocket
- hands the actual socket work to `handle_socket`

Why subscribe before upgrade:

- to avoid missing events published during the short gap between connection setup and the async socket task starting

## Who is the "client"?

Client does not necessarily mean trader.

- client = technical connection endpoint such as browser, bot, backend service, mobile app
- trader = business identity involved in orders

One trader may have multiple clients open at once.

## `tokio::select!`

Inside `handle_socket`, `tokio::select!` waits for whichever async event becomes ready first.

In this file the two main sources are:

- `socket.recv()` for incoming client messages
- `rx.recv()` for incoming broadcast events

This is not busy polling. The task sleeps until Tokio wakes it.

How Tokio knows something happened:

- the socket future registers interest in incoming network data
- the broadcast receiver registers interest in published messages
- Tokio wakes the task when one becomes ready

## Infinite loop behavior

`handle_socket` uses an intentional indefinite loop.

That loop lasts for the lifetime of one WebSocket connection.

There is one such loop per connection, not one global loop for all clients.

If 100 clients connect, there will be 100 connection tasks, each with:

- its own socket
- its own broadcast receiver
- its own subscription set

## Why this instead of polling?

The loop is indefinite, but it is not constantly using CPU.

It waits on async operations:

- socket input
- broadcast events

So it is event-driven waiting, not active polling.

## `ClientMsg::Subscribe { channel, from_seq }`

This branch supports recovery after disconnects.

If a client reconnects and passes `from_seq`, the server:

1. replays historical events with `seq_id >= from_seq`
2. then adds the channel to live subscriptions

Why duplicates are possible:

- there is a small boundary window between replay and live delivery
- an event near that edge may be seen in both replay and live stream

That is why clients should deduplicate using `sequence_id`.

## `subscriptions.contains(&*envelope.channel)`

This expression is an awkward way to turn `String` into `&str`.

Meaning:

- `envelope.channel` is a `String`
- `*` dereferences to the underlying string slice
- `&` borrows it again as `&str`

Clearer equivalent:

```rust
subscriptions.contains(envelope.channel.as_str())
```

## Where `frame` comes from

This code:

```rust
if let Some(frame) = msg.to_text() {
    if socket.send(frame).await.is_err() {
        break;
    }
}
```

`msg` is a Rust enum value of type `ServerMsg`.

`to_text()`:

- serializes that enum into JSON
- wraps it as a WebSocket `Message::Text(...)`

The local variable `frame` is just the outgoing WebSocket message payload chosen by the author as a variable name.

## Serde usage in this file

Example:

```rust
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMsg<'a> { ... }
```

### Why `Serialize`?

It lets Serde convert the enum into JSON for WebSocket responses.

### Why `#[serde(tag = "type")]`?

It adds a discriminator field so clients can tell which variant they received.

Examples:

- `Pong` -> `{ "type": "pong" }`
- `Subscribed { channel: ... }` -> `{ "type": "subscribed", "channel": ... }`

### Why `rename_all = "snake_case"`?

It keeps the JSON naming convention consistent automatically.

Examples:

- `Subscribed` -> `subscribed`
- `Disconnected` -> `disconnected`

This avoids having to manually rename each variant.

## `ServerMsg<'a>` and lifetimes

The `<'a>` is a lifetime parameter.

It means the enum can borrow data instead of owning everything.

For example:

- `channel: &'a str`
- `event_type: &'a str`
- `payload: &'a serde_json::Value`

This avoids unnecessary cloning while serializing messages.

## `impl<'a> ServerMsg<'a> { ... }`

```rust
impl<'a> ServerMsg<'a> {
    fn to_text(&self) -> Option<Message> {
        serde_json::to_string(self).ok().map(Message::Text)
    }
}
```

Meaning:

- for any lifetime `'a`, add methods on `ServerMsg<'a>`

What `to_text` does:

1. serialize `self` to a JSON string
2. convert `Result` to `Option`
3. if successful, wrap the string in `Message::Text`

Expanded form:

```rust
fn to_text(&self) -> Option<Message> {
    match serde_json::to_string(self) {
        Ok(json) => Some(Message::Text(json)),
        Err(_) => None,
    }
}
```

## Why this shape instead of Node.js style callbacks?

This problem exists in JS/TS too.

Difference:

- Rust/Tokio makes the async event loop explicit with `select!`, channels, and tasks
- Node.js usually expresses the same logic with event listeners and the built-in event loop

So the architecture is not unusual. It is just more explicit than JS.

## Standard practice

For async Rust WebSocket servers, these are standard and reasonable choices:

- one task per connection
- a shared pub/sub source for fanout
- `tokio::select!` for socket input plus internal events
- short lock scope
- sequence-based replay on reconnect

## Open architectural considerations

Things worth thinking about later:

- WebSocket auth for private channels such as `orders.{trader_id}`
- Retention strategy for replay if in-memory logs grow or the process restarts
- Client backpressure policy and reconnect behavior
- Whether cancel events need richer payloads for better channel routing
- Snapshot + stream client bootstrap correctness
