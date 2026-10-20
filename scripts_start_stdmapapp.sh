#!/usr/bin/env bash
# fixtures/stdmapapp conformance servers: FastAPI reference on :8572, generated binary on :8573 (REF_PORT, CAND_PORT).
# No database. Run from the repository root after building generated/stdmapapp_axum
# (python -m py2axum fixtures/stdmapapp --root . ...).
cd "$(dirname "$0")"
REF_PORT=${REF_PORT:-8572}; CAND_PORT=${CAND_PORT:-8573}
LOG=${LOG:-/tmp/py2axum-logs}; mkdir -p "$LOG"
for port in $REF_PORT $CAND_PORT; do pid=$(lsof -tiTCP:$port -sTCP:LISTEN); [ -n "$pid" ] && kill $pid; done; sleep 1
# stdout and stderr apart, process ids kept: the log conformance (tests/conformance.py --logs) compares them
PYTHONUNBUFFERED=1 nohup uvicorn fixtures.stdmapapp.main:app --host 127.0.0.1 --port $REF_PORT \
  > "$LOG/stdm-py-$REF_PORT.out" 2> "$LOG/stdm-py-$REF_PORT.err" & echo $! > "$LOG/stdm-py-$REF_PORT.pid"
nohup env ${RUST_ENV:-} ${BIN:-generated/stdmapapp_axum/target/release/stdmapapp_axum} \
  --host 127.0.0.1 --port $CAND_PORT > "$LOG/stdm-rs-$CAND_PORT.out" 2> "$LOG/stdm-rs-$CAND_PORT.err" & echo $! > "$LOG/stdm-rs-$CAND_PORT.pid"
for p in $REF_PORT $CAND_PORT; do
  ok=
  for i in $(seq 150); do curl -s -o /dev/null "127.0.0.1:$p/health" && ok=1 && break; sleep 0.2; done
  [ -n "$ok" ] || { echo "port $p not ready after 30 s, see $LOG" >&2; exit 1; }
done
echo started
