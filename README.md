# quiz-backend

A high-performance **GraphQL quiz service** in Rust (axum + async-graphql + sqlx) on a
**custom SQLite build**: the [`begin-concurrent`](https://github.com/sqlite/sqlite/tree/begin-concurrent)
branch of the SQLite repository, compiled from source and **statically linked into the
binary**, so sqlx transparently uses it instead of the system SQLite.

```text
$ curl -s localhost:8080/healthz
{"sqlite":"3.55.0","status":"ok","tx_mode":"concurrent","uptime_sec":1}
```

## Why the begin-concurrent branch?

Vanilla SQLite allows at most **one writer at a time**. The `begin-concurrent` branch adds
optimistic **page-level locking**: transactions started with `BEGIN CONCURRENT` run
concurrently and only serialize (and conflict-check) at `COMMIT`. A commit whose pages were
touched by another writer fails with `SQLITE_BUSY_SNAPSHOT` and is simply retried.

This service is built to make the most of that:

| Technique | Where |
|---|---|
| **Every write** runs in `BEGIN CONCURRENT` with automatic `SQLITE_BUSY_SNAPSHOT` retry (exponential backoff + jitter, metrics per retry) | `crates/quiz-service/src/tx.rs` |
| **Random 62-bit primary keys** instead of auto-increment rowids — monotonically increasing ids funnel all concurrent inserts onto the *same* b-tree page and cause conflict storms (see `third_party/sqlite/doc/begin_concurrent.md`) | `src/ids.rs` |
| **In-process COMMIT mutex** — the branch docs recommend serializing COMMITs with an application mutex instead of letting the busy-handler spin when all writers share one process | `src/tx.rs` |
| `unix-excl` VFS (configurable) — more efficient locking for single-process use, per the branch docs | `src/db.rs` |
| WAL journaling, `synchronous=NORMAL`, 32 MiB page cache, 256 MiB mmap, in-memory temp store | `src/db.rs` |
| sqlx **linked against our static SQLite** via `SQLITE3_LIB_DIR` + a pkg-config shim (no bundled 3.46, no system lib) | `.cargo/config.toml`, `third_party/sqlite-build/` |

Measured on a 2 vCPU sandbox (server + load generator colocated), the spread-write
scenario shows **+19 % graded attempts/s** over `BEGIN IMMEDIATE`:

| Scenario (c=8, 10 s) | Mode | req/s | submit/s | p50 | p95 | p99 | errors |
|---|---|---:|---:|---:|---:|---:|---:|
| read | concurrent | **4 195** | — | 1.4 ms | 2.7 ms | 26 ms | 0 |
| mixed (80/20) | concurrent | **2 200** | 369 | 1.8 ms | 13 ms | 34 ms | 0 |
| write, 1 hot quiz | concurrent | 1 213 | 606 | 1.0 ms | 29 ms | 114 ms | 0 |
| write, 1 hot quiz | immediate | 1 192 | 596 | — | — | — | 0 |
| write, 8 quizzes | concurrent | **1 408** | **704** | 2.3 ms | 39 ms | 108 ms | 0 |
| write, 8 quizzes | immediate | 1 185 | 592 | 1.8 ms | 31 ms | 103 ms | 0 |

The "1 hot quiz" case is deliberately adversarial: every writer touches the same
leaderboard index page, the worst case for page-level locking. The retry engine absorbs
every conflict (5 235 retries, **zero failed requests**). Spread writes across quizzes and
`BEGIN CONCURRENT` pulls clearly ahead. Re-run everything yourself with
`loadtest/compare-tx-modes.sh`.

## Features

- **Auth**: register/login, Argon2id password hashing, HS256 JWT bearer tokens.
- **Quizzes**: create/update/delete, public or private, optional time limit and max attempts.
- **Questions**: single choice, multiple choice (partial credit), true/false — batched
  creation, server-side grading (the client never sends correctness data).
- **Gameplay**: start attempt → submit answers → instant graded result with per-question
  breakdown; ownership and time-limit enforcement.
- **Leaderboards**: top-N per quiz (window-function ranking) + **live updates over
  GraphQL subscriptions** (`graphql-transport-ws` at `/graphql/ws`).
- **Observability**: structured JSON logs (or pretty), OpenTelemetry OTLP/gRPC traces,
  Prometheus `/metrics` (http + tx + business counters), per-operation tracing spans.
- **Correctness under concurrency**: integration tests include 12 simultaneous submits
  that must all commit with exact scores.

## Layout

```
├── Cargo.toml                  # workspace: quiz-service + bench
├── .cargo/config.toml          # env-var linking of the custom SQLite into libsqlite3-sys
├── crates/quiz-service/        # the service (lib + bin + integration tests)
│   ├── migrations/0001_init.sql
│   └── src/
│       ├── tx.rs               # BEGIN CONCURRENT engine (retry/backoff/metrics)
│       ├── grading.rs          # single/multi/boolean + partial credit
│       ├── db.rs               # pool, pragmas, migrations, demo seed
│       ├── telemetry.rs        # OTel + Prometheus + tracing
│       ├── auth.rs             # argon2 + JWT
│       ├── repo/               # users / quizzes / attempts data access
│       ├── graphql/            # schema, query/mutation/subscription roots
│       └── main.rs
├── crates/bench/               # quiz-bench load generator (Rust)
├── scripts/build-sqlite.sh     # reproducible custom-SQLite build (pinned commit)
├── third_party/sqlite-build/   # libsqlite3.a + headers + sqlite3.pc (generated, git-ignored)
├── loadtest/                   # oha/wrk recipes + tx-mode comparison script
├── observability/              # prometheus + grafana provisioning
├── Dockerfile                  # multi-stage, SQLite built from source inside
└── docker-compose.yml          # app + Jaeger + Prometheus + Grafana
```

## Quickstart (Docker)

```bash
docker compose up --build
```

| What | Where |
|---|---|
| GraphQL API | http://localhost:8080/graphql |
| Playground | http://localhost:8080/playground |
| Jaeger UI (traces) | http://localhost:16686 — service `quiz-service` |
| Prometheus | http://localhost:9090 |
| Grafana (dashboard provisioned) | http://localhost:3000 |
| Demo account | user `demo`, password `demo1234` |

The compose stack seeds a demo quiz on first boot (`QUIZ_SEED_DEMO=1`).

## Quickstart (local)

Requirements: Rust 1.85+, `cc`, `make`, `git`, `tclsh`. The custom SQLite is not committed
(a static library is platform specific): build it once, then the service.

```bash
./scripts/build-sqlite.sh
cargo build --release
QUIZ_DATABASE=/tmp/quiz.db QUIZ_LISTEN_ADDR=127.0.0.1:8080 RUST_LOG=info \
  ./target/release/quiz-service
```

Run the test suite (17 tests incl. end-to-end flows and the concurrent-submit test):

```bash
cargo test
```

## API tour

```graphql
mutation { register(username: "alice", password: "password123") { token user { id username } } }
# use the token:  Authorization: Bearer <token>

mutation {
  createQuiz(input: { title: "Rust Basics", isPublic: true }) { id }
  # then addQuestions(quizId, input: [ ... ])
}

mutation {
  startAttempt(quizId: "<quiz-id>") { id status startedAt }
  submitAttempt(attemptId: "<attempt-id>", answers: [
    { questionId: "<q1>", selectedOptionIds: ["<opt>"] },
  ]) { score maxScore correctCount totalCount answers { correct awarded possible } }
}

query { leaderboard(quizId: "<quiz-id>", first: 10) { rank user { username } score } }

subscription { leaderboardUpdated(quizId: "<quiz-id>") { entries { rank user { username } score } } }
```

All errors carry machine-readable codes: `UNAUTHORIZED`, `FORBIDDEN`, `NOT_FOUND`,
`BAD_REQUEST`, `CONFLICT`, `DATABASE_ERROR`, `TX_EXHAUSTED`, `INTERNAL_ERROR`.

IDs are random 62-bit integers surfaced as GraphQL `ID` strings; timestamps are unix
epoch **milliseconds**.

## Benchmarks

```bash
cargo build --release
./loadtest/compare-tx-modes.sh 20 8 8   # duration concurrency quizzes

# or by hand:
./target/release/quiz-bench --scenario write --concurrency 16 --duration 30 --quizzes 8
./target/release/quiz-bench --scenario read  --concurrency 50 --duration 30
./loadtest/oha-read.sh http://127.0.0.1:8080 30s 50
```

`quiz-bench` bootstraps its own users and quizzes, then reports throughput, per-operation
latency percentiles and error counts. `--scenario write` is the `BEGIN CONCURRENT` hot
path (start + grade + commit per iteration).

## The custom SQLite build

`scripts/build-sqlite.sh` clones the sqlite repository at a **pinned commit** on the
`begin-concurrent` branch (SQLite 3.55.0 as of this pin), generates the amalgamation,
and compiles `libsqlite3.a` with:

- `-O3`, thread-safe, full-text-search (FTS3/4/5), R\*Tree/GeoPoly, STAT4, math functions
- **sqlx compatibility set** mirrored from libsqlite3-sys' bundled build (API armor,
  column metadata, unlock-notify, load extension, soundex, URI filenames, ...)
- performance defaults: `SQLITE_DEFAULT_MEMSTATUS=0`, `SQLITE_DEFAULT_WAL_SYNCHRONOUS=1`
  (NORMAL), `LIKE_DOESNT_MATCH_BLOBS`

sqlx then links this build instead of its bundled 3.46 via `.cargo/config.toml`:

```toml
[env]
LIBSQLITE3_SYS_USE_PKG_CONFIG = "1"   # force libsqlite3-sys to its "linked" path
SQLITE3_STATIC = "1"                  # prefer the .a over any system .so
SQLITE3_LIB_DIR = { value = "third_party/sqlite-build/lib", relative = true }
SQLITE3_INCLUDE_DIR = { value = "third_party/sqlite-build/include", relative = true }
```

`third_party/sqlite-build/lib/pkgconfig/sqlite3.pc` (path-portable via `${pcfiledir}`)
ensures the pkg-config probe inside libsqlite3-sys resolves to *our* library even when a
system SQLite is installed. Proof of linkage: `/healthz` reports the version, `ldd`
shows no `libsqlite3.so`, and the integration tests assert `sqlite_version() = 3.55.0`.

To rebuild: `./scripts/build-sqlite.sh` (override with `SQLITE_GIT_REF=<ref>`). Build
metadata is recorded in `third_party/sqlite-build/BUILD_INFO`.

## Configuration (env)

| Variable | Default | Meaning |
|---|---|---|
| `QUIZ_LISTEN_ADDR` | `0.0.0.0:8080` | HTTP listen address |
| `QUIZ_DATABASE` | `quiz.db` | SQLite file path (`sqlite://` prefix tolerated) |
| `QUIZ_JWT_SECRET` | dev secret (warns) | JWT signing key |
| `QUIZ_JWT_TTL_HOURS` | `24` | token lifetime |
| `QUIZ_POOL_MAX` / `QUIZ_POOL_MIN` | `8` / `2` | sqlx pool size |
| `QUIZ_BUSY_TIMEOUT_MS` | `5000` | per-connection busy timeout |
| `QUIZ_TX_MODE` | `concurrent` | `concurrent` \| `deferred` \| `immediate` |
| `QUIZ_TX_MAX_RETRIES` | `64` | write-tx retry budget on busy/snapshot conflicts |
| `QUIZ_COMMIT_MUTEX` | `1` | serialize COMMITs in-process (recommended) |
| `QUIZ_SQLITE_VFS` | `unix-excl` | `unix` if external tools need the live file |
| `QUIZ_CACHE_SIZE_KIB` | `32768` | per-connection page cache |
| `QUIZ_MMAP_SIZE` | `268435456` | mmap read limit |
| `QUIZ_OTEL_ENDPOINT` | unset | OTLP gRPC endpoint (e.g. `http://jaeger:4317`) |
| `QUIZ_LOG_FORMAT` | `json` | `json` \| `pretty` |
| `QUIZ_SEED_DEMO` | `0` | idempotent demo user + quiz on boot |
| `RUST_LOG` | `info` | tracing filter |

## Operational notes

- **Keep write transactions small**: concurrent transactions cannot spill their page
  cache mid-transaction (enforced by the patch itself); each submit grades one attempt,
  comfortably within the 32 MiB cache.
- **unix-excl**: the default VFS locks the database file exclusively for the process —
  the sqlite3 CLI cannot attach to the *live* file. Set `QUIZ_SQLITE_VFS=unix` when you
  need that (small locking overhead).
- **Leaderboard index**: the per-quiz leaderboard index is the one deliberate hotspot
  (score + submission time of the same quiz cluster on a few pages). Contention there is
  exactly what the retry engine is for; spread across quizzes and it disappears.
- **Metrics of note**: `tx_retries_total{mode}` (conflict rate), `tx_duration_seconds`
  (including retries), `attempts_submitted_total`, `http_request_duration_seconds`.

## Testing

```bash
cargo test                 # 10 unit + 7 integration tests
cargo clippy --all-targets
cargo fmt --check
```

Integration tests spin the real router on temp SQLite files and cover: full play flow
with partial credit and hidden answers, auth/ownership, validation, time limits,
max attempts, health/metrics endpoints, and 12-way concurrent submits.

## License

MIT.
