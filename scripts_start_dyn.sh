#!/usr/bin/env bash
# fixtures/dynapp conformance servers: FastAPI reference on :8200, generated binary on :8280.
cd "$(dirname "$0")"
LOG=${LOG:-/tmp/py2axum-logs}; mkdir -p "$LOG"
DB=${DATABASE_URL:?DATABASE_URL required}
for port in 8200 8280 8299; do pid=$(lsof -tiTCP:$port -sTCP:LISTEN); [ -n "$pid" ] && kill $pid; done; sleep 1
# web push sink (tests/push_sink.py): pywebpush blocks the server that calls it, so its own process
nohup uvicorn tests.push_sink:app --port 8299 --log-level warning > "$LOG/push-sink.log" 2>&1 &
DATABASE_URL="$DB" nohup uvicorn fixtures.dynapp.main:app --port 8200 --log-level warning > "$LOG/dyn-py.log" 2>&1 &
DATABASE_URL="$DB" PORT=8280 PY2AXUM_PYTHON_URL=http://127.0.0.1:8200 nohup generated/dynapp_axum/target/release/dynapp_axum > "$LOG/dyn-rs.log" 2>&1 &
for p in 8200 8280 8299; do
  ok=
  for i in $(seq 150); do curl -s -o /dev/null "127.0.0.1:$p/tasks/0" && ok=1 && break; sleep 0.2; done
  [ -n "$ok" ] || { echo "port $p not ready after 30 s, see $LOG" >&2; exit 1; }
done
echo started
