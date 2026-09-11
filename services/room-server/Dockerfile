ARG RUNTIME_IMAGE=debian:bookworm-slim
ARG BUILDER_IMAGE=rust:1-slim-bookworm

# Build stage
FROM ${BUILDER_IMAGE} AS builder

WORKDIR /usr/src/room-server
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY migrations ./migrations

RUN cargo build --release --locked

# Runtime stage
FROM ${RUNTIME_IMAGE}

ARG SKIP_RUNTIME_APT=false

USER root

RUN if [ "$SKIP_RUNTIME_APT" != "true" ]; then \
      apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
        && rm -rf /var/lib/apt/lists/*; \
    fi

RUN id -u appuser >/dev/null 2>&1 || useradd -m -u 1000 -s /bin/bash appuser

WORKDIR /app
COPY --from=builder /usr/src/room-server/target/release/ltk-room-server /app/ltk-room-server
COPY --from=builder /usr/src/room-server/migrations /app/migrations

USER appuser

EXPOSE 3000

ENV RUST_LOG=ltk_room_server=info,tower_http=info
ENV SERVER_HOST=0.0.0.0
ENV SERVER_PORT=3000

ENTRYPOINT ["/app/ltk-room-server"]
