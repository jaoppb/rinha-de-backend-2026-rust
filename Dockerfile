FROM rust:1.85-slim AS builder

WORKDIR /app
COPY api/Cargo.toml api/Cargo.lock ./
# Create a dummy main.rs to build dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src

COPY api/src ./src
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/api /api
ENTRYPOINT ["/api"]
