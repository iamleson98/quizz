# Production image of the live quiz service (the Rust game server). Published by
# .github/workflows/image.yml as ghcr.io/<owner>/quizz on a version tag.
#
#   docker build -t quizz .                                          # this platform
#   docker buildx build --platform linux/amd64,linux/arm64 -t quizz .
#
# The service needs SQLite's begin-concurrent branch (several writers at once), so the image builds
# that exact commit (pinned in scripts/build-sqlite.sh) into a static library first and links the
# service against it. Any library in third_party/ is replaced by this build.
#
# The builder always runs natively, on the machine that builds (BUILDPLATFORM), and cross-compiles
# when the image is for another platform: an arm64 laptop building the amd64 image uses the Debian
# cross toolchain for SQLite and for the Rust target, and nothing runs under emulation (the Rust
# compiler crashes under QEMU). Build scripts and proc macros are built for the build machine and
# run there. When both platforms are the same (amd64 CI, a native arm64 build) it is a plain native
# build and installs no cross packages. The runtime image has no package manager and runs no
# program during the build, so it needs no emulation either.
#
# It holds only quiz-service. The load-test client (quiz-bench) is built from source
# (cargo build --release -p bench) and is not in the image.

# ────────────────────────────── Builder ──────────────────────────────
# The latest stable Rust when this was written, pinned by digest (dependabot moves tag and digest
# together).
FROM --platform=$BUILDPLATFORM rust:1.99.1-trixie@sha256:8a4e90c7946b74626488b1446f712c0fe0ed1a596cbd3d871684bc9c5433ae07 AS builder
ARG BUILDARCH
ARG TARGETARCH

# The toolchain for TARGETARCH. tclsh generates SQLite's amalgamation. When the target is another
# architecture, the cross compiler, its C library and the Rust target are installed, and the
# settings that point cargo and the SQLite build at them go to /toolchain.env for the steps below
# (CC and AR are deliberately not exported to cargo: a build script would take them for the
# build machine's compiler).
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu;  gnu=x86_64-linux-gnu;  cross="gcc-x86-64-linux-gnu libc6-dev-amd64-cross" ;; \
      arm64) triple=aarch64-unknown-linux-gnu; gnu=aarch64-linux-gnu; cross="gcc-aarch64-linux-gnu libc6-dev-arm64-cross" ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    packages=tcl; \
    if [ "$TARGETARCH" = "$BUILDARCH" ]; then \
      echo 'export BINARY=target/release/quiz-service' > /toolchain.env; \
    else \
      packages="$packages $cross"; \
      lower="$(printf %s "$triple" | tr - _)"; \
      upper="$(printf %s "$lower" | tr a-z A-Z)"; \
      printf '%s\n' \
        "export BINARY=target/$triple/release/quiz-service" \
        "export SQLITE_CC=$gnu-gcc SQLITE_AR=$gnu-ar" \
        "export CARGO_BUILD_TARGET=$triple" \
        "export CARGO_TARGET_${upper}_LINKER=$gnu-gcc" \
        "export CC_${lower}=$gnu-gcc AR_${lower}=$gnu-ar" \
        "export SQLITE_TARGET_OUT=/build/third_party/sqlite-build-target" \
        "export PKG_CONFIG_ALLOW_CROSS=1" \
        "export HOST_PKG_CONFIG_PATH=/build/third_party/sqlite-build/lib/pkgconfig" \
        "export TARGET_PKG_CONFIG_PATH=/build/third_party/sqlite-build-target/lib/pkgconfig" > /toolchain.env; \
    fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends $packages; \
    rm -rf /var/lib/apt/lists/*; \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then rustup target add "$triple"; fi

WORKDIR /build

# SQLite first, in a layer of its own: it changes when the pinned commit does, not with the sources.
# The library for the build machine always (.cargo/config.toml points at it): when the
# service is cross-compiled, the proc macros (sqlx's) and build scripts still link it and run here.
# A cross build adds the same library compiled for the target; libsqlite3-sys is built once for
# each side, and HOST_PKG_CONFIG_PATH / TARGET_PKG_CONFIG_PATH (/toolchain.env) tell each which
# one it links, so the service's binary carries the target's.
COPY scripts/build-sqlite.sh scripts/build-sqlite.sh
RUN . /toolchain.env \
    && ./scripts/build-sqlite.sh \
    && if [ -n "${SQLITE_CC:-}" ]; then \
         CC="$SQLITE_CC" AR="$SQLITE_AR" SQLITE_OUT="$SQLITE_TARGET_OUT" ./scripts/build-sqlite.sh; \
       fi

# The workspace, built against that library (.cargo/config.toml points at it). Every member
# crate is copied because the workspace names them all; only the service is built. /data is the
# empty data directory the runtime image gets.
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY crates ./crates
RUN . /toolchain.env \
    && cargo build --release --locked -p quiz-service \
    && cp "$BINARY" /quiz-service \
    && mkdir /data

# ────────────────────────────── Runtime ──────────────────────────────
# Distroless: glibc and libgcc, which is all a dynamically linked Rust binary needs, with no shell
# and no package manager, so nothing has to run to assemble it. Debian 13 like the builder, so the
# binary meets the glibc it was linked against. The service makes no TLS connection (it calls the
# LMS server over plain HTTP inside the cluster), so nothing is installed for certificates. Pinned
# by the digest of the multi-platform index (the amd64 and arm64 images both resolve from it);
# raise it deliberately.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:29ee70762214fe8190cea04ef1c0a84a8365879e047ce8f911ae5a5ab2355fa2

COPY --from=builder /quiz-service /usr/local/bin/quiz-service
# 2002 is the user the Kubernetes manifest runs it as; /data is its to write.
COPY --from=builder --chown=2002:2002 /data /data

USER 2002:2002
WORKDIR /data

ENV QUIZ_LISTEN_ADDR=0.0.0.0:8080 \
    QUIZ_INTERNAL_ADDR=0.0.0.0:8081 \
    QUIZ_DATABASE=/data/quiz.db \
    RUST_LOG=info

# 8080 = the public REST API (/v1) and the game's websocket (/ws), reached through the site's edge
# 8081 = health, readiness, metrics and the server's calls (cluster-internal only)
EXPOSE 8080 8081

VOLUME ["/data"]

ENTRYPOINT ["/usr/local/bin/quiz-service"]
