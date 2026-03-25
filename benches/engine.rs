// Engine microbenchmark — measures matching engine throughput with no HTTP overhead.
//
// Why no HTTP?
//   A local HTTP benchmark is misleading: the load generator and server compete
//   for the same CPU, and loopback has no real latency. Benchmarking the engine
//   directly gives an accurate, noise-free measure of matching throughput.
//
// Scenarios:
//   resting_orders  — N buy orders that never match (pure intake + sequencing)
//   match_1to1      — N/2 asks followed by N/2 crossing bids (1:1 full fills)
//   sweep           — N resting asks at different prices, one large sweeping bid
//
// All orders are pre-signed outside the timing loop so ed25519 signing cost
// is not included in the measurement.
//
// Run:
//   cargo bench
//   cargo bench -- --save-baseline before   (save a named baseline)
//   cargo bench -- --baseline before         (compare against it)

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use ed25519_dalek::{Signer, SigningKey};
use rand::rngs::OsRng;

use clob_api::domain::market::{MarketConfig, MarketId, MarketStatus};
use clob_api::domain::order::{Side, SignedOrder, TimeInForce, CURRENT_SCHEMA_VERSION};
use clob_api::engine::Engine;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn btc_usdc() -> MarketConfig {
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

/// Fresh engine with a fixed clock — no wall-clock calls in the hot path.
fn fresh_engine() -> Engine {
    let mut e = Engine::with_clock(
        Box::new(|| 1_000_000u64) as Box<dyn Fn() -> u64 + Send + Sync>,
    );
    e.add_market(btc_usdc());
    e
}

/// Sign a single order. Call this outside the timing loop.
fn sign_order(
    key: &SigningKey,
    side: Side,
    price_ticks: u64,
    size_lots: u64,
    nonce: u64,
    client_order_id: &str,
) -> SignedOrder {
    let pubkey = key.verifying_key().to_bytes();
    let mut o = SignedOrder {
        schema_version: CURRENT_SCHEMA_VERSION,
        client_order_id: client_order_id.to_string(),
        market_id: MarketId("BTC-USDC".into()),
        trader_id: hex::encode(pubkey),
        side,
        price_ticks,
        size_lots,
        time_in_force: TimeInForce::Gtc,
        nonce,
        expiry_ts_ms: 0,
        created_at_ms: 1_000_000,
        salt: nonce,
        trader_pubkey: pubkey,
        signature: [0u8; 64],
    };
    let hash = o.canonical_hash();
    o.signature = key.sign(&hash.0).to_bytes();
    o
}

// ---------------------------------------------------------------------------
// Scenario 1: resting_orders
//
// N buy orders at N distinct price levels — none of them match.
// Measures raw intake throughput: validation + sequencing + book insertion.
// ---------------------------------------------------------------------------

fn bench_resting_orders(c: &mut Criterion) {
    let key = SigningKey::generate(&mut OsRng);
    let mut group = c.benchmark_group("resting_orders");

    for &n in &[1_000u64, 10_000u64] {
        // Pre-sign all orders — signing cost excluded from timing.
        let orders: Vec<SignedOrder> = (1..=n)
            .map(|i| sign_order(&key, Side::Buy, i * 1_000, 1, i, &format!("c{i}")))
            .collect();

        group.throughput(Throughput::Elements(n));
        group.bench_with_input(BenchmarkId::from_parameter(n), &orders, |b, orders| {
            b.iter_batched(
                || (fresh_engine(), orders.clone()),
                |(mut engine, orders)| {
                    for order in orders {
                        engine.process_order(order).unwrap();
                    }
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Scenario 2: match_1to1
//
// N/2 asks placed first, then N/2 crossing bids. Each bid fully fills one ask.
// Measures matching throughput: price crossing, fill recording, state updates.
// ---------------------------------------------------------------------------

fn bench_match_1to1(c: &mut Criterion) {
    let maker = SigningKey::generate(&mut OsRng);
    let taker = SigningKey::generate(&mut OsRng);
    let mut group = c.benchmark_group("match_1to1");

    for &n in &[1_000u64, 10_000u64] {
        let half = n / 2;

        // N/2 resting asks, then N/2 crossing bids — pre-signed.
        let mut orders: Vec<SignedOrder> = Vec::with_capacity(n as usize);
        for i in 1..=half {
            orders.push(sign_order(&maker, Side::Sell, 50_000_000, 1, i, &format!("ask{i}")));
        }
        for i in 1..=half {
            orders.push(sign_order(&taker, Side::Buy, 50_000_000, 1, i, &format!("bid{i}")));
        }

        group.throughput(Throughput::Elements(n));
        group.bench_with_input(BenchmarkId::from_parameter(n), &orders, |b, orders| {
            b.iter_batched(
                || (fresh_engine(), orders.clone()),
                |(mut engine, orders)| {
                    for order in orders {
                        engine.process_order(order).unwrap();
                    }
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Scenario 3: sweep
//
// N resting asks at N distinct price levels (1 lot each), then one large bid
// that sweeps through all N levels at once. Stresses the inner matching loop.
// ---------------------------------------------------------------------------

fn bench_sweep(c: &mut Criterion) {
    let maker = SigningKey::generate(&mut OsRng);
    let taker = SigningKey::generate(&mut OsRng);
    let mut group = c.benchmark_group("sweep");

    for &n in &[100u64, 1_000u64] {
        let base = 50_000_000u64;

        // N asks at base, base+1_000, base+2_000, ...
        let mut orders: Vec<SignedOrder> = (1..=n)
            .map(|i| {
                sign_order(&maker, Side::Sell, base + (i - 1) * 1_000, 1, i, &format!("ask{i}"))
            })
            .collect();

        // One bid priced above the highest ask, size = N → sweeps all levels.
        orders.push(sign_order(&taker, Side::Buy, base + n * 1_000, n, 1, "sweep"));

        group.throughput(Throughput::Elements(n + 1));
        group.bench_with_input(BenchmarkId::from_parameter(n), &orders, |b, orders| {
            b.iter_batched(
                || (fresh_engine(), orders.clone()),
                |(mut engine, orders)| {
                    for order in orders {
                        engine.process_order(order).unwrap();
                    }
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_resting_orders, bench_match_1to1, bench_sweep);
criterion_main!(benches);
