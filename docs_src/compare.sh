#!/usr/bin/env bash
# The documentation's examples, checked: translate docs_src/ (three routes are left to Python by
# `--python-side auto`, as the pages say), build the binary, run it next to the FastAPI application on the same
# database, and compare their responses with tests/conformance.py and docs_src/scenario.py.
#
#   DATABASE_URL=postgresql://postgres@127.0.0.1/py2axum_docs docs_src/compare.sh
#
# Needs: PostgreSQL (the database must exist; the lifespan creates the tables), cargo, and a Python with py2axum,
# docs_src/requirements.txt, httpx and websockets. REF_PORT (9150) and CAND_PORT (9190) choose the ports;
# CARGO_TARGET_DIR is honoured; SKIP_BUILD=1 reuses the last binary.
set -euo pipefail
cd "$(dirname "$0")/.."
DB=${DATABASE_URL:-postgresql://postgres@127.0.0.1/py2axum_docs}
DB=${DB/postgresql+psycopg:/postgresql:}
REF_PORT=${REF_PORT:-9150}
CAND_PORT=${CAND_PORT:-9190}
PY=${PYTHON:-python}
LOG=${LOG:-/tmp/docs-src-logs}
CRATE=${CRATE:-generated/docs_axum}
BIN=${CARGO_TARGET_DIR:-$CRATE/target}/release/docs_axum
mkdir -p "$LOG"

if [ -z "${SKIP_BUILD:-}" ]; then
  "$PY" -m py2axum check docs_src --root . --python-side auto
  "$PY" -m py2axum docs_src --root . --python-side auto -o "$CRATE" --name docs_axum
  cargo build --release --quiet --manifest-path "$CRATE/Cargo.toml"
fi

pids=()
stop() { for p in ${pids[@]+"${pids[@]}"}; do kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true; pids=(); }
trap stop EXIT
for port in $REF_PORT $CAND_PORT; do
  if lsof -tiTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then echo "port $port is busy" >&2; exit 1; fi
done

ready() {
  for _ in $(seq 150); do curl -sf -o /dev/null "127.0.0.1:$1/health" && return; sleep 0.1; done
  echo "port $1 not ready, see $LOG" >&2; exit 1
}

start() {  # $1: extra environment for the binary
  # one after the other: both lifespans create the tables
  DATABASE_URL="${DB/postgresql:/postgresql+psycopg:}" "$PY" -m uvicorn docs_src.main:app --port "$REF_PORT" \
    --log-level warning > "$LOG/python.log" 2>&1 &
  pids+=($!)
  ready "$REF_PORT"
  env DATABASE_URL="$DB" PORT="$CAND_PORT" PY2AXUM_PYTHON_URL="http://127.0.0.1:$REF_PORT" $1 "$BIN" \
    > "$LOG/binary.log" 2>&1 &
  pids+=($!)
  ready "$CAND_PORT"
}

compare() {
  DATABASE_URL="$DB" "$PY" tests/conformance.py "http://127.0.0.1:$REF_PORT" "http://127.0.0.1:$CAND_PORT" \
    --scenario docs_src/scenario.py "$@"
}

echo "== normal pass"
start ""
compare
stop

echo "== forced streaming pass"
start "PY2AXUM_STREAM_CHUNK=256 PY2AXUM_STREAM_MIN_ROWS=0"
compare --ignore-encoding
