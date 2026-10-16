#!/usr/bin/env bash
# fixtures/sentryapp: fake Sentry server :8799 (tests/sentry_sink.py), FastAPI :8710 reporting as project 1,
# the generated binary :8790 as project 2 (REF_PORT, CAND_PORT, SINK_PORT). SENTRY_PII=1 for send_default_pii.
# The binary's logging level follows Python's root default (WARNING), see PY2AXUM_LOG_LEVEL.
cd "$(dirname "$0")"
REF_PORT=${REF_PORT:-8710}; CAND_PORT=${CAND_PORT:-8790}; SINK_PORT=${SINK_PORT:-8799}
LOG=${LOG:-/tmp/py2axum-logs}; mkdir -p "$LOG"
for port in $REF_PORT $CAND_PORT $SINK_PORT; do pid=$(lsof -tiTCP:$port -sTCP:LISTEN); [ -n "$pid" ] && kill $pid; done; sleep 1
nohup python tests/sentry_sink.py $SINK_PORT > "$LOG/sentry-sink.log" 2>&1 &
# BIN, else built in its own target dir, or in dynapp's
if [ -z "${BIN:-}" ]; then
  BIN=generated/sentryapp_axum/target/release/sentryapp_axum
  [ -x "$BIN" ] || BIN=generated/dynapp_axum/target/release/sentryapp_axum
fi
export GIT_SHA=0123456789 SENTRY_PII=${SENTRY_PII:-0}
SENTRY_DSN=http://pk@127.0.0.1:$SINK_PORT/1 nohup uvicorn fixtures.sentryapp.main:app --port $REF_PORT --log-level warning > "$LOG/sentry-py.log" 2>&1 &
SENTRY_DSN=http://pk@127.0.0.1:$SINK_PORT/2 PY2AXUM_LOG_LEVEL=WARNING PORT=$CAND_PORT nohup env ${RUST_ENV:-} "$BIN" > "$LOG/sentry-rs.log" 2>&1 &
for p in $REF_PORT $CAND_PORT; do
  ok=
  for i in $(seq 150); do curl -s -o /dev/null "127.0.0.1:$p/health" && ok=1 && break; sleep 0.2; done
  [ -n "$ok" ] || { echo "port $p not ready after 30 s, see $LOG" >&2; exit 1; }
done
echo started
