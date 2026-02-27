FROM rust:1.92-bookworm AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/any-player-sync-server /usr/local/bin/any-player-sync-server

ENV DB_HOST=127.0.0.1 \
    DB_PORT=5432 \
    DB_USER=postgres \
    DB_PASSWORD=postgres \
    DB_NAME=any_player_sync \
    DB_SSLMODE=prefer

# BIND_ADDRESS defaults to 127.0.0.1:8080. For Docker deployments where the
# container port must be reachable via published ports, set:
#   -e BIND_ADDRESS=0.0.0.0:8080
# Only do this when the service is behind an authenticated, access-controlled
# reverse proxy — the API has no built-in authentication.

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/any-player-sync-server"]
