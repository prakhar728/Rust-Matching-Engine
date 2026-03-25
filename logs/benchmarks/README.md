# Benchmarks

This folder stores benchmark results, methodology notes, and performance analysis for the CLOB engine.

## Structure

| File | Contents |
|---|---|
| `README.md` | This file — index and methodology notes |
| `engine_microbench.md` | Criterion microbench results (engine, no HTTP) |
| `http_loadtest.md` | HTTP load test results (separate-machine, to be filled) |

## Methodology

### Two types of benchmarks

**Engine microbench (`benches/engine.rs`)**
- Runs with `cargo bench`
- Benchmarks `Engine::process_order` directly — no HTTP, no network, no JSON
- Results are accurate locally because there is no competing client process
- Uses a fixed clock (`|| 1_000_000u64`) to eliminate syscall noise
- All orders are pre-signed outside the timing loop — ed25519 cost excluded
- Use these numbers to reason about matching engine throughput and regressions

**HTTP load test**
- Must be run from a separate machine (VPS, second container, etc.)
- Local HTTP numbers are misleading: client and server share CPU, loopback has no latency
- Tool: `oha` or `wrk`
- Measures p50 / p95 / p99 latency and throughput under real concurrency
- Fill in `http_loadtest.md` when a server is deployed

### How to run the microbench

```bash
# Run all benchmarks
cargo bench

# Save a named baseline (before a change)
cargo bench -- --save-baseline before

# Compare against a saved baseline (after a change)
cargo bench -- --baseline before

# Run only one benchmark group
cargo bench -- resting_orders
```

Results are written to `target/criterion/` as HTML reports (open in browser).

### What to track over time

- Throughput (orders/sec) per scenario
- Whether throughput scales linearly with N
- Regressions after engine changes — always save a baseline before refactoring
