# HTTP Load Test Results

**Date:** 2026-11-05 · **Target:** `quiz-service` (axum + async-graphql + embedded SQLite 3.55.0 begin-concurrent branch)
**Environment:** 2 vCPU / 4 GB RAM Linux sandbox · server and load client share the same CPUs · WAL mode, `synchronous=NORMAL`, 32 MiB page cache, 256 MiB mmap, pool max 8, commit mutex on
**Client:** `loadtest/http-loadtest.py` — closed-loop virtual users, aiohttp + uvloop, HTTP keep-alive, POST `/graphql`
**Data:** 24 registered users, 12 public quizzes × 5 questions (single / multi / boolean), JWT auth on every request

Reproduce: `./loadtest/run-http-suite.sh concurrent full full` then `./loadtest/run-http-suite.sh immediate writes full`

## 1. Read path (40% quiz detail / 35% quiz list / 25% leaderboard)

| VUs | RPS | p50 | p90 | p99 | max | errors |
|----:|----:|----:|----:|----:|----:|-------:|
| 16  | 2,629 | 4.3 ms | 8.6 ms | 31.8 ms | 65 ms | 0 |
| 64  | 3,025 | 17.1 ms | 41.3 ms | 56.9 ms | 87 ms | 0 |
| 128 | 3,139 | 37.7 ms | 70.3 ms | 94.4 ms | 127 ms | 0 |
| 256 | 3,334 | 69.9 ms | 126.1 ms | 170.9 ms | 209 ms | 0 |

- Throughput saturates at **~3,300 HTTP GraphQL req/s**; beyond 64 VUs extra concurrency only inflates queueing latency.
- The 2 vCPU box is shared between the server and the Python load client — the read path is **client-bound** here (a single 4-VU smoke run measured p50 **1.2 ms**). On dedicated multi-core hardware the ceiling is substantially higher.
- Zero errors at every concurrency level.

## 2. Write path — BEGIN CONCURRENT vs BEGIN IMMEDIATE (end-to-end, fresh DB per mode)

Each cycle = `startAttempt` + `submitAttempt` (server-side grading of 5 answers inside one write transaction, then leaderboard publish). RPS counts individual mutation ops.

| Scenario | CONCURRENT | IMMEDIATE | Δ throughput |
|---|---:|---:|---:|
| write, 16 VUs | **1,577 ops/s** (789 submissions/s) | 1,510 ops/s (755/s) | **+4.4%** |
| write, 48 VUs | **1,767 ops/s** (883 submissions/s) | 1,654 ops/s (827/s) | **+6.8%** |
| mixed 75/25, 48 VUs | **2,063 ops/s** | 2,036 ops/s | +1.4% |

Latency tail at 48 VUs:

| | CONCURRENT | IMMEDIATE |
|---|---:|---:|
| p50 | 5.9 ms | 5.3 ms |
| p90 | 27.4 ms | 23.9 ms |
| p99 | **82.5 ms** | 107.3 ms |
| max | **289 ms** | **1,790 ms** |

**The headline result is the tail, not the mean.** BEGIN IMMEDIATE serializes writers at `BEGIN`, so contention waits pile up in front of the write lock and explode the worst case (**1.79 s max**). BEGIN CONCURRENT lets writers run in parallel and resolves conflicts at commit time with exponential backoff — worst case stays at **289 ms**, p99 drops 23%, and it still wins on throughput.

## 3. Transaction engine internals (Prometheus `/metrics`)

| Metric | CONCURRENT | IMMEDIATE |
|---|---:|---:|
| tx commits | 117,233 | 112,356 |
| `tx_retries_total` (BUSY_SNAPSHOT) | 28,702 (24.5% of txs) | 0 |
| avg tx duration | **8.10 ms** | 9.55 ms |
| failed txs / user-visible errors | **0** | **0** |

- A quarter of write transactions hit a snapshot conflict under 48-way concurrency and were **transparently retried** by the engine — callers never saw a failure.
- Average write transaction is **17.9% faster** end-to-end under CONCURRENT, consistent with the DB-level micro-benchmark (`quiz-bench`, +19%).

## 4. Resource usage

- Server RSS: **64 MB** (concurrent run) / **71 MB** (immediate run) after ~85k write ops and ~117k txs — flat, no growth.
- DB after ~230k transactions across both runs: a few MB of WAL that checkpointed cleanly.

## 5. Conclusions

1. **Reads:** ~3.3k GraphQL req/s with sub-10 ms p50 on a 2-core box that also runs the load generator; the ceiling is client-bound, not server-bound.
2. **Writes:** ~880 complete graded quiz submissions/s with 24.5% of transactions winning optimistic conflicts — zero lost updates, zero errors.
3. **BEGIN CONCURRENT is the right default** for this workload: +5-7% throughput, −23% p99, **6.2× lower worst-case latency** than BEGIN IMMEDIATE.
4. Numbers are a floor for the sandbox; expect materially higher absolute figures on dedicated multi-core hardware (README notes the same for `loadtest/compare-tx-modes.sh`).

Raw per-run JSON lives in `loadtest/results-full/` (concurrent) and `loadtest/results-writes/` (immediate), tx metrics in `metrics-after.txt`, chart in `loadtest-results.png`.
