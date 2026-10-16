#!/usr/bin/env bash
# fixtures/dynapp conformance servers: FastAPI reference on :8200, generated binary on :8280 (REF_PORT,
# CAND_PORT).
cd "$(dirname "$0")"
REF_PORT=${REF_PORT:-8200}; CAND_PORT=${CAND_PORT:-8280}
LOG=${LOG:-/tmp/py2axum-logs}; mkdir -p "$LOG"
DB=${DATABASE_URL:?DATABASE_URL required}
# fixtures/dynapp/apiv.py: a router prefix read from the settings, overridden here (read at startup, not frozen)
export DYNAPP_API_PREFIX=/api/v9
for port in $REF_PORT $CAND_PORT; do pid=$(lsof -tiTCP:$port -sTCP:LISTEN); [ -n "$pid" ] && kill $pid; done; sleep 1
# stdout and stderr apart, process ids kept: the log conformance (tests/conformance.py --logs) compares them
DATABASE_URL="$DB" PYTHONUNBUFFERED=1 nohup uvicorn fixtures.dynapp.main:app --host 127.0.0.1 --port $REF_PORT \
  > "$LOG/dyn-py-$REF_PORT.out" 2> "$LOG/dyn-py-$REF_PORT.err" & echo $! > "$LOG/dyn-py-$REF_PORT.pid"
DATABASE_URL="$DB" PY2AXUM_PYTHON_URL=http://127.0.0.1:$REF_PORT nohup env ${RUST_ENV:-} ${BIN:-generated/dynapp_axum/target/release/dynapp_axum} \
  --host 127.0.0.1 --port $CAND_PORT > "$LOG/dyn-rs-$CAND_PORT.out" 2> "$LOG/dyn-rs-$CAND_PORT.err" & echo $! > "$LOG/dyn-rs-$CAND_PORT.pid"
for p in $REF_PORT $CAND_PORT; do
  ok=
  for i in $(seq 150); do curl -s -o /dev/null "127.0.0.1:$p/tasks/0" && ok=1 && break; sleep 0.2; done
  [ -n "$ok" ] || { echo "port $p not ready after 30 s, see $LOG" >&2; exit 1; }
done
echo started
