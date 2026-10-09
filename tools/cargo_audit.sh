#!/usr/bin/env sh
# RustSec audit of the lock file shipped in every generated crate (py2axum/runtime/Cargo.lock).
# Exits non-zero on any advisory not listed below. Each ignored advisory is justified in docs/advanced/security.md
# ("Rust dependencies"); remove the line as soon as a fixed version exists.
#   RUSTSEC-2023-0071 rsa (Marvin timing attack on private-key DECRYPTION): the runtime only generates keys
#   and verifies PKCS#1 v1.5 signatures with public keys; no RSA decryption or signing is reachable.
set -eu
cd "$(dirname "$0")/.."
exec cargo audit --file py2axum/runtime/Cargo.lock \
  --ignore RUSTSEC-2023-0071 \
  "$@"
