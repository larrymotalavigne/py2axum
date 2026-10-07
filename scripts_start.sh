#!/usr/bin/env bash
# Start both servers in the background (FastAPI :8000, generated axum :8080).
cd "$(dirname "$0")"
LOG=${LOG:-/tmp/py2axum-logs}; mkdir -p "$LOG"
pkill -x app_axum 2>/dev/null; pkill -f "[u]vicorn app.main" 2>/dev/null; sleep 1
UPSTREAM_URL=http://127.0.0.1:8000 nohup uvicorn app.main:app --port 8000 --log-level warning ${UVICORN_ARGS} > "$LOG/py.log" 2>&1 &
UPSTREAM_URL=http://127.0.0.1:8080 PORT=8080 nohup env ${RUST_ENV} generated/app_axum/target/release/app_axum > "$LOG/rs.log" 2>&1 &
for p in 8000 8080; do
  ok=
  for i in $(seq 150); do curl -sf "127.0.0.1:$p/health" >/dev/null && ok=1 && break; sleep 0.2; done
  [ -n "$ok" ] || { echo "port $p not ready after 30 s, see $LOG" >&2; exit 1; }
done
echo started
