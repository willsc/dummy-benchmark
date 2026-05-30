#!/usr/bin/env bash
# Spin up the whole pipeline end-to-end for a short demo run.
#
#   scripts/run-demo.sh                  # builds (release) and runs ~10s
#   DURATION=30 scripts/run-demo.sh      # custom run length
#
# The bus file is wiped first so producers/consumers start clean.

set -euo pipefail

DURATION="${DURATION:-10}"
BUS="${BUS:-/tmp/shmbus.bin}"
BIND="${BIND:-127.0.0.1:9001}"
RATE="${RATE:-2000}"

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "[demo] building release binaries..."
cargo build --release --workspace

rm -f "$BUS"

echo "[demo] starting feedhandler..."
./target/release/feedhandler --bind "$BIND" --bus "$BUS" &
FH_PID=$!

# Let the feedhandler initialise the bus before the consumer attaches.
sleep 0.3

echo "[demo] starting trading-engine..."
./target/release/trading-engine --bus "$BUS" &
TE_PID=$!

sleep 0.2

echo "[demo] starting mock-exchange (target=$BIND, rate=$RATE/sym)..."
./target/release/mock-exchange --target "$BIND" --rate "$RATE" &
MX_PID=$!

cleanup() {
    echo
    echo "[demo] tearing down..."
    kill "$MX_PID" "$TE_PID" "$FH_PID" 2>/dev/null || true
    wait "$MX_PID" "$TE_PID" "$FH_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

echo "[demo] running for ${DURATION}s..."
sleep "$DURATION"
