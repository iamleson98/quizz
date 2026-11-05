#!/usr/bin/env bash
# Compare SQLite write-transaction strategies of quiz-service:
#   concurrent  - BEGIN CONCURRENT (begin-concurrent branch)
#   immediate   - BEGIN IMMEDIATE (classic WAL serialized writer)
#
# Runs the write scenario against both modes on a fresh database each and
# prints the submitAttempt rows side by side.
set -euo pipefail

cd "$(dirname "$0")/.."

URL="http://127.0.0.1:8099"
DURATION="${1:-20}"
CON="${2:-8}"
QUIZZES="${3:-8}"   # quizzes to spread writes across

run_mode() {
    local mode="$1" db
    db="$(mktemp -u /tmp/quiz-cmp-XXXX).db"
    echo "---------------- TX MODE: $mode ----------------"
    QUIZ_DATABASE="$db" QUIZ_TX_MODE="$mode" QUIZ_LISTEN_ADDR="127.0.0.1:8099" \
        RUST_LOG=warn ./target/release/quiz-service > /tmp/quiz-cmp-$mode.log 2>&1 &
    local pid=$!
    sleep 1
    ./target/release/quiz-bench --url "$URL" --scenario write \
        --concurrency "$CON" --duration "$DURATION" --quizzes "$QUIZZES" 2>/dev/null \
        | grep -E "requests|errors|submitAttempt|startAttempt"
    echo "tx retries: $(curl -s "$URL/metrics" | awk '/^tx_retries_total/ {print $2}')"
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null || true
    rm -f "$db"*
}

cargo build --release -p quiz-service -p bench >/dev/null

run_mode concurrent
echo
run_mode immediate

echo
echo "Tip: increase --quizzes to spread contention further (fewer page conflicts),"
echo "or set it to 1 for the adversarial single-hotspot profile."
