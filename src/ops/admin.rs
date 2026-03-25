// Administrative operations — market lifecycle and circuit breaker controls.
//
// These are privileged mutations that bypass normal order-intake validation.
// Phase -1 exposes them via a separate admin API surface (not yet wired to
// REST — that's Phase 1). For now they are pure engine mutations callable
// from internal tooling or tests.
//
// Operations:
//   pause_market(market_id)   — stop accepting new orders; preserve the book
//   resume_market(market_id)  — reopen intake after a pause
//   cancel_all_orders(market) — forcibly cancel every resting order in a book
//                               (used before settling or emergency shutdown)
//
// All operations sequence an event into the engine log so they are visible
// in replay and the WS broadcast feed.

use thiserror::Error;

use crate::domain::market::MarketStatus;
use crate::engine::Engine;
use crate::events::{CancelReason, Event};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum AdminError {
    #[error("market '{0}' not found")]
    MarketNotFound(String),

    #[error("market '{0}' is already {1:?}")]
    AlreadyInState(String, MarketStatus),

    #[error("market '{0}' is settled — cannot be modified")]
    Settled(String),
}

// ---------------------------------------------------------------------------
// Admin operations
// ---------------------------------------------------------------------------

/// Pause a market: stop accepting new orders.
///
/// The resting book is preserved — existing orders remain queued.
/// Returns the seq_id of the MarketPaused event.
pub fn pause_market(
    engine: &mut Engine,
    market_id: &str,
    triggered_by: &str,
) -> Result<u64, AdminError> {
    // Validate current state.
    let market = engine
        .list_markets()
        .into_iter()
        .find(|m| m.id.0 == market_id)
        .ok_or_else(|| AdminError::MarketNotFound(market_id.to_string()))?
        .clone();

    match market.status {
        MarketStatus::Paused => {
            return Err(AdminError::AlreadyInState(
                market_id.to_string(),
                MarketStatus::Paused,
            ))
        }
        MarketStatus::Settled => return Err(AdminError::Settled(market_id.to_string())),
        MarketStatus::Active => {}
    }

    // Update market status.
    let mut updated = market;
    updated.status = MarketStatus::Paused;
    engine.add_market(updated);

    // Sequence the event.
    let seq_id = engine.sequencer.peek_next_seq_id();
    engine.sequencer.append_event(Event::MarketPaused {
        market_id: crate::domain::market::MarketId(market_id.to_string()),
        triggered_by: triggered_by.to_string(),
    });

    Ok(seq_id)
}

/// Resume a paused market: reopen intake.
///
/// Returns the seq_id of the MarketResumed event.
pub fn resume_market(
    engine: &mut Engine,
    market_id: &str,
    triggered_by: &str,
) -> Result<u64, AdminError> {
    let market = engine
        .list_markets()
        .into_iter()
        .find(|m| m.id.0 == market_id)
        .ok_or_else(|| AdminError::MarketNotFound(market_id.to_string()))?
        .clone();

    match market.status {
        MarketStatus::Active => {
            return Err(AdminError::AlreadyInState(
                market_id.to_string(),
                MarketStatus::Active,
            ))
        }
        MarketStatus::Settled => return Err(AdminError::Settled(market_id.to_string())),
        MarketStatus::Paused => {}
    }

    let mut updated = market;
    updated.status = MarketStatus::Active;
    engine.add_market(updated);

    let seq_id = engine.sequencer.peek_next_seq_id();
    engine.sequencer.append_event(Event::MarketResumed {
        market_id: crate::domain::market::MarketId(market_id.to_string()),
        triggered_by: triggered_by.to_string(),
    });

    Ok(seq_id)
}

/// Cancel every resting order in a market (admin force cancel).
///
/// Used before settling a market or in an emergency. Each cancelled order
/// produces an `OrderCancelled { reason: AdminForce }` event in the log.
///
/// Returns the number of orders cancelled.
pub fn cancel_all_orders(engine: &mut Engine, market_id: &str) -> Result<usize, AdminError> {
    // Verify the market exists.
    engine
        .list_markets()
        .into_iter()
        .find(|m| m.id.0 == market_id)
        .ok_or_else(|| AdminError::MarketNotFound(market_id.to_string()))?;

    // Snapshot all open order IDs (we need to release the borrow before mutating).
    let open_orders: Vec<(crate::domain::order::OrderId, crate::domain::order::OrderHash)> = {
        let snapshot = engine.l2_snapshot(market_id, usize::MAX);
        // l2_snapshot gives price levels; we need to iterate orders directly.
        // We use get_fills as a proxy check (market exists), then use the
        // sequencer log to find all open orders for this market.
        //
        // The engine doesn't directly expose "list all open orders for market".
        // We collect them from the sequencer log: every OrderAccepted event that
        // hasn't been closed by a subsequent Fill/Cancel.
        //
        // Phase 1+: add Engine::open_orders(market_id) for direct access.
        drop(snapshot);

        engine
            .sequencer
            .log
            .iter()
            .filter_map(|se| {
                if let crate::events::Event::OrderAccepted { order } = &se.event {
                    if order.market_id.0 == market_id && !order.is_closed() {
                        return Some((order.order_id.clone(), order.order_hash));
                    }
                }
                None
            })
            .collect()
    };

    let count = open_orders.len();
    for (order_id, order_hash) in open_orders {
        // Update state store.
        engine.state.cancel_order(order_hash.0);

        // Sequence the cancel event.
        engine.sequencer.append_event(Event::OrderCancelled {
            order_id,
            order_hash,
            reason: CancelReason::AdminForce,
        });
    }

    Ok(count)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::{MarketConfig, MarketId, MarketStatus};
    use crate::domain::order::{Side, TimeInForce, CURRENT_SCHEMA_VERSION};
    use crate::engine::Engine;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    fn btc_market() -> MarketConfig {
        MarketConfig {
            id: MarketId("BTC-USDC".into()),
            base_asset: "BTC".into(),
            quote_asset: "USDC".into(),
            tick_size: 1_000,
            lot_size: 1,
            fee_bps_maker: 10,
            fee_bps_taker: 20,
            status: MarketStatus::Active,
        }
    }

    fn test_engine() -> Engine {
        let mut e = Engine::with_clock(
            Box::new(|| 1_000_000u64) as Box<dyn Fn() -> u64 + Send + Sync>,
        );
        e.add_market(btc_market());
        e
    }

    fn place_order(engine: &mut Engine, key: &SigningKey, nonce: u64) {
        let pubkey = key.verifying_key().to_bytes();
        let mut signed = crate::domain::order::SignedOrder {
            schema_version: CURRENT_SCHEMA_VERSION,
            client_order_id: format!("cid-{nonce}"),
            market_id: MarketId("BTC-USDC".into()),
            trader_id: hex::encode(pubkey),
            side: Side::Buy,
            price_ticks: 50_000_000,
            size_lots: 1,
            time_in_force: TimeInForce::Gtc,
            nonce,
            expiry_ts_ms: 0,
            created_at_ms: 1_000_000,
            salt: 0,
            trader_pubkey: pubkey,
            signature: [0u8; 64],
        };
        let hash = signed.canonical_hash();
        signed.signature = key.sign(&hash.0).to_bytes();
        engine.process_order(signed).unwrap();
    }

    #[test]
    fn pause_active_market_succeeds() {
        let mut engine = test_engine();
        let seq = pause_market(&mut engine, "BTC-USDC", "ops").unwrap();
        assert!(seq > 0);
        let markets = engine.list_markets();
        assert_eq!(markets[0].status, MarketStatus::Paused);
    }

    #[test]
    fn pause_then_resume_restores_active() {
        let mut engine = test_engine();
        pause_market(&mut engine, "BTC-USDC", "ops").unwrap();
        resume_market(&mut engine, "BTC-USDC", "ops").unwrap();
        let markets = engine.list_markets();
        assert_eq!(markets[0].status, MarketStatus::Active);
    }

    #[test]
    fn pause_already_paused_returns_error() {
        let mut engine = test_engine();
        pause_market(&mut engine, "BTC-USDC", "ops").unwrap();
        let err = pause_market(&mut engine, "BTC-USDC", "ops").unwrap_err();
        assert!(matches!(err, AdminError::AlreadyInState(_, MarketStatus::Paused)));
    }

    #[test]
    fn resume_already_active_returns_error() {
        let mut engine = test_engine();
        let err = resume_market(&mut engine, "BTC-USDC", "ops").unwrap_err();
        assert!(matches!(err, AdminError::AlreadyInState(_, MarketStatus::Active)));
    }

    #[test]
    fn unknown_market_returns_not_found() {
        let mut engine = test_engine();
        assert!(matches!(
            pause_market(&mut engine, "DOESNT-EXIST", "ops").unwrap_err(),
            AdminError::MarketNotFound(_)
        ));
    }

    #[test]
    fn pause_sequences_market_paused_event() {
        let mut engine = test_engine();
        let seq = pause_market(&mut engine, "BTC-USDC", "ops-user").unwrap();
        let last_event = &engine.sequencer.log.last().unwrap().event;
        assert!(matches!(
            last_event,
            crate::events::Event::MarketPaused { triggered_by, .. }
            if triggered_by == "ops-user"
        ));
        assert_eq!(engine.sequencer.log.last().unwrap().seq_id, seq);
    }

    #[test]
    fn resume_sequences_market_resumed_event() {
        let mut engine = test_engine();
        pause_market(&mut engine, "BTC-USDC", "ops").unwrap();
        let seq = resume_market(&mut engine, "BTC-USDC", "ops").unwrap();
        let last_event = &engine.sequencer.log.last().unwrap().event;
        assert!(matches!(last_event, crate::events::Event::MarketResumed { .. }));
        assert_eq!(engine.sequencer.log.last().unwrap().seq_id, seq);
    }

    #[test]
    fn cancel_all_orders_cancels_open_orders() {
        let mut engine = test_engine();
        let key1 = SigningKey::generate(&mut OsRng);
        let key2 = SigningKey::generate(&mut OsRng);
        place_order(&mut engine, &key1, 1);
        place_order(&mut engine, &key2, 1);

        let count = cancel_all_orders(&mut engine, "BTC-USDC").unwrap();
        assert_eq!(count, 2);

        // Both orders should be cancelled in the state store.
        let cancel_count = engine
            .sequencer
            .log
            .iter()
            .filter(|se| {
                matches!(
                    &se.event,
                    crate::events::Event::OrderCancelled {
                        reason: CancelReason::AdminForce,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(cancel_count, 2);
    }

    #[test]
    fn cancel_all_unknown_market_returns_error() {
        let mut engine = test_engine();
        assert!(matches!(
            cancel_all_orders(&mut engine, "NOPE").unwrap_err(),
            AdminError::MarketNotFound(_)
        ));
    }
}
