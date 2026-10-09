#!/usr/bin/env bash
# Translate the bookshelf example, build the binary, run it next to the FastAPI application on the same
# database, and compare their responses (tests/conformance.py, twice: normal, then list streaming forced).
# The one route py2axum does not translate (GET /books/export.zip, `zipfile`) is left to Python by
# `--python-side auto`: the binary relays it to the FastAPI server (PY2AXUM_PYTHON_URL), as in production.
#
#   DATABASE_URL=postgresql://postgres@127.0.0.1/bookshelf examples/bookshelf/compare.sh
#
# Needs: PostgreSQL (the database must exist; its tables are created from schema.sql), cargo, and a Python with
# py2axum and examples/bookshelf/requirements.txt installed (plus httpx and websockets for the harness).
# REF_PORT (9050) and CAND_PORT (9090) choose the ports; CARGO_TARGET_DIR is honoured; SKIP_BUILD=1 reuses
# the last binary.
set -euo pipefail
cd "$(dirname "$0")/../.."
DB=${DATABASE_URL:-postgresql://postgres@127.0.0.1/bookshelf}
DB=${DB/postgresql+psycopg:/postgresql:}
REF_PORT=${REF_PORT:-9050}
CAND_PORT=${CAND_PORT:-9090}
PY=${PYTHON:-python}
LOG=${LOG:-/tmp/bookshelf-logs}
CRATE=generated/bookshelf
BIN=${CARGO_TARGET_DIR:-$CRATE/target}/release/bookshelf
mkdir -p "$LOG"

if [ -z "${SKIP_BUILD:-}" ]; then
  "$PY" -m py2axum check examples/bookshelf/app --root examples/bookshelf --python-side auto
  "$PY" -m py2axum examples/bookshelf/app --root examples/bookshelf --python-side auto -o "$CRATE" --name bookshelf
  cargo build --release --quiet --manifest-path "$CRATE/Cargo.toml"
fi
PGOPTIONS=--client-min-messages=warning psql -q -v ON_ERROR_STOP=1 "$DB" -f examples/bookshelf/schema.sql

pids=()
stop() { for p in ${pids[@]+"${pids[@]}"}; do kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true; pids=(); }
trap stop EXIT
for port in $REF_PORT $CAND_PORT; do
  if lsof -tiTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then echo "port $port is busy" >&2; exit 1; fi
done

start() {  # $1: extra environment for the binary
  # a low bcrypt cost keeps registrations fast; both servers share the default token secret
  (cd examples/bookshelf && DATABASE_URL="${DB/postgresql:/postgresql+psycopg:}" BOOKSHELF_BCRYPT_ROUNDS=4 \
    exec "$PY" -m uvicorn app.main:app --port "$REF_PORT" --log-level warning) > "$LOG/python.log" 2>&1 &
  pids+=($!)
  env DATABASE_URL="$DB" PORT="$CAND_PORT" PY2AXUM_PYTHON_URL="http://127.0.0.1:$REF_PORT" BOOKSHELF_BCRYPT_ROUNDS=4 \
    $1 "$BIN" > "$LOG/binary.log" 2>&1 &
  pids+=($!)
  for port in $REF_PORT $CAND_PORT; do
    for _ in $(seq 100); do curl -sf -o /dev/null "127.0.0.1:$port/health" && continue 2; sleep 0.1; done
    echo "port $port not ready, see $LOG" >&2; exit 1
  done
}

compare() {
  DATABASE_URL="$DB" "$PY" tests/conformance.py "http://127.0.0.1:$REF_PORT" "http://127.0.0.1:$CAND_PORT" \
    --scenario examples/bookshelf/scenario.py "$@"
}

echo "== normal pass"
start ""
compare
stop

# lists streamed from the session whenever the binary can (docs/advanced/streaming.md), in
# blocks of 256 bytes: same bodies, but a streamed response has no content-length, so GZipMiddleware's
# minimum size no longer applies (--ignore-encoding)
echo "== forced streaming pass"
start "PY2AXUM_STREAM_CHUNK=256 PY2AXUM_STREAM_MIN_ROWS=0"
compare --ignore-encoding
