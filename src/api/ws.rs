// WebSocket feed — real-time event streaming.
//
// Architecture:
//   The REST handlers publish events to a tokio::sync::broadcast channel after
//   every engine mutation. Each WS connection subscribes to that channel and
//   filters events by the channels the client has subscribed to.
//
// Channel naming (matches phase_-1.md §6):
//   "book.l3.{market_id}"   — L3 order-book updates (OrderAccepted, OrderCancelled)
//   "trades.{market_id}"    — Fill events for a market
//   "orders.{trader_id}"    — Private per-trader events (all types)
//   "market.{market_id}"    — Market status changes (paused / resumed)
//   "cancels"               — Order cancelled events (all markets, public)
//
// Client → Server message format:
//   { "action": "subscribe",   "channel": "book.l3.BTC-USDC" }
//   { "action": "subscribe",   "channel": "trades.BTC-USDC", "from_seq": 100 }
//   { "action": "unsubscribe", "channel": "book.l3.BTC-USDC" }
//   { "action": "ping" }
//
// Server → Client message format:
//   { "type": "subscribed",   "channel": "book.l3.BTC-USDC" }
//   { "type": "unsubscribed", "channel": "book.l3.BTC-USDC" }
//   { "type": "event",        "channel": "...", "sequence_id": 42,
//     "event_type": "order_accepted", "payload": {...}, "timestamp_ms": ... }
//   { "type": "error",        "code": "...", "message": "..." }
//   { "type": "pong" }
//   { "type": "disconnected", "reason": "lagged" }
//
// Reconnect with from_seq:
//   On subscribe with "from_seq": N, the handler replays all matching events
//   from the engine's in-memory sequencer log with seq_id >= N, then switches
//   to live broadcast. This lets clients recover missed events after a disconnect.
//
// Lag handling:
//   If a client's broadcast receiver falls BROADCAST_CAPACITY events behind,
//   tokio sends RecvError::Lagged. We send a "disconnected" message and close.

use std::collections::HashSet;
use std::sync::Arc;

use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::Response,
    routing::get,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::api::{AppState, WsEnvelope};

// ---------------------------------------------------------------------------
// Client → Server messages
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ClientMsg {
    Subscribe {
        channel: String,
        /// Optional: replay events with seq_id >= from_seq before going live.
        #[serde(default)]
        from_seq: u64,
    },
    Unsubscribe {
        channel: String,
    },
    Ping,
}

// ---------------------------------------------------------------------------
// Server → Client messages
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMsg<'a> {
    Subscribed { channel: &'a str },
    Unsubscribed { channel: &'a str },
    Event {
        channel: &'a str,
        sequence_id: u64,
        event_type: &'a str,
        payload: &'a serde_json::Value,
        timestamp_ms: u64,
    },
    Error { code: &'static str, message: String },
    Pong,
    Disconnected { reason: &'static str },
}

impl<'a> ServerMsg<'a> {
    fn to_text(&self) -> Option<Message> {
        serde_json::to_string(self).ok().map(Message::Text)
    }
}

// ---------------------------------------------------------------------------
// WebSocket upgrade handler
// ---------------------------------------------------------------------------

/// GET /v1/stream
///
/// Upgrades the connection to a WebSocket. The client then sends subscribe
/// messages and receives live event envelopes.
async fn ws_handler(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> Response {
    // Subscribe to the broadcast channel BEFORE upgrading the HTTP connection.
    // This ensures we don't miss any events that fire between upgrade and the
    // first recv() call inside the async task.
    let rx = state.events.subscribe();

    ws.on_upgrade(move |socket| handle_socket(socket, rx, state))
}

// ---------------------------------------------------------------------------
// Per-connection task
// ---------------------------------------------------------------------------

async fn handle_socket(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<Arc<WsEnvelope>>,
    state: AppState,
) {
    // Channels this connection is currently subscribed to.
    let mut subscriptions: HashSet<String> = HashSet::new();

    loop {
        tokio::select! {
            // ── Incoming message from the client ─────────────────────────
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_client_message(
                            &text,
                            &mut socket,
                            &mut subscriptions,
                            &state,
                        ).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        // Client closed gracefully or connection dropped.
                        break;
                    }
                    Some(Ok(_)) => {
                        // Binary / Ping / Pong frames — ignore.
                    }
                    Some(Err(_)) => {
                        // Read error — close.
                        break;
                    }
                }
            }

            // ── Incoming event from the broadcast channel ─────────────────
            result = rx.recv() => {
                match result {
                    Ok(envelope) => {
                        // Only forward if the client has subscribed to this channel.
                        if subscriptions.contains(&*envelope.channel) {
                            let msg = ServerMsg::Event {
                                channel: &envelope.channel,
                                sequence_id: envelope.sequence_id,
                                event_type: &envelope.event_type,
                                payload: &envelope.payload,
                                timestamp_ms: envelope.timestamp_ms,
                            };
                            if let Some(frame) = msg.to_text() {
                                if socket.send(frame).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Client fell too far behind — disconnect with reason.
                        let msg = ServerMsg::Disconnected { reason: "lagged" };
                        if let Some(frame) = msg.to_text() {
                            let _ = socket.send(frame).await;
                        }
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Broadcast channel shut down — this shouldn't happen
                        // while the engine is running, but handle it cleanly.
                        break;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Handle a single parsed client message
// ---------------------------------------------------------------------------

async fn handle_client_message(
    raw: &str,
    socket: &mut WebSocket,
    subscriptions: &mut HashSet<String>,
    state: &AppState,
) {
    let msg: ClientMsg = match serde_json::from_str(raw) {
        Ok(m) => m,
        Err(e) => {
            let reply = ServerMsg::Error {
                code: "PARSE_ERROR",
                message: format!("could not parse message: {e}"),
            };
            if let Some(frame) = reply.to_text() {
                let _ = socket.send(frame).await;
            }
            return;
        }
    };

    match msg {
        ClientMsg::Subscribe { channel, from_seq } => {
            // Replay historical events from the engine log before going live.
            // This happens synchronously in the handler before we add the
            // channel to subscriptions, so:
            //   1. Replay events with seq_id >= from_seq.
            //   2. Add channel to subscriptions → receive live events.
            // There's a small window where events between the last replayed
            // seq_id and the live stream could be duplicated — clients should
            // deduplicate by sequence_id if they set from_seq.
            if from_seq > 0 {
                replay_history(socket, state, &channel, from_seq).await;
            }

            subscriptions.insert(channel.clone());

            let reply = ServerMsg::Subscribed { channel: &channel };
            if let Some(frame) = reply.to_text() {
                let _ = socket.send(frame).await;
            }
        }

        ClientMsg::Unsubscribe { channel } => {
            subscriptions.remove(&channel);
            let reply = ServerMsg::Unsubscribed { channel: &channel };
            if let Some(frame) = reply.to_text() {
                let _ = socket.send(frame).await;
            }
        }

        ClientMsg::Ping => {
            if let Some(frame) = ServerMsg::Pong.to_text() {
                let _ = socket.send(frame).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Historical replay on subscribe
// ---------------------------------------------------------------------------

/// Send all events for `channel` with seq_id >= `from_seq` from the engine log.
///
/// Runs under the engine lock for a brief snapshot, then releases before
/// sending so we don't hold the lock across async socket I/O.
async fn replay_history(
    socket: &mut WebSocket,
    state: &AppState,
    channel: &str,
    from_seq: u64,
) {
    // Snapshot the relevant events while the lock is held.
    let envelopes: Vec<Arc<WsEnvelope>> = {
        let engine = state.engine.lock().await;
        engine
            .sequencer
            .log
            .iter()
            .filter(|se| se.seq_id >= from_seq)
            .flat_map(WsEnvelope::from_sequenced)
            .filter(|env| env.channel == channel)
            .map(Arc::new)
            .collect()
    };

    // Send outside the lock.
    for env in envelopes {
        let msg = ServerMsg::Event {
            channel: &env.channel,
            sequence_id: env.sequence_id,
            event_type: &env.event_type,
            payload: &env.payload,
            timestamp_ms: env.timestamp_ms,
        };
        if let Some(frame) = msg.to_text() {
            if socket.send(frame).await.is_err() {
                return; // Client disconnected mid-replay.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the WebSocket router.
///
/// Merge with `build_router` in main:
/// ```ignore
/// let app = build_router(state.clone()).merge(build_ws_router(state));
/// ```
pub fn build_ws_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/stream", get(ws_handler))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::SharedState;
    use crate::domain::market::{MarketConfig, MarketId, MarketStatus};
    use crate::engine::Engine;

    fn test_state() -> AppState {
        let mut engine = Engine::with_clock(
            Box::new(|| 1_000_000u64) as Box<dyn Fn() -> u64 + Send + Sync>,
        );
        engine.add_market(MarketConfig {
            id: MarketId("BTC-USDC".into()),
            base_asset: "BTC".into(),
            quote_asset: "USDC".into(),
            tick_size: 1_000,
            lot_size: 1,
            fee_bps_maker: 10,
            fee_bps_taker: 20,
            status: MarketStatus::Active,
        });
        SharedState::new(engine)
    }

    #[test]
    fn client_subscribe_parses() {
        let raw = r#"{"action":"subscribe","channel":"book.l3.BTC-USDC"}"#;
        let msg: ClientMsg = serde_json::from_str(raw).unwrap();
        assert!(matches!(msg, ClientMsg::Subscribe { channel, .. } if channel == "book.l3.BTC-USDC"));
    }

    #[test]
    fn client_subscribe_with_from_seq_parses() {
        let raw = r#"{"action":"subscribe","channel":"trades.BTC-USDC","from_seq":42}"#;
        let msg: ClientMsg = serde_json::from_str(raw).unwrap();
        assert!(
            matches!(msg, ClientMsg::Subscribe { channel, from_seq } if channel == "trades.BTC-USDC" && from_seq == 42)
        );
    }

    #[test]
    fn client_unsubscribe_parses() {
        let raw = r#"{"action":"unsubscribe","channel":"trades.BTC-USDC"}"#;
        let msg: ClientMsg = serde_json::from_str(raw).unwrap();
        assert!(matches!(msg, ClientMsg::Unsubscribe { channel } if channel == "trades.BTC-USDC"));
    }

    #[test]
    fn client_ping_parses() {
        let raw = r#"{"action":"ping"}"#;
        let msg: ClientMsg = serde_json::from_str(raw).unwrap();
        assert!(matches!(msg, ClientMsg::Ping));
    }

    #[test]
    fn server_subscribed_serializes() {
        let msg = ServerMsg::Subscribed { channel: "trades.BTC-USDC" };
        let json: serde_json::Value = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(json["type"], "subscribed");
        assert_eq!(json["channel"], "trades.BTC-USDC");
    }

    #[test]
    fn server_event_serializes() {
        let payload = serde_json::json!({"order_id": "abc"});
        let msg = ServerMsg::Event {
            channel: "book.l3.BTC-USDC",
            sequence_id: 7,
            event_type: "order_accepted",
            payload: &payload,
            timestamp_ms: 1_000_000,
        };
        let json: serde_json::Value = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(json["type"], "event");
        assert_eq!(json["sequence_id"], 7);
        assert_eq!(json["event_type"], "order_accepted");
    }

    #[test]
    fn server_pong_serializes() {
        let msg = ServerMsg::Pong;
        let json: serde_json::Value = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(json["type"], "pong");
    }

    #[test]
    fn ws_envelope_order_accepted_fans_out_to_two_channels() {
        use crate::events::{Event, SequencedEvent};
        use crate::domain::order::{Order, OrderHash, OrderId, OrderStatus, Side, TimeInForce};

        let order = Order {
            order_id: OrderId("o1".into()),
            client_order_id: "cid".into(),
            order_hash: OrderHash([0u8; 32]),
            market_id: MarketId("BTC-USDC".into()),
            trader_id: "trader-a".into(),
            trader_pubkey: [0u8; 32],
            side: Side::Buy,
            price_ticks: 100,
            orig_size_lots: 10,
            filled_size_lots: 0,
            time_in_force: TimeInForce::Gtc,
            nonce: 1,
            expiry_ts_ms: 0,
            status: OrderStatus::Open,
            created_sequence: 1,
            last_update_sequence: 1,
            accepted_at_ms: 1_000_000,
        };

        let se = SequencedEvent {
            seq_id: 1,
            timestamp_ms: 1_000_000,
            event: Event::OrderAccepted { order },
        };

        let envelopes = WsEnvelope::from_sequenced(&se);
        assert_eq!(envelopes.len(), 2);
        let channels: Vec<&str> = envelopes.iter().map(|e| e.channel.as_str()).collect();
        assert!(channels.contains(&"book.l3.BTC-USDC"));
        assert!(channels.contains(&"orders.trader-a"));
    }

    #[test]
    fn ws_envelope_fill_fans_out_to_three_channels() {
        use crate::domain::order::{Fill, OrderHash, OrderId};
        use crate::events::{Event, SequencedEvent};

        let fill = Fill {
            market_id: MarketId("BTC-USDC".into()),
            taker_order_id: OrderId("t1".into()),
            taker_order_hash: OrderHash([1u8; 32]),
            taker_trader_id: "trader-a".into(),
            maker_order_id: OrderId("m1".into()),
            maker_order_hash: OrderHash([2u8; 32]),
            maker_trader_id: "trader-b".into(),
            price_ticks: 100,
            size_lots: 5,
            sequence_id: 3,
            executed_at_ms: 1_000_003,
        };

        let se = SequencedEvent {
            seq_id: 3,
            timestamp_ms: 1_000_003,
            event: Event::Fill(fill),
        };

        let envelopes = WsEnvelope::from_sequenced(&se);
        assert_eq!(envelopes.len(), 3); // trades.BTC-USDC, orders.trader-a, orders.trader-b
        let channels: Vec<&str> = envelopes.iter().map(|e| e.channel.as_str()).collect();
        assert!(channels.contains(&"trades.BTC-USDC"));
        assert!(channels.contains(&"orders.trader-a"));
        assert!(channels.contains(&"orders.trader-b"));
    }

    #[test]
    fn ws_envelope_fill_same_trader_deduplicates() {
        use crate::domain::order::{Fill, OrderHash, OrderId};
        use crate::events::{Event, SequencedEvent};

        let fill = Fill {
            market_id: MarketId("BTC-USDC".into()),
            taker_order_id: OrderId("t1".into()),
            taker_order_hash: OrderHash([1u8; 32]),
            taker_trader_id: "same-trader".into(),
            maker_order_id: OrderId("m1".into()),
            maker_order_hash: OrderHash([2u8; 32]),
            maker_trader_id: "same-trader".into(), // same as taker
            price_ticks: 100,
            size_lots: 5,
            sequence_id: 3,
            executed_at_ms: 1_000_003,
        };

        let se = SequencedEvent {
            seq_id: 3,
            timestamp_ms: 1_000_003,
            event: Event::Fill(fill),
        };

        let envelopes = WsEnvelope::from_sequenced(&se);
        // trades.BTC-USDC + orders.same-trader (not doubled for taker+maker)
        assert_eq!(envelopes.len(), 2);
    }

    #[tokio::test]
    async fn broadcast_channel_delivers_event_to_subscriber() {
        let state = test_state();
        let mut rx = state.events.subscribe();

        // Simulate what a REST handler does after process_order
        let envelope = Arc::new(WsEnvelope {
            channel: "book.l3.BTC-USDC".into(),
            sequence_id: 1,
            event_type: "order_accepted".into(),
            payload: serde_json::json!({}),
            timestamp_ms: 1_000_000,
        });

        state.events.send(envelope.clone()).unwrap();

        let received = rx.recv().await.unwrap();
        assert_eq!(received.channel, "book.l3.BTC-USDC");
        assert_eq!(received.sequence_id, 1);
    }
}
