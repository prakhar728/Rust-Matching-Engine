# Engine Microbench Results

Benchmarks the matching engine directly with no HTTP overhead.
Run with `cargo bench` using the Criterion framework (`benches/engine.rs`).

## Environment

| Property | Value |
|---|---|
| Date | 2026-03-13 |
| Machine | MacBook Pro (local) |
| OS | macOS Darwin 25.3.0 |
| Profile | `release` (Criterion always uses `--release`) |
| Rust | stable |
| Clock | Fixed (`|| 1_000_000u64`) — no syscall noise |
| Signing | Pre-generated outside timing loop — ed25519 cost excluded |

## Results

### resting_orders — N buy orders, no matches

All buys at distinct price levels. Measures pure order intake: validation + sequencing + book insertion.

| N | Time (median) | Throughput |
|---|---|---|
| 1,000 | 23.43 ms | **42,676 orders/sec** |
| 10,000 | 236.43 ms | **42,296 orders/sec** |

**Observation:** Throughput is flat across 1k and 10k — the engine scales linearly. No degradation from book growth.

---

### match_1to1 — N/2 asks + N/2 crossing bids

N/2 resting asks placed first, then N/2 bids at the same price. Each bid fully fills one ask.
Measures matching throughput: price crossing, fill recording, StateStore updates.

| N | Time (median) | Throughput |
|---|---|---|
| 1,000 | 26.33 ms | **37,973 orders/sec** |
| 10,000 | 499.72 ms | **20,011 orders/sec** |

**Observation:** Throughput halves from 1k to 10k. Likely cause: `HashMap` allocation growth causing cache pressure as the order/fill maps expand. Worth profiling before the HTTP load test phase.

---

### sweep — N resting asks at distinct prices, one large sweeping bid

N asks placed at N different price levels (1 lot each), then one bid priced above all asks with size = N.
The single bid walks through all N price levels. Stresses the inner matching loop.

| N | Time (median) | Throughput |
|---|---|---|
| 100 | 2.35 ms | **42,970 orders/sec** |
| 1,000 | 23.77 ms | **42,106 orders/sec** |

**Observation:** Sweep throughput matches resting throughput — the inner fill loop per price level is tight and adds negligible overhead per level.

---

## Summary

| Scenario | Best throughput | Notes |
|---|---|---|
| Resting orders | ~42.7k / sec | Flat scaling, no degradation |
| 1:1 matching | ~38k / sec (small) | Degrades at 10k — HashMap pressure |
| Sweep | ~43k / sec | Inner loop is tight |

**The engine comfortably exceeds the 10k orders/sec target** across all scenarios, even at 10k matched orders. The 10k target is for HTTP throughput under real network conditions — the engine itself is not the bottleneck.

## Next Steps

- Profile `match_1to1` at 10k to confirm HashMap as the cause of the throughput drop
- Save a baseline before any engine refactoring: `cargo bench -- --save-baseline <name>`
- Fill in `http_loadtest.md` once a server is deployed to a VPS

## Scenarios Not Yet Benchmarked

- Cancel throughput (cancel N resting orders)
- Mixed workload (interleaved place + cancel)
- Multi-market (orders spread across N markets)
- Concurrent HTTP throughput — see `http_loadtest.md`
