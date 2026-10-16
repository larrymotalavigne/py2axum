#!/usr/bin/env bash
# fixtures/libsrcapp conformance servers: FastAPI reference on :8568, generated binary on :8599 (REF_PORT, CAND_PORT).
# No database. Run from the repository root after building generated/libsrcapp_axum
# (python -m py2axum fixtures/libsrcapp --root . --translate-libs auto ...).
cd "$(dirname "$0")"
REF_PORT=${REF_PORT:-8568}; CAND_PORT=${CAND_PORT:-8599}
LOG=${LOG:-/tmp/py2axum-logs}; mkdir -p "$LOG"
for port in $REF_PORT $CAND_PORT; do pid=$(lsof -tiTCP:$port -sTCP:LISTEN); [ -n "$pid" ] && kill $pid; done; sleep 1
# stdout and stderr apart, process ids kept: the log conformance (tests/conformance.py --logs) compares them
PYTHONUNBUFFERED=1 nohup uvicorn fixtures.libsrcapp.main:app --host 127.0.0.1 --port $REF_PORT \
  > "$LOG/lsrc-py-$REF_PORT.out" 2> "$LOG/lsrc-py-$REF_PORT.err" & echo $! > "$LOG/lsrc-py-$REF_PORT.pid"
nohup env ${RUST_ENV:-} ${BIN:-generated/libsrcapp_axum/target/release/libsrcapp_axum} \
  --host 127.0.0.1 --port $CAND_PORT > "$LOG/lsrc-rs-$CAND_PORT.out" 2> "$LOG/lsrc-rs-$CAND_PORT.err" & echo $! > "$LOG/lsrc-rs-$CAND_PORT.pid"
for p in $REF_PORT $CAND_PORT; do
  ok=
  for i in $(seq 150); do curl -s -o /dev/null "127.0.0.1:$p/health" && ok=1 && break; sleep 0.2; done
  [ -n "$ok" ] || { echo "port $p not ready after 30 s, see $LOG" >&2; exit 1; }
done
echo started
