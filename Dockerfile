# Build stage: compile the release binary.
FROM rust:1-slim AS builder

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --release -p asp-cli

# Runtime stage: minimal image, non-root user, placeholder binary only.
FROM debian:stable-slim AS runtime

COPY --from=builder /build/target/release/rasp /usr/local/bin/rasp

# debian:stable-slim ships no `adduser`; a dedicated numeric UID keeps the
# runtime stage minimal while guaranteeing a non-root process.
USER 1000:1000
WORKDIR /app

ENTRYPOINT ["/usr/local/bin/rasp"]
CMD ["version"]