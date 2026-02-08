# Stage 1: Build
FROM rust:1.75-slim as builder

RUN apt-get update && apt-get install -y \
    protobuf-compiler \
    libprotobuf-dev \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src src/

# Cache dependencies
RUN mkdir -p .cargo && \
    cargo vendor > /dev/null 2>&1 || true && \
    cargo build --release

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/rusd /usr/local/bin/rusd

RUN chmod +x /usr/local/bin/rusd

EXPOSE 2379 2380

HEALTHCHECK --interval=10s --timeout=5s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:2379/health || exit 1

ENTRYPOINT ["rusd"]
CMD ["--help"]
