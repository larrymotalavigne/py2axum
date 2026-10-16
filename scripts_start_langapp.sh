#!/usr/bin/env bash
# fixtures/langapp conformance servers: FastAPI reference on :8540, generated binary on :8590 (REF_PORT, CAND_PORT).
# PostgreSQL at DATABASE_URL (dynapp's tables), Redis at REDIS_URL; POST /dunders/proxied/{name} stays in Python
# (relayed to the reference). Run from the repository root after building generated/langapp_axum.
cd "$(dirname "$0")"
REF_PORT=${REF_PORT:-8540}; CAND_PORT=${CAND_PORT:-8590}
LOG=${LOG:-/tmp/py2axum-logs}; mkdir -p "$LOG"
DB=${DATABASE_URL:?DATABASE_URL required}
for port in $REF_PORT $CAND_PORT; do pid=$(lsof -tiTCP:$port -sTCP:LISTEN); [ -n "$pid" ] && kill $pid; done; sleep 1
# stdout and stderr apart, process ids kept: the log conformance (tests/conformance.py --logs) compares them
DATABASE_URL="$DB" PYTHONUNBUFFERED=1 nohup uvicorn fixtures.langapp.main:app --host 127.0.0.1 --port $REF_PORT \
  > "$LOG/lang-py-$REF_PORT.out" 2> "$LOG/lang-py-$REF_PORT.err" & echo $! > "$LOG/lang-py-$REF_PORT.pid"
DATABASE_URL="$DB" PY2AXUM_PYTHON_URL=http://127.0.0.1:$REF_PORT nohup env ${RUST_ENV:-} ${BIN:-generated/langapp_axum/target/release/langapp_axum} \
  --host 127.0.0.1 --port $CAND_PORT > "$LOG/lang-rs-$CAND_PORT.out" 2> "$LOG/lang-rs-$CAND_PORT.err" & echo $! > "$LOG/lang-rs-$CAND_PORT.pid"
for p in $REF_PORT $CAND_PORT; do
  ok=
  for i in $(seq 150); do curl -s -o /dev/null "127.0.0.1:$p/health" && ok=1 && break; sleep 0.2; done
  [ -n "$ok" ] || { echo "port $p not ready after 30 s, see $LOG" >&2; exit 1; }
done
echo started
