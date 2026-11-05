#!/usr/bin/env bash
# Full HTTP load-test suite for quiz-service.
#
# Usage: run-http-suite.sh <tx-mode> <tag> [suite]
#   tx-mode : concurrent | immediate
#   tag     : label for result files (results-<tag>/)
#   suite   : full (read+write+mixed) | writes (write+mixed only)  [default full]
#
# Everything (server + clients) runs inside this single invocation so the
# sandbox cannot reap the server between commands.
set -euo pipefail
MODE=${1:?usage: run-http-suite.sh <tx-mode> <tag> [full|writes]}
TAG=${2:?usage: run-http-suite.sh <tx-mode> <tag> [full|writes]}
SUITE=${3:-full}
DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$DIR/.."
DB="$ROOT/loadtest/perf-$TAG.db"
LOG="$ROOT/loadtest/server-$TAG.log"
OUT="$ROOT/loadtest/results-$TAG"

rm -f "$DB" "$DB-wal" "$DB-shm" "$LOG"
mkdir -p "$OUT"

QUIZ_DATABASE="$DB" QUIZ_JWT_SECRET=loadtest-secret-0192 QUIZ_SEED_DEMO=1 \
QUIZ_LISTEN_ADDR=127.0.0.1:8080 QUIZ_TX_MODE="$MODE" RUST_LOG=warn \
  "$ROOT/bin/quiz-service" > "$LOG" 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT

for _ in $(seq 1 60); do
  if curl -s -m 2 http://127.0.0.1:8080/graphql -H 'content-type: application/json' \
       -d '{"query":"{ sqliteInfo }"}' 2>/dev/null | grep -q sqliteInfo; then
    break
  fi
  sleep 0.25
done
echo "--- server ready (pid $SRV, tx mode: $MODE)"
curl -s http://127.0.0.1:8080/graphql -H 'content-type: application/json' \
  -d '{"query":"{ sqliteInfo }"}'; echo

python3 "$DIR/http-loadtest.py" setup --users 24 --quizzes 12

metrics() { curl -s http://127.0.0.1:8080/metrics | grep '^tx_' || true; }
metrics > "$OUT/metrics-before.txt"

if [ "$SUITE" = "full" ]; then
  for vus in 16 64 128 256; do
    python3 "$DIR/http-loadtest.py" run --scenario read --vus "$vus" --duration 12 \
      > "$OUT/read-$vus.json"
  done
fi
python3 "$DIR/http-loadtest.py" run --scenario write --vus 16 --duration 25 > "$OUT/write-16.json"
python3 "$DIR/http-loadtest.py" run --scenario write --vus 48 --duration 25 > "$OUT/write-48.json"
python3 "$DIR/http-loadtest.py" run --scenario mixed --vus 48 --duration 25 > "$OUT/mixed-48.json"

metrics > "$OUT/metrics-after.txt"
echo "server_rss_kb=$(ps -o rss= -p "$SRV" | tr -d ' ')" > "$OUT/server.txt"
echo "--- tx metrics after suite:"
cat "$OUT/metrics-after.txt"

kill "$SRV" 2>/dev/null || true
trap - EXIT
wait "$SRV" 2>/dev/null || true
echo "SUITE $TAG DONE"
