- PyJWT 2.13 and 2.14 as well as 2.15 (the locked release decides). `jwt.decode`: 2.13 reads the segments with
  the lenient `base64url_decode`, 2.14 refuses any `=`, 2.15 accepts two trailing; before 2.15 a claim `int()`
  fails on raises its own `TypeError`/`OverflowError`; 2.13 writes the `verify_*` defaults into the caller's
  `options`. `PyJWKClient`/`PyJWK`/`PyJWKSet`: 2.13 refreshes after every unknown `kid`, follows redirects and lets
  key-material errors through; 2.14 adds the refresh cooldown, refuses an empty HMAC key and raises
  `InvalidKeyError` for a `ValueError` of the key material; 2.15 for a `TypeError` too, skips set members that are
  not objects and checks the payload before caching it. JWK numbers are decoded as PyJWT does (characters outside
  the alphabet skipped) and checked with cryptography's messages (an even modulus is accepted).
  `fixtures/langapp/jwksx.py` checks them against a JWKS endpoint.
