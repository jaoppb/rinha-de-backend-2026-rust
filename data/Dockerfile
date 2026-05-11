# Stage 1: Build the Rust indexer
FROM rust:1.85-slim AS builder
WORKDIR /app
COPY data/indexer/Cargo.toml data/indexer/Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src
COPY data/indexer/src ./src
RUN cargo build --release

# Stage 2: Run the indexer to generate binaries
FROM rust:1.85-slim AS generator
WORKDIR /app
COPY --from=builder /app/target/release/indexer /usr/local/bin/indexer

# Accept the input file as a build argument
ARG INPUT_FILE=resources/references.json.gz
COPY $INPUT_FILE /app/input_data

RUN mkdir /data && \
    indexer /app/input_data /data && \
    ls -lh /data

# Stage 3: Final image to serve the data
FROM alpine:latest
RUN mkdir -p /data/internal /data/shared
COPY --from=generator /data/*.bin /data/internal/
ENTRYPOINT ["sh", "-c", "cp /data/internal/*.bin /data/shared/ && echo 'Datasets loaded to /data/shared' && ls -lh /data/shared/ && tail -f /dev/null"]
