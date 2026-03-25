pub mod rest;
pub mod ws;

use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

use crate::engine::Engine;
use crate::events::SequencedEvent;
use crate::pg::PgStore;
use crate::risk::{RiskChecker, RiskConfig};

// ---------------------------------------------------------------------------
// Shared application state
// ---------------------------------------------------------------------------

/// The broadcast channel capacity.
///
/// Slow WS consumers that fall more than BROADCAST_CAPACITY events behind will
/// receive a `Lagged` error and their connection will be closed. Increase this
/// if the engine produces bursts larger than this value.
pub const BROADCAST_CAPACITY: usize = 4_096;

/// The canonical event envelope broadcast to all WebSocket subscribers.
///
/// Each REST handler publishes one envelope per sequenced event generated
/// by the engine. WS connections filter by `channel` and forward matching
/// envelopes to the client.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WsEnvelope {
    /// Which logical stream this event belongs to.
    ///
    /// Format rules:
    ///   "book.l3.{market_id}"   — L3 order book updates (accepts, cancels)
    ///   "trades.{market_id}"    — fill events
    ///   "orders.{trader_id}"    — private per-trader events (all event types)
    pub channel: String,

    /// The global seq_id of the engine event that produced this envelope.
    pub sequence_id: u64,

    /// Snake-case event kind — mirrors the serde tag on `events::Event`.
    pub event_type: String,

    /// Full event payload serialized to JSON.
    pub payload: serde_json::Value,

    /// Wall-clock timestamp from the sequenced event (epoch ms).
    pub timestamp_ms: u64,
}

impl WsEnvelope {
    /// Build all envelopes for a single SequencedEvent.
    ///
    /// One event can fan out to multiple channels — e.g. a Fill is published
    /// to `trades.{market_id}`, `orders.{taker_id}`, and `orders.{maker_id}`.
    pub fn from_sequenced(se: &SequencedEvent) -> Vec<Self> {
        use crate::events::Event;

        let payload = match serde_json::to_value(&se.event) {
            Ok(v) => v,
            Err(_) => return vec![],
        };

        let ts = se.timestamp_ms;
        let sid = se.seq_id;

        match &se.event {
            Event::OrderAccepted { order } => {
                let market = order.market_id.0.clone();
                let trader = order.trader_id.clone();
                vec![
                    Self {
                        channel: format!("book.l3.{market}"),
                        sequence_id: sid,
                        event_type: "order_accepted".into(),
                        payload: payload.clone(),
                        timestamp_ms: ts,
                    },
                    Self {
                        channel: format!("orders.{trader}"),
                        sequence_id: sid,
                        event_type: "order_accepted".into(),
                        payload,
                        timestamp_ms: ts,
                    },
                ]
            }

            Event::Fill(fill) => {
                let market = fill.market_id.0.clone();
                let taker = fill.taker_trader_id.clone();
                let maker = fill.maker_trader_id.clone();
                // If taker == maker (shouldn't happen due to STP, but be defensive)
                // dedup by collecting into a set of channels.
                let mut envelopes = vec![Self {
                    channel: format!("trades.{market}"),
                    sequence_id: sid,
                    event_type: "fill".into(),
                    payload: payload.clone(),
                    timestamp_ms: ts,
                }];
                envelopes.push(Self {
                    channel: format!("orders.{taker}"),
                    sequence_id: sid,
                    event_type: "fill".into(),
                    payload: payload.clone(),
                    timestamp_ms: ts,
                });
                if maker != taker {
                    envelopes.push(Self {
                        channel: format!("orders.{maker}"),
                        sequence_id: sid,
                        event_type: "fill".into(),
                        payload,
                        timestamp_ms: ts,
                    });
                }
                envelopes
            }

            Event::OrderCancelled { order_id, .. } => {
                // We don't know the market_id or trader_id from the cancel event alone
                // (the engine doesn't embed them to keep the event compact).
                // Publish to a generic cancels channel; the private orders channel
                // won't receive this unless the client subscribes by order_id lookup.
                //
                // Phase 1+: embed market_id + trader_id in OrderCancelled event so
                // we can fan out to both book.l3 and orders channels here.
                vec![Self {
                    channel: "cancels".into(),
                    sequence_id: sid,
                    event_type: "order_cancelled".into(),
                    payload,
                    timestamp_ms: ts,
                }]
            }

            Event::OrderRejected { trader_id, .. } => {
                // Private channel only — rejections are not book events.
                vec![Self {
                    channel: format!("orders.{trader_id}"),
                    sequence_id: sid,
                    event_type: "order_rejected".into(),
                    payload,
                    timestamp_ms: ts,
                }]
            }

            Event::MarketPaused { market_id, .. } | Event::MarketResumed { market_id, .. } => {
                let et = match &se.event {
                    Event::MarketPaused { .. } => "market_paused",
                    _ => "market_resumed",
                };
                vec![Self {
                    channel: format!("market.{}", market_id.0),
                    sequence_id: sid,
                    event_type: et.into(),
                    payload,
                    timestamp_ms: ts,
                }]
            }
        }
    }
}

/// Combined application state shared by REST and WebSocket handlers.
pub struct SharedState {
    /// The matching engine, protected by an async mutex.
    pub engine: Mutex<Engine>,

    /// Pre-trade risk checker (rate limits, size limits, price band).
    /// Separate from the engine lock so risk checks don't block reads.
    pub risk: Mutex<RiskChecker>,

    /// Optional Postgres event store. None in tests and when DATABASE_URL is unset.
    pub pg: Option<Arc<PgStore>>,

    /// Broadcast sender — REST handlers publish here after every engine call.
    /// WS handlers subscribe to receive live events.
    pub events: broadcast::Sender<Arc<WsEnvelope>>,
}

/// The axum application state type alias.
pub type AppState = Arc<SharedState>;

impl SharedState {
    pub fn new(engine: Engine) -> AppState {
        Self::with_risk(engine, RiskChecker::new(RiskConfig::default()))
    }

    /// Construct with a custom RiskChecker (no Postgres — used in tests).
    pub fn with_risk(engine: Engine, risk: RiskChecker) -> AppState {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            engine: Mutex::new(engine),
            risk: Mutex::new(risk),
            pg: None,
            events: tx,
        })
    }

    /// Construct with both a custom RiskChecker and a PgStore.
    pub fn with_pg(engine: Engine, risk: RiskChecker, store: PgStore) -> AppState {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            engine: Mutex::new(engine),
            risk: Mutex::new(risk),
            pg: Some(Arc::new(store)),
            events: tx,
        })
    }

    /// Publish all envelopes for events appended to the sequencer log
    /// between `seq_before` (exclusive) and the current log tail.
    ///
    /// Called by REST handlers immediately after every engine mutation.
    /// Ignores send errors — lagged receivers are handled per-connection.
    pub fn publish_since(self: &Arc<Self>, seq_before: u64) {
        // We can't hold the engine lock here (callers already released it),
        // so we reacquire it briefly just to snapshot the new tail.
        // This is fine — the lock is uncontended at this point.
        let envelopes: Vec<Arc<WsEnvelope>> = {
            // try_lock: if it fails, the engine is still locked by someone else —
            // skip publishing for this cycle rather than deadlocking.
            let guard = match self.engine.try_lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            guard
                .sequencer
                .log
                .iter()
                .filter(|se| se.seq_id > seq_before)
                .flat_map(WsEnvelope::from_sequenced)
                .map(Arc::new)
                .collect()
        };

        for env in envelopes {
            // `send` fails only if there are no active receivers — that's fine.
            let _ = self.events.send(env);
        }
    }

    /// Persist all events appended since `seq_before` to Postgres.
    ///
    /// No-op if no PgStore is configured (tests, no DATABASE_URL).
    /// Logs errors to stderr rather than propagating — persistence failures
    /// should not cause API calls to fail (the in-memory state is still valid).
    pub async fn persist_since(self: &Arc<Self>, seq_before: u64) {
        let pg = match &self.pg {
            Some(pg) => pg.clone(),
            None => return,
        };

        let events: Vec<SequencedEvent> = {
            let guard = match self.engine.try_lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            guard
                .sequencer
                .log
                .iter()
                .filter(|se| se.seq_id > seq_before)
                .cloned()
                .collect()
        };

        if let Err(e) = pg.append_events(&events).await {
            eprintln!("[pg] error persisting {} events: {e}", events.len());
        }
    }
}
