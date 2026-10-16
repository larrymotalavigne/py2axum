#!/usr/bin/env bash
# fixtures/libapp conformance servers: FastAPI reference on :8530, generated binary on :8580 (REF_PORT, CAND_PORT).
# No database; RabbitMQ at BROKER_DSN; the DNS stub of fixtures/libapp/dnsx.py (tests/dns_stub.py) on :8531 (DNS_STUB_PORT,
# UDP and TCP), and a UDP port nobody answers on, :8532 (DNS_DEAD_PORT). Run from the repository root after building
# generated/libapp_axum.
cd "$(dirname "$0")"
REF_PORT=${REF_PORT:-8530}; CAND_PORT=${CAND_PORT:-8580}
export DNS_STUB_PORT=${DNS_STUB_PORT:-8531} DNS_DEAD_PORT=${DNS_DEAD_PORT:-8532}
LOG=${LOG:-/tmp/py2axum-logs}; mkdir -p "$LOG"
for port in $REF_PORT $CAND_PORT $DNS_STUB_PORT; do pid=$(lsof -tiTCP:$port -sTCP:LISTEN); [ -n "$pid" ] && kill $pid; done; sleep 1
nohup python tests/dns_stub.py $DNS_STUB_PORT > "$LOG/lib-dns-$DNS_STUB_PORT.log" 2>&1 &
# stdout and stderr apart, process ids kept: the log conformance (tests/conformance.py --logs) compares them
PYTHONUNBUFFERED=1 nohup uvicorn fixtures.libapp.main:app --host 127.0.0.1 --port $REF_PORT \
  > "$LOG/lib-py-$REF_PORT.out" 2> "$LOG/lib-py-$REF_PORT.err" & echo $! > "$LOG/lib-py-$REF_PORT.pid"
nohup env ${RUST_ENV:-} ${BIN:-generated/libapp_axum/target/release/libapp_axum} \
  --host 127.0.0.1 --port $CAND_PORT > "$LOG/lib-rs-$CAND_PORT.out" 2> "$LOG/lib-rs-$CAND_PORT.err" & echo $! > "$LOG/lib-rs-$CAND_PORT.pid"
for p in $REF_PORT $CAND_PORT; do
  ok=
  for i in $(seq 150); do curl -s -o /dev/null "127.0.0.1:$p/health" && ok=1 && break; sleep 0.2; done
  [ -n "$ok" ] || { echo "port $p not ready after 30 s, see $LOG" >&2; exit 1; }
done
echo started
