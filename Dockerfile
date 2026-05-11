FROM rust:1.85-slim AS builder

# Set workdir to a subfolder to match the project structure (api/ next to resources/)
WORKDIR /app/api
COPY resources /app/resources
COPY api/Cargo.toml api/Cargo.lock api/build.rs ./

# Create a dummy main.rs to build dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src

COPY api/src ./src
ARG CARGO_FEATURES=""
RUN cargo build --release --features "${CARGO_FEATURES}"

FROM debian:bookworm-slim
COPY --from=builder /app/api/target/release/api /api
ENTRYPOINT ["/api"]
