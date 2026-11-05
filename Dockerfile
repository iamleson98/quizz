# syntax=docker/dockerfile:1
# Multi-stage build:
#   1. builder  - rust + tcl + git; builds the custom begin-concurrent SQLite
#                 from source, then the release binaries (SQLite statically
#                 linked into them).
#   2. runtime  - debian slim with just the binary and CA certs.

FROM rust:1-slim-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential git tcl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Build the custom SQLite first (layer-cached independently of Rust sources).
COPY scripts/build-sqlite.sh scripts/build-sqlite.sh
RUN ./scripts/build-sqlite.sh

# Build the Rust workspace against the prebuilt SQLite (see .cargo/config.toml).
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY crates ./crates
RUN cargo build --release -p quiz-service -p bench \
    && mkdir -p /out \
    && cp target/release/quiz-service target/release/quiz-bench /out/

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home quiz

COPY --from=builder /out/quiz-service /usr/local/bin/quiz-service
COPY --from=builder /out/quiz-bench  /usr/local/bin/quiz-bench

USER quiz
ENV QUIZ_LISTEN_ADDR=0.0.0.0:8080 \
    QUIZ_DATABASE=/data/quiz.db \
    RUST_LOG=info
VOLUME /data
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -sf http://127.0.0.1:8080/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/quiz-service"]
