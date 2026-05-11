FROM rust:1.85-slim AS builder

WORKDIR /app

# Copy shared resources
COPY resources ./resources

# 1. Build Indexer
COPY indexer/Cargo.toml ./indexer/
COPY indexer/src ./indexer/src
RUN cd indexer && cargo build --release

# 2. Run Indexer to generate data
ARG INPUT_FILE=resources/references.json.gz
RUN mkdir -p /app/data && \
    ./indexer/target/release/indexer ${INPUT_FILE} /app/data

# 3. Build API
COPY api/Cargo.toml api/Cargo.lock api/build.rs ./api/
COPY api/src ./api/src
RUN cd api && cargo build --release

FROM debian:bookworm-slim
WORKDIR /app
COPY --from=builder /app/api/target/release/api /api
COPY --from=builder /app/data /app/data
ENTRYPOINT ["/api"]
