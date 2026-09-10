# Build stage
FROM rust:1-slim-bookworm AS builder

WORKDIR /usr/src/room-server
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY migrations ./migrations

RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 -s /bin/bash appuser

WORKDIR /app
COPY --from=builder /usr/src/room-server/target/release/ltk-room-server /app/ltk-room-server
COPY --from=builder /usr/src/room-server/migrations /app/migrations

USER appuser

EXPOSE 3000

ENV RUST_LOG=ltk_room_server=info,tower_http=info
ENV SERVER_HOST=0.0.0.0
ENV SERVER_PORT=3000

ENTRYPOINT ["/app/ltk-room-server"]
