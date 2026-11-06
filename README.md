# quiz-service

A live quiz game for a classroom, and the teacher's quiz bank behind it. Rust
(axum, sqlx) on a **custom SQLite**: the
[`begin-concurrent`](https://github.com/sqlite/sqlite/tree/begin-concurrent) branch,
compiled from source and statically linked into the binary.

The teacher picks a quiz and starts a game in a live session; every student
sees each question at once, with a time limit; when it closes everyone sees the
right answer, how the class answered and the top of the scoreboard; the teacher
decides when to move on; at the end there is a podium. The teacher then keeps
the quiz and its results, or lets them go.

```text
$ curl -s localhost:8081/healthz
{"sqlite":"3.55.0","status":"ok","tx_mode":"concurrent","uptime_sec":1}
```

## What it is

| Part | Where |
|---|---|
| **The bank**: authoring, listing, keeping and deleting quizzes, and reading finished games' results. REST/JSON under `/v1`, described by [`openapi.json`](openapi.json) | public port 8080 |
| **The game**: one websocket, `GET /ws`, subprotocol `quiz.v1`, compact frames. Written down in [`PROTOCOL.md`](PROTOCOL.md) | public port 8080 |
| **Probes, metrics and the calls only the LMS server makes** | internal port 8081 |

There are **no accounts, passwords or logins**. Everything the service knows
about a person comes in a *ticket*: a short-lived HS256 JWT signed by the LMS
server with a key only the two share. Quizzes and players are known by the
LMS's own ids (text), opaque to this service.

- Quizzes: single choice, multiple choice (partial credit) and true/false;
  text only. 200 quizzes per teacher, 100 questions per quiz, question text 500
  characters, options 200, 2 to 6 options (a true/false question has exactly
  2), 5 to 120 seconds per question (default 20), 500 players per game, one
  running game per session and per teacher.
- Scoring: a right answer earns `1000 x (0.5 + 0.5 x remaining/limit)`, rounded;
  multiple choice is scaled by its partial credit; wrong or none earns 0. A
  **streak bonus** is added for answering fully correctly several questions in a
  row: `min(100 x (n - 1), 500)` for the n-th in a row (nothing for the first); a
  wrong, partly right or missing answer ends the streak and a player who joins
  mid-game starts at 0. The scoreboard orders by total, then by total answer
  time (a question not answered counts as its whole time limit).
- The host sees the class live: how many chose each option, the names of those
  who join, and a **provisional top ten** that is updated as answers arrive (at
  most every 500 ms). An answer is scored when it arrives, with the figures the
  close will apply, so the provisional ranking is the final one. Players are
  never sent any of it while a question is open: a moving ranking would tell an
  unanswered student which option is right.
- Answer keys never leave the service before a question's reveal.
- Answers are held in memory during a question and written in **one
  transaction when it closes**: a class answering together is one write.
- A game's connected minutes (the host's, each player's) are pushed to the LMS
  server as *spans*: lesson time for the fee and attendance.

## Tickets

| Claim | Meaning |
|---|---|
| `iss` / `aud` | `lms` / `quiz`, both checked |
| `sub` | the person's id (opaque, at most 64 characters) |
| `name` | display name, shown on the scoreboard |
| `role` | `teacher` (the bank only), `host` (the teacher of this session: the bank and running its game), `player` (a student of this session) |
| `sid`, `cid` | session id and class id (`host` and `player` only) |
| `rc` | when the room closes, unix ms (`host` and `player` only): the game ends then |
| `iat` / `exp` | valid 10 minutes; a socket is checked once, when it opens |

HTTP: `Authorization: Bearer <ticket>`. Websocket: the first frame, `hello`.
Never in a URL.

## Quickstart

Requirements: Rust 1.88+, `cc`, `make`, `git`, `tclsh` (to build SQLite).

```bash
./scripts/build-sqlite.sh          # once: the custom SQLite into third_party/sqlite-build/
cargo build --release

export QUIZ_TICKET_KEY=$(openssl rand -hex 32)
export QUIZ_INTERNAL_KEY=$(openssl rand -hex 32)
export QUIZ_ALLOWED_ORIGINS=http://localhost:5173
QUIZ_DATABASE=/tmp/quiz.db QUIZ_SNAPSHOT_DIR= QUIZ_LMS_URL= \
  ./target/release/quiz-service
```

The service refuses to start without the two keys (at least 32 bytes each) and
the allowed origins. `QUIZ_SNAPSHOT_DIR=` (empty) turns snapshots off and
`QUIZ_LMS_URL=` (empty) turns span pushing off, which is what you want without
the LMS server beside it.

The Docker image builds SQLite itself (`docker compose up --build`, with the keys
in the environment).

## Try it with a ticket

`mint-ticket` is a development tool: it signs a ticket with `QUIZ_TICKET_KEY`
(in production the LMS server does this; the tool is not in the image).

```bash
mint() { cargo run -q -p quiz-service --bin mint-ticket -- "$@"; }

TEACHER=$(mint --role teacher --sub teacher1 --name "Ms Lan")
HOST=$(mint --role host --sub teacher1 --name "Ms Lan" --sid session1 --cid class1)
STUDENT=$(mint --role player --sub student1 --name "An" --sid session1 --cid class1)
```

The bank, with `curl` (ids are strings; times are unix ms):

```bash
API=http://127.0.0.1:8080
curl -s -X POST $API/v1/quizzes -H "Authorization: Bearer $TEACHER" -H 'Content-Type: application/json' -d '{
  "title": "Fractions", "description": "Warm-up",
  "questions": [
    {"kind": "single", "text": "1/2 + 1/2 = ?", "timeLimitSec": 20, "explanation": "One whole.",
     "options": [{"text": "1", "isCorrect": true}, {"text": "2", "isCorrect": false}]},
    {"kind": "boolean", "text": "0.5 equals 1/2",
     "options": [{"text": "True", "isCorrect": true}, {"text": "False", "isCorrect": false}]}
  ]}'                                                   # 201 + the quiz, with answer keys
curl -s "$API/v1/quizzes?limit=20" -H "Authorization: Bearer $TEACHER"
curl -s -i $API/v1/quizzes/<id> -H "Authorization: Bearer $TEACHER"       # ETag; send it back in If-None-Match for a 304
curl -s -X PUT $API/v1/quizzes/<id> -H "Authorization: Bearer $TEACHER" -H 'Content-Type: application/json' -d @quiz.json
curl -s -X PUT $API/v1/quizzes/<id>/kept -H "Authorization: Bearer $TEACHER" -H 'Content-Type: application/json' -d '{"kept": false}'
curl -s $API/v1/quizzes/<id>/games -H "Authorization: Bearer $TEACHER"    # results of finished games
curl -s -X DELETE $API/v1/quizzes/<id> -H "Authorization: Bearer $TEACHER"
```

A quiz is saved whole: `PUT` takes the title and the full question list. A
question or option that carries the `id` it was read with keeps its identity
(and the results recorded for it); one without an id is new; one left out is
deleted. Errors use the real status and a body `{"code","message","fields"}`;
`code` is `UNAUTHORIZED`, `FORBIDDEN`, `NOT_FOUND`, `BAD_REQUEST`, `CONFLICT` or
`INVALID` (422, with `fields: [{path, message}]` such as
`questions[2].options[1].text`).

The game, with the socket (Node 22 or later has `WebSocket` built in):

```bash
node -e '
const [host, player] = process.argv.slice(1);
const open = (t, name) => { const ws = new WebSocket("ws://127.0.0.1:8080/ws", { protocols: ["quiz.v1"], headers: { origin: "http://localhost:5173" } });
  ws.onopen = () => ws.send(JSON.stringify([0, t])); ws.onmessage = (m) => console.log(name, m.data); return ws; };
const h = open(host, "host"), p = open(player, "student");
setTimeout(() => h.send(JSON.stringify([1, "<quiz id>"])), 500);   // lobby
setTimeout(() => h.send("[2]"), 1500);                              // first question
setTimeout(() => p.send("[4,1,1]"), 2500);                          // answer question 1: option 1
setTimeout(() => h.send("[3]"), 4000);                              // end
setTimeout(() => process.exit(0), 4500);' "$HOST" "$STUDENT"
```

`PROTOCOL.md` has every frame. The OpenAPI document is generated from the code;
`openapi.json` at the root is committed and a test fails when it is out of date
(`UPDATE_OPENAPI=1 cargo test -p quiz-service --test openapi` rewrites it).

## Retention: nothing lasts for ever

Every quiz carries the moment it expires (`expiresAt`), every finished game
the moment its results do. A sweep, **at startup and daily**, deletes what has
expired. The three periods are constants in `src/retention.rs`, not settings:

| What | Expires |
|---|---|
| a kept quiz (made in the bank) | 90 days after its last use |
| a quiz that is not kept (a quick quiz made in the room) | 7 days after its last use |
| a finished game's results (players, answers) | 90 days after the game ended, even while its quiz stays |

*Use* is saving an edit (create or update), starting a game on the quiz, or
keeping / un-keeping it; each use moves the expiry a full period on, and
keeping a quiz switches between the 7- and the 90-day period. Reading it,
listing it and fetching its results are not uses. Deleting a quiz deletes its
games at once. A game that was running when the service stopped is closed at
the next start so its results start their own countdown.

`POST /internal/users/{id}/forget` (header `x-internal-key`) deletes that
person's quizzes and games, ends a game they host, and blanks their name on
other teachers' scoreboards. The LMS server calls it when an account is deleted.

## Configuration (environment)

| Variable | Default | Meaning |
|---|---|---|
| `QUIZ_TICKET_KEY` | **required** | HS256 key tickets are verified with (at least 32 bytes) |
| `QUIZ_INTERNAL_KEY` | **required** | key of the `x-internal-key` header on `/internal/*` (at least 32 bytes), compared in constant time |
| `QUIZ_ALLOWED_ORIGINS` | **required** | comma-separated origins of the site: CORS answers them only, and a websocket upgrade must come from one |
| `QUIZ_LISTEN_ADDR` | `0.0.0.0:8080` | public listener: `/v1/*` and `/ws` |
| `QUIZ_INTERNAL_ADDR` | `0.0.0.0:8081` | internal listener: `/healthz`, `/readyz`, `/metrics`, `/internal/*` |
| `QUIZ_DATABASE` | `quiz.db` (`/data/quiz.db` in the image) | SQLite file (`sqlite://` prefix tolerated) |
| `QUIZ_SNAPSHOT_DIR` | `/data/snapshots` | where `quiz.db` snapshots are written; empty turns snapshots off |
| `QUIZ_SNAPSHOT_INTERVAL_SECS` | `900` | time between snapshot attempts |
| `QUIZ_LMS_URL` | `http://lms-server.lms.svc.cluster.local:8065` | the LMS server (plain http) that receives lesson-time spans; empty turns pushing off |
| `QUIZ_WORKER_THREADS` | `2` | tokio worker threads: on its node the service shares cores with the SFU |
| `QUIZ_POOL_MAX` / `QUIZ_POOL_MIN` | `8` / `2` | sqlx pool size |
| `QUIZ_BUSY_TIMEOUT_MS` | `5000` | per-connection busy timeout |
| `QUIZ_TX_MODE` | `concurrent` | `concurrent` \| `deferred` \| `immediate` |
| `QUIZ_TX_MAX_RETRIES` | `64` | write-transaction retry budget on busy/snapshot conflicts |
| `QUIZ_COMMIT_MUTEX` | `1` | serialize COMMITs in-process (recommended) |
| `QUIZ_SQLITE_VFS` | `unix-excl` | `unix` if external tools need the live file |
| `QUIZ_CACHE_SIZE_KIB` | `32768` | per-connection page cache |
| `QUIZ_MMAP_SIZE` | `268435456` | mmap read limit |
| `QUIZ_OTEL_ENDPOINT` | unset | OTLP gRPC endpoint (e.g. `http://jaeger:4317`) |
| `QUIZ_LOG_FORMAT` | `json` | `json` \| `pretty` |
| `RUST_LOG` | `info` | tracing filter |

## Operations

- **One replica.** The SQLite file takes an exclusive lock (`unix-excl`), so
  deploy with strategy `Recreate`. Restarting ends running games (the teacher
  starts the game again); everything already written stays. On `SIGTERM` the
  service ends games, closes every open span, makes one last push to the LMS
  server, closes sockets (1001) and exits.
- **Snapshots.** Every `QUIZ_SNAPSHOT_INTERVAL_SECS`, when anything changed, the
  database is copied with `VACUUM INTO` to `quiz.db.tmp` in `QUIZ_SNAPSHOT_DIR`,
  fsynced and renamed over `quiz.db` (atomic; works with the default
  `unix-excl` VFS). The off-site copy takes `quiz.db` from there.
  `quiz_snapshot_last_success_seconds` is the Unix time of the last interval at
  which the snapshot was current: it advances at **every** interval, also when
  nothing changed and no file was written, so an idle night is not a stale
  backup.
- **Lesson-time spans.** The service notes each connected stretch of a running
  game (game, session, user, role `host`|`player`, from, to) in an outbox table
  and `POST`s them to `QUIZ_LMS_URL` + `/api/v4/lms/internal/quiz/spans`, header
  `x-internal-key`, body `{"spans":[{"game_id","session_id","user_id","role",
  "from","to"}]}` (`game_id` a decimal string, times in ms), at most 500 a
  request, answer 204. A host's open span is re-sent every 60 s with its end so
  far; a player's is sent when it closes (disconnect, game end, `rc`, shutdown).
  A reconnect within 60 s extends the span. The call is idempotent by (game,
  user, from) and a span's end only grows. Network errors and 5xx are retried
  with exponential backoff and jitter (1 s to 5 min); a 404 or any other 4xx
  drops the span (counted).
- **Metrics** (internal `/metrics`): `quiz_games_running`,
  `quiz_players_connected`, `quiz_answers_total`,
  `quiz_question_close_seconds` (histogram), `tx_commits_total`,
  `tx_retries_total` (write conflicts), `tx_duration_seconds`,
  `quiz_snapshot_last_success_seconds`, `quiz_spans_outbox_depth`,
  `quiz_spans_pushed_total`, `quiz_spans_dropped_total`, and
  `http_requests_total` / `http_request_duration_seconds` per route.
  `observability/` has a Prometheus config and a Grafana dashboard.
- **Keep write transactions small**: concurrent transactions cannot spill their
  page cache mid-transaction (enforced by the patch itself).
- **`unix-excl`** locks the database file exclusively for the process, so the
  `sqlite3` CLI cannot attach to the live file; read a snapshot instead, or set
  `QUIZ_SQLITE_VFS=unix`.

## Why the begin-concurrent branch

Vanilla SQLite allows one writer at a time. `begin-concurrent` adds optimistic
page-level locking: transactions started with `BEGIN CONCURRENT` run together and
only conflict-check at `COMMIT`; a commit whose pages another writer touched
fails with `SQLITE_BUSY_SNAPSHOT` and is retried.

| Technique | Where |
|---|---|
| Every write runs in `BEGIN CONCURRENT` with retry (exponential backoff and jitter, metrics per retry) | `src/tx.rs` |
| Random 62-bit row ids: increasing ids funnel concurrent inserts onto one page | `src/ids.rs` |
| An in-process COMMIT mutex, as the branch docs recommend for one process | `src/tx.rs` |
| `unix-excl` VFS, WAL, `synchronous=NORMAL`, 32 MiB page cache, in-memory temp store | `src/db.rs` |
| sqlx linked against our static SQLite through `SQLITE3_LIB_DIR` and a pkg-config shim | `.cargo/config.toml`, `scripts/build-sqlite.sh` |

## Layout

```
├── openapi.json                 # the bank's API, generated and checked by a test
├── PROTOCOL.md                  # the game socket
├── crates/quiz-service/
│   ├── migrations/0001_init.sql # the one baseline schema
│   ├── src/
│   │   ├── auth.rs              # tickets
│   │   ├── rest/                # the bank: handlers, bodies, OpenAPI
│   │   ├── ws.rs                # one task per socket
│   │   ├── game/                # rooms, frames, scoring, spans, registry
│   │   ├── repo/                # quizzes, games, the spans outbox
│   │   ├── retention.rs         # the periods and the sweep
│   │   ├── push.rs              # spans to the LMS server
│   │   ├── snapshot.rs          # VACUUM INTO + atomic rename
│   │   ├── server.rs            # the two routers
│   │   ├── tx.rs                # the BEGIN CONCURRENT engine
│   │   └── bin/mint-ticket.rs   # dev tool
│   └── tests/                   # bank, game (real sockets), ops, openapi
├── crates/bench/                # quiz-bench
├── scripts/build-sqlite.sh      # reproducible custom-SQLite build (pinned commit)
├── observability/               # prometheus + grafana
├── Dockerfile
└── docker-compose.yml           # app + Jaeger + Prometheus + Grafana
```

## Testing

```bash
./scripts/build-sqlite.sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

- Room and game logic runs under `#[tokio::test(start_paused = true)]` against a
  fake store: the deadline, everyone answered, early close, late join,
  reconnect, scoring, partial credit, keys hidden before the reveal, role
  checks, one game per session and per teacher, 500 players, slow sockets, idle
  and room-close endings, spans, forget.
- `tests/bank.rs` drives the REST router: tickets and roles, ownership (another
  owner's quiz is 404), every limit, validation `fields`, whole-quiz saves, ETag
  304, CORS preflight, compression, what the public listener does not serve.
- `tests/game.rs` runs the real server on a port with websocket clients and a
  fake LMS server: a three-question game with a host and players, reconnect,
  origin / subprotocol / hello / ticket rules, socket, size and rate limits, and
  spans (ordering, batches of 500, idempotency, retry after a 503, drop on a
  404, shutdown flush, reconnect bridging).
- `tests/ops.rs` moves the service clock to test expiry (kept vs not kept, use
  moving it, viewing not, keeping switching 7 and 90 days, results expiring at 90
  days while the quiz stays, the startup sweep), forget, the internal listener
  and snapshots (atomic write, `unix-excl`, the gauge advancing when nothing
  changed).

## Benchmark

`quiz-bench` plays a class against a running service over real sockets and
reports the budgets the game is built for:

```bash
export QUIZ_TICKET_KEY=...        # the service's key
ulimit -n 8192                    # a thousand sockets in each process
./target/release/quiz-bench --url http://127.0.0.1:8080 --internal-url http://127.0.0.1:8081 \
    --origin http://localhost:5173 --players 300 --questions 10 --pid <service pid>
```

A game takes at most 500 players, so more are spread over classes of 500 (each
with its own host and game) playing at once. Measured on an arm64 Mac
(10 cores, service and bench on the same machine, loopback, `QUIZ_WORKER_THREADS`
at its default of 2), a typical question (80 characters, four options of 30):

| | 300 players | 1,000 players (2 classes) | budget |
|---|---:|---:|---:|
| received per player per question | 427 B | 427 B | 1,024 B |
| sent per player per question | 13 B | 13 B | 64 B |
| frames received per player per question | 3 | 3 | |
| whole class reached after a question opens (slowest) | 20.2 ms | 18.9 ms | 100 ms |
| last answer to the reveal received everywhere (slowest) | 9.9 ms | 12.8 ms | |
| memory per idle socket | 17.2 KiB | 11.7 KiB | 20 KiB |
| idle RSS | 7.3 MiB | 7.2 MiB | 20 MiB |
| question close (reveal and one write), mean | 33.9 ms | 42.0 ms | |
| service CPU per question (whole class) | 45 ms | 135 ms | |
| write conflicts retried | 0 | 12 | |
| host: one progress frame (counts and provisional top ten) | 254 B | 258 B | |
| host: the one full-names frame after its hello | 4.5 KB (300) | 7.5 KB (500) | |

A 120-character explanation adds about 120 B to what a player receives (539 B).
Websocket permessage-deflate is not used: the websocket library the service
builds on does not implement it, and a deflate context costs 32 KiB or more per
socket (the whole budget is 20 KiB); on frames this size it would save about
a quarter to two thirds of ~250 B, which is not needed to meet the budget.
Encoding a question once instead of per socket costs 0.6 microseconds against
0.6 ms for 1,000 sockets.

## License

MIT.
