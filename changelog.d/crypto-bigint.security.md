- `p256` 0.14 (`crypto-bigint` 0.7.10, RUSTSEC-2026-0387: timing variability in operations meant to be constant-time).
  The runtime uses P-256 for VAPID signatures and ECDH in `pywebpush.webpush` and for ES256 Google ID token verification.
