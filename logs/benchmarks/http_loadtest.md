# HTTP Load Test Results

Real-world throughput and latency numbers for the full API stack (axum + engine + JSON).

> **Status: Not yet run.** Fill this in once a server is deployed to a VPS or two separate Docker containers.

## Why not local?

Local HTTP benchmarks are misleading:
- Client and server share the same CPU — both run at half capacity
- Loopback has no real network latency — p50 latency will appear ~0ms
- OS scheduler arbitrarily splits time between the two processes

A valid HTTP load test requires the client and server on **separate machines**.

## Recommended Setup

```
[load generator machine]  ──── LAN/cloud network ────  [server machine]
        oha / wrk                                         cargo run --release
```

Minimum viable setup:
- Two small VPS instances in the same datacenter (same region = low baseline latency)
- Or: two Docker containers with `--cpuset-cpus` to pin to separate cores on the same host

## Tool

```bash
# Install oha (Rust-based, like wrk but with better output)
cargo install oha

# Basic throughput test — 50 concurrent connections, 30 second run
oha -c 50 -z 30s -m POST \
  -H "content-type: application/json" \
  -d '{ ... signed order json ... }' \
  http://<server-ip>:8080/v1/orders

# Latency profile — lower concurrency, focus on tail latency
oha -c 10 -z 30s ...
```

## What to Measure

| Metric | Target | Notes |
|---|---|---|
| Throughput (orders/sec) | ≥ 10,000 | Under sustained concurrent load |
| p50 latency | < 5ms | Median response time |
| p95 latency | < 20ms | 95th percentile |
| p99 latency | < 50ms | Tail latency |
| Error rate | 0% | No 5xx under normal load |

## Results

> To be filled in.

### Run 1

| Property | Value |
|---|---|
| Date | — |
| Server machine | — |
| Client machine | — |
| Network | — |
| Concurrency | — |
| Duration | — |

| Metric | Result |
|---|---|
| Throughput | — |
| p50 latency | — |
| p95 latency | — |
| p99 latency | — |
| Error rate | — |

**Notes:**

---

## Scenarios to Test

- `POST /v1/orders` — resting orders (no match), sustained concurrency
- `POST /v1/orders` — crossing orders (1:1 match), sustained concurrency
- Mixed read/write — `GET /v1/books/:market_id` interleaved with order placement
- WebSocket fan-out — N connected WS clients while orders are placed at high rate
