#!/usr/bin/env bash
# fixtures/syncapp conformance servers: FastAPI reference on :8400, generated binary on :8480 (BIN: the binary).
cd "$(dirname "$0")"
LOG=${LOG:-/tmp/py2axum-logs}; mkdir -p "$LOG"
DB=${DATABASE_URL:?DATABASE_URL required}
for port in 8400 8480; do pid=$(lsof -tiTCP:$port -sTCP:LISTEN); [ -n "$pid" ] && kill $pid; done; sleep 1
DATABASE_URL="$DB" nohup uvicorn fixtures.syncapp.main:app --port 8400 --log-level warning > "$LOG/sync-py.log" 2>&1 &
DATABASE_URL="$DB" PORT=8480 nohup ${BIN:-generated/dynapp_axum/target/release/syncapp_axum} > "$LOG/sync-rs.log" 2>&1 &
for p in 8400 8480; do
  ok=
  for i in $(seq 150); do curl -s -o /dev/null "127.0.0.1:$p/health" && ok=1 && break; sleep 0.2; done
  [ -n "$ok" ] || { echo "port $p not ready after 30 s, see $LOG" >&2; exit 1; }
done
echo started
