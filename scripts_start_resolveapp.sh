#!/usr/bin/env bash
# fixtures/resolveapp conformance servers: FastAPI reference on :8520, generated binary on :8525 (REF_PORT, CAND_PORT).
# No database. Run from the repository root after building generated/resolveapp_axum.
cd "$(dirname "$0")"
REF_PORT=${REF_PORT:-8520}; CAND_PORT=${CAND_PORT:-8525}
LOG=${LOG:-/tmp/py2axum-logs}; mkdir -p "$LOG"
for port in $REF_PORT $CAND_PORT; do pid=$(lsof -tiTCP:$port -sTCP:LISTEN); [ -n "$pid" ] && kill $pid; done; sleep 1
# stdout and stderr apart, process ids kept: the log conformance (tests/conformance.py --logs) compares them
PYTHONUNBUFFERED=1 nohup uvicorn fixtures.resolveapp.main:app --host 127.0.0.1 --port $REF_PORT \
  > "$LOG/resolve-py-$REF_PORT.out" 2> "$LOG/resolve-py-$REF_PORT.err" & echo $! > "$LOG/resolve-py-$REF_PORT.pid"
nohup env ${RUST_ENV:-} ${BIN:-generated/resolveapp_axum/target/release/resolveapp_axum} \
  --host 127.0.0.1 --port $CAND_PORT > "$LOG/resolve-rs-$CAND_PORT.out" 2> "$LOG/resolve-rs-$CAND_PORT.err" & echo $! > "$LOG/resolve-rs-$CAND_PORT.pid"
for p in $REF_PORT $CAND_PORT; do
  ok=
  for i in $(seq 150); do curl -s -o /dev/null "127.0.0.1:$p/health" && ok=1 && break; sleep 0.2; done
  [ -n "$ok" ] || { echo "port $p not ready after 30 s, see $LOG" >&2; exit 1; }
done
echo started
