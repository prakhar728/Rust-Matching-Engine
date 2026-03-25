use std::path::PathBuf;

use clob_api::api::{SharedState, rest::build_router, ws::build_ws_router};
use clob_api::domain::market::{MarketConfig, MarketId, MarketStatus};
use clob_api::engine::Engine;
use clob_api::pg::PgStore;
use clob_api::risk::{RiskChecker, RiskConfig};
use clob_api::snapshot::{create_snapshot, snapshot_path};

#[tokio::main]
async fn main() {
    // ── Connect to Postgres (optional — skipped if DATABASE_URL not set) ──
    let pg: Option<PgStore> = match std::env::var("DATABASE_URL") {
        Ok(url) => {
            let store = PgStore::connect(&url).await.expect("failed to connect to Postgres");
            store.migrate().await.expect("failed to run Postgres migrations");
            println!("[pg] connected to Postgres");
            Some(store)
        }
        Err(_) => {
            eprintln!("[pg] DATABASE_URL not set — running without persistence");
            None
        }
    };

    // ── Build engine and register markets ─────────────────────────────────
    let mut engine = Engine::new();

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

    engine.add_market(MarketConfig {
        id: MarketId("ETH-USDC".into()),
        base_asset: "ETH".into(),
        quote_asset: "USDC".into(),
        tick_size: 100,
        lot_size: 1,
        fee_bps_maker: 10,
        fee_bps_taker: 20,
        status: MarketStatus::Active,
    });

    // ── Restore engine from persisted event log ────────────────────────────
    if let Some(ref store) = pg {
        match store.load_all_events().await {
            Ok(events) if !events.is_empty() => {
                let count = events.len();
                engine
                    .restore_from_events(events)
                    .expect("failed to restore engine from event log");
                println!("[pg] restored engine from {count} events");
            }
            Ok(_) => println!("[pg] no events in Postgres — starting fresh"),
            Err(e) => eprintln!("[pg] failed to load events: {e}"),
        }
    }

    // ── Create shared state (engine + broadcast channel + optional pg) ─────
    let risk = RiskChecker::new(RiskConfig::default());
    let state = match pg {
        Some(store) => SharedState::with_pg(engine, risk, store),
        None => SharedState::with_risk(engine, risk),
    };

    // ── Periodic snapshot task — every 60 seconds ─────────────────────────
    let snapshot_dir = PathBuf::from("./snapshots");
    std::fs::create_dir_all(&snapshot_dir).expect("failed to create snapshots directory");

    let snapshot_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        interval.tick().await; // skip first immediate tick — nothing to snapshot yet
        loop {
            interval.tick().await;
            let (events, markets, checksum) = {
                let engine = snapshot_state.engine.lock().await;
                let events = engine.sequencer.log.clone();
                let markets =
                    engine.list_markets().into_iter().cloned().collect::<Vec<_>>();
                let checksum = engine.state.checksum();
                (events, markets, checksum)
            };
            if events.is_empty() {
                continue;
            }
            let seq_id = events.last().unwrap().seq_id;
            let path = snapshot_path(&snapshot_dir, seq_id);
            match create_snapshot(&path, &events, &markets, checksum) {
                Ok(_) => eprintln!("[snapshot] wrote snapshot_{seq_id}.json"),
                Err(e) => eprintln!("[snapshot] error writing snapshot: {e}"),
            }
        }
    });

    // ── Build router — REST + WebSocket routes merged ─────────────────────
    let app = build_router(state.clone()).merge(build_ws_router(state));

    // ── Start server ───────────────────────────────────────────────────────
    let addr = "0.0.0.0:8080";
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    println!("clob-api listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}
