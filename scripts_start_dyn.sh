#!/usr/bin/env bash
# fixtures/dynapp conformance servers: FastAPI reference on :8200, generated binary on :8280 (REF_PORT,
# CAND_PORT; the push sink stays on :8299, shared).
cd "$(dirname "$0")"
REF_PORT=${REF_PORT:-8200}; CAND_PORT=${CAND_PORT:-8280}
LOG=${LOG:-/tmp/py2axum-logs}; mkdir -p "$LOG"
DB=${DATABASE_URL:?DATABASE_URL required}
# fixtures/dynapp/apiv.py: a router prefix read from the settings, overridden here (read at startup, not frozen)
export DYNAPP_API_PREFIX=/api/v9
# fixtures/dynapp/libs.py /none-settings: env_parse_none_str, case_sensitive
export DYNAPP_NONE_INT=none DYNAPP_NONE_STR=none DYNAPP_NONE_LIST=none DYNAPP_NONE_CASE=None dynapp_lower=lo \
  DYNAPP_NONE_REQ=none DYNAPP_NONE_DICT='{"a": 1}' \
  DYNAPP_RAW='not json' DYNAPP_JSON='[1, 2]'
for port in $REF_PORT $CAND_PORT; do pid=$(lsof -tiTCP:$port -sTCP:LISTEN); [ -n "$pid" ] && kill $pid; done; sleep 1
lsof -tiTCP:8299 -sTCP:LISTEN >/dev/null || {
# web push sink (tests/push_sink.py): pywebpush blocks the server that calls it, so its own process
nohup uvicorn tests.push_sink:app --port 8299 --log-level warning > "$LOG/push-sink.log" 2>&1 & }
DATABASE_URL="$DB" nohup uvicorn fixtures.dynapp.main:app --port $REF_PORT --log-level warning > "$LOG/dyn-py-$REF_PORT.log" 2>&1 &
DATABASE_URL="$DB" PORT=$CAND_PORT PY2AXUM_PYTHON_URL=http://127.0.0.1:$REF_PORT nohup env ${RUST_ENV:-} generated/dynapp_axum/target/release/dynapp_axum > "$LOG/dyn-rs-$CAND_PORT.log" 2>&1 &
for p in $REF_PORT $CAND_PORT 8299; do
  ok=
  for i in $(seq 150); do curl -s -o /dev/null "127.0.0.1:$p/tasks/0" && ok=1 && break; sleep 0.2; done
  [ -n "$ok" ] || { echo "port $p not ready after 30 s, see $LOG" >&2; exit 1; }
done
echo started
