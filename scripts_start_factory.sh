#!/usr/bin/env bash
# fixtures/factoryapp: FastAPI on :8300, the binary on :8380, each in prometheus_client's multiprocess mode
# with its own (emptied) PROMETHEUS_MULTIPROC_DIR. Run from the repository root, after building
# generated/factoryapp_axum (CARGO_TARGET_DIR=generated/dynapp_axum/target).
set -e
for port in 8300 8380; do pid=$(lsof -tiTCP:$port -sTCP:LISTEN || true); [ -n "$pid" ] && kill $pid; done
sleep 1
D=${TMPDIR:-/tmp}/py2axum-prom
rm -rf "$D-py" "$D-rs" && mkdir -p "$D-py" "$D-rs" logs
(PROMETHEUS_MULTIPROC_DIR="$D-py" nohup uvicorn fixtures.factoryapp.main:app --port 8300 --log-level warning > logs/fac-py.log 2>&1 &)
(PROMETHEUS_MULTIPROC_DIR="$D-rs" PORT=8380 nohup generated/dynapp_axum/target/release/factoryapp_axum > logs/fac-rs.log 2>&1 &)
for i in $(seq 60); do curl -s -o /dev/null 127.0.0.1:8300/health && curl -s -o /dev/null 127.0.0.1:8380/health && break; sleep 0.5; done
