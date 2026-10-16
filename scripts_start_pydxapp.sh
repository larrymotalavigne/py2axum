#!/usr/bin/env bash
# fixtures/pydxapp conformance servers: FastAPI reference on :8560, generated binary on :8596 (REF_PORT, CAND_PORT).
# No database. Run from the repository root after building generated/pydxapp_axum.
cd "$(dirname "$0")"
REF_PORT=${REF_PORT:-8560}; CAND_PORT=${CAND_PORT:-8596}
LOG=${LOG:-/tmp/py2axum-logs}; mkdir -p "$LOG"
for port in $REF_PORT $CAND_PORT; do pid=$(lsof -tiTCP:$port -sTCP:LISTEN); [ -n "$pid" ] && kill $pid; done; sleep 1
# stdout and stderr apart, process ids kept: the log conformance (tests/conformance.py --logs) compares them
PYTHONUNBUFFERED=1 nohup uvicorn fixtures.pydxapp.main:app --host 127.0.0.1 --port $REF_PORT \
  > "$LOG/pydx-py-$REF_PORT.out" 2> "$LOG/pydx-py-$REF_PORT.err" & echo $! > "$LOG/pydx-py-$REF_PORT.pid"
nohup env ${RUST_ENV:-} ${BIN:-generated/pydxapp_axum/target/release/pydxapp_axum} \
  --host 127.0.0.1 --port $CAND_PORT > "$LOG/pydx-rs-$CAND_PORT.out" 2> "$LOG/pydx-rs-$CAND_PORT.err" & echo $! > "$LOG/pydx-rs-$CAND_PORT.pid"
for p in $REF_PORT $CAND_PORT; do
  ok=
  for i in $(seq 150); do curl -s -o /dev/null "127.0.0.1:$p/health" && ok=1 && break; sleep 0.2; done
  [ -n "$ok" ] || { echo "port $p not ready after 30 s, see $LOG" >&2; exit 1; }
done
echo started
