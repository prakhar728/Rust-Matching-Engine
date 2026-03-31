# ── Stage 1: build ────────────────────────────────────────────────────────────
FROM rust:1.88-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./

RUN mkdir -p src/bin benches && \                                                                                         
    echo 'fn main() {}' > src/main.rs && \                                                                                
    echo 'fn main() {}' > src/bin/cli.rs && \                                                                             
    echo 'fn main() {}' > benches/engine.rs && \                                                                          
    cargo build --release --bin clob-api && \                                                                             
    rm -rf src benches


COPY src ./src
COPY benches ./benches
RUN touch src/main.rs && cargo build --release --bin clob-api

# ── Stage 2: runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime


RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/clob-api /usr/local/bin/clob-api

ENV RUST_LOG=info

EXPOSE 8080

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["clob-api"]
