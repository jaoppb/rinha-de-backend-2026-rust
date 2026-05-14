FROM rust:1.85-slim AS builder
WORKDIR /app
COPY Cargo.toml ./
ARG FEATURES=
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release ${FEATURES}
COPY src ./src
RUN touch src/main.rs && cargo build --release ${FEATURES}

FROM debian:bookworm-slim
WORKDIR /app
COPY --from=builder /app/target/release/lb /lb
ENTRYPOINT ["/lb"]
