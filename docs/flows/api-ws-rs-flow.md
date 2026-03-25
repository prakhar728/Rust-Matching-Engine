# `src/api/ws.rs` Flow

## Why this file exists

`ws.rs` is the realtime streaming side of the API.

It upgrades HTTP connections to WebSockets, lets each client subscribe to channels, and forwards matching events from the shared broadcast stream.

## High-level block flow

```mermaid
flowchart TD
    A[Client connects to GET /v1/stream] --> B[ws_handler]
    B --> C[Subscribe to shared broadcast channel]
    C --> D[Upgrade HTTP connection to WebSocket]
    D --> E[Start handle_socket for this connection]
    E --> F[Loop for this connection]
    F --> G[Receive client messages]
    F --> H[Receive internal broadcast events]
    H --> I[Forward matching events to client]
    G --> F
    I --> F
    F --> J[Stop on disconnect or lag or send failure]
```

## Function guide

### `ClientMsg`

What it does:

- describes messages the client can send:
  - subscribe
  - unsubscribe
  - ping

Why we need it:

- the server needs a typed contract for incoming WebSocket control messages

### `ServerMsg`

What it does:

- describes messages the server sends:
  - subscribed
  - unsubscribed
  - event
  - error
  - pong
  - disconnected

Why we need it:

- the server needs one consistent outbound message protocol

### `ServerMsg::to_text()`

What it does:

- serializes a server message to JSON and wraps it as a WebSocket text frame

Why we need it:

- sockets send `Message`, not arbitrary Rust enums

### `ws_handler(...)`

Block flow:

```mermaid
flowchart TD
    A[GET /v1/stream] --> B[Extract AppState]
    B --> C[Create fresh broadcast receiver]
    C --> D[Register on_upgrade callback]
    D --> E[Call handle_socket socket rx state]
```

Why we need it:

- this is the bridge from HTTP upgrade request into a long-lived WebSocket task

### `handle_socket(...)`

Block flow:

```mermaid
flowchart TD
    A[New WebSocket connection] --> B[Create empty subscription set]
    B --> C[Loop]
    C --> D[tokio::select on socket.recv and rx.recv]
    D --> E[Client message arrives]
    D --> F[Broadcast event arrives]
    E --> G[Parse and handle message]
    F --> H[Forward only if channel is subscribed]
    G --> C
    H --> C
    C --> I[Break on close or error or lag or send failure]
```

Why we need it:

- each client needs an independent long-lived event loop

### `handle_client_message(...)`

Block flow:

```mermaid
flowchart TD
    A[Raw text message] --> B[Parse JSON into ClientMsg]
    B --> C{Parse ok?}
    C -- No --> D[Send parse error]
    C -- Yes --> E{Message type}
    E -- Subscribe --> F[Optionally replay history from from_seq]
    F --> G[Add channel to subscription set]
    G --> H[Send subscribed ack]
    E -- Unsubscribe --> I[Remove channel]
    I --> J[Send unsubscribed ack]
    E -- Ping --> K[Send pong]
```

Why we need it:

- keeps connection control logic separate from the top-level select loop

### `replay_history(...)`

Block flow:

```mermaid
flowchart TD
    A[Subscribe with from_seq] --> B[Lock engine briefly]
    B --> C[Snapshot matching sequencer log events]
    C --> D[Unlock engine]
    D --> E[Send replayed events over socket]
```

Why we need it:

- clients need a recovery path after disconnects

### `build_ws_router(state)`

What it does:

- registers the WebSocket route on the Axum router

Why we need it:

- keeps WS route wiring separate from REST route wiring

## Important design decisions

- one socket loop per connection
- pub/sub fanout via `broadcast`
- local per-connection subscription filtering
- replay then live stream on reconnect
- duplicates are tolerated at replay/live boundary and must be deduped by `sequence_id`
- slow clients are dropped instead of buffering forever
