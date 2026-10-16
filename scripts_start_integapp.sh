#!/usr/bin/env bash
# fixtures/integapp conformance servers: FastAPI reference on :8550, generated binary on :8595 (REF_PORT, CAND_PORT;
# the push sink stays on :8299, shared). PostgreSQL at DATABASE_URL (dynapp's tables), RabbitMQ at BROKER_DSN. The
# routers of fixtures/extlib (a library on the PYTHONPATH) stay in Python: relayed to the reference (--python-side mount).
# Run from the repository root after building generated/integapp_axum.
cd "$(dirname "$0")"
REF_PORT=${REF_PORT:-8550}; CAND_PORT=${CAND_PORT:-8595}
LOG=${LOG:-/tmp/py2axum-logs}; mkdir -p "$LOG"
DB=${DATABASE_URL:?DATABASE_URL required}
# fixtures/integapp/libs.py /none-settings: env_parse_none_str, case_sensitive
export DYNAPP_NONE_INT=none DYNAPP_NONE_STR=none DYNAPP_NONE_LIST=none DYNAPP_NONE_CASE=None dynapp_lower=lo \
  DYNAPP_NONE_REQ=none DYNAPP_NONE_DICT='{"a": 1}' \
  DYNAPP_RAW='not json' DYNAPP_JSON='[1, 2]'
for port in $REF_PORT $CAND_PORT; do pid=$(lsof -tiTCP:$port -sTCP:LISTEN); [ -n "$pid" ] && kill $pid; done; sleep 1
lsof -tiTCP:8299 -sTCP:LISTEN >/dev/null || {
# web push sink (tests/push_sink.py): pywebpush blocks the server that calls it, so its own process
nohup uvicorn tests.push_sink:app --port 8299 --log-level warning > "$LOG/push-sink.log" 2>&1 & }
# stdout and stderr apart, process ids kept: the log conformance (tests/conformance.py --logs) compares them
DATABASE_URL="$DB" PYTHONPATH=fixtures/extlib${PYTHONPATH:+:$PYTHONPATH} PYTHONUNBUFFERED=1 nohup uvicorn fixtures.integapp.main:app --host 127.0.0.1 --port $REF_PORT \
  > "$LOG/integ-py-$REF_PORT.out" 2> "$LOG/integ-py-$REF_PORT.err" & echo $! > "$LOG/integ-py-$REF_PORT.pid"
DATABASE_URL="$DB" PY2AXUM_PYTHON_URL=http://127.0.0.1:$REF_PORT nohup env ${RUST_ENV:-} ${BIN:-generated/integapp_axum/target/release/integapp_axum} \
  --host 127.0.0.1 --port $CAND_PORT > "$LOG/integ-rs-$CAND_PORT.out" 2> "$LOG/integ-rs-$CAND_PORT.err" & echo $! > "$LOG/integ-rs-$CAND_PORT.pid"
for p in $REF_PORT $CAND_PORT 8299; do
  ok=
  for i in $(seq 150); do curl -s -o /dev/null "127.0.0.1:$p/health" && ok=1 && break; sleep 0.2; done
  [ -n "$ok" ] || { echo "port $p not ready after 30 s, see $LOG" >&2; exit 1; }
done
echo started
