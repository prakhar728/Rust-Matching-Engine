FROM rust:1.84-bookworm AS builder

ARG APP_BIN=clob-api
WORKDIR /app

COPY . .
RUN cargo build --release --bin ${APP_BIN}

FROM debian:bookworm-slim AS runtime

ARG APP_BIN=clob-api

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/${APP_BIN} /usr/local/bin/${APP_BIN}

ENV APP_BIN=${APP_BIN}
ENV RUST_LOG=info
ENV APP_PORT=8080

EXPOSE 8080

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/bin/sh", "-c", "/usr/local/bin/${APP_BIN}"]
