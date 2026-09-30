# Build stage: compile the release binary.
FROM rust:1-slim AS builder

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --release -p asp-cli

# Runtime stage: minimal image, non-root user, placeholder binary only.
FROM debian:stable-slim AS runtime

RUN adduser --system --group --home /app app

COPY --from=builder /build/target/release/rasp /usr/local/bin/rasp

USER app
WORKDIR /app

ENTRYPOINT ["/usr/local/bin/rasp"]
CMD ["version"]