#!/usr/bin/env bash
# Load-test recipes for quiz-service using oha (https://github.com/hatoo/oha)
# and wrk. Install with: cargo install oha  /  apt|brew install wrk
#
# These exercise the READ path (no auth needed). For authenticated write
# load tests use the bundled Rust loadgen:
#   cargo run --release -p bench -- --scenario write --concurrency 16 --duration 30
set -euo pipefail

URL="${1:-http://127.0.0.1:8080}"
DURATION="${2:-30s}"
CON="${3:-50}"

GQL='{"query":"{ quizzes(first: 20) { total items { id title questionCount creator { username } } } }"}'
LB='{"query":"{ __typename }"}'

echo "==> target: $URL  duration: $DURATION  connections: $CON"
curl -sf "$URL/healthz" && echo

echo
echo "==> [1/4] healthz (pure HTTP)"
oha -z "$DURATION" -c "$CON" --no-tui "$URL/healthz"

echo
echo "==> [2/4] GraphQL quiz list"
oha -z "$DURATION" -c "$CON" --no-tui \
    -m POST -H 'content-type: application/json' -d "$GQL" "$URL/graphql"

echo
echo "==> [3/4] GraphQL introspection-ish query"
oha -z "$DURATION" -c "$CON" --no-tui \
    -m POST -H 'content-type: application/json' -d "$LB" "$URL/graphql"

echo
echo "==> [4/4] wrk on healthz (for comparison with other stacks)"
if command -v wrk >/dev/null; then
    wrk -t"$(nproc)" -c"$CON" -d"${DURATION%s}s" "$URL/healthz"
else
    echo "wrk not installed, skipping"
fi

echo
echo "Done. For WRITE benchmarks (startAttempt/submitAttempt under"
echo "BEGIN CONCURRENT contention) use: cargo run --release -p bench"
