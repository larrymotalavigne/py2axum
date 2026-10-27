- PyJWT's `PyJWKClient`, `PyJWK` and `PyJWKSet` follow PyJWT 2.13 and 2.14 as well as 2.15 (the locked release):
  2.13 refreshes after every unknown `kid`, follows redirects and lets key-material errors through; 2.14 adds the
  refresh cooldown and raises `InvalidKeyError` for a `ValueError` of the key material; 2.15 for a `TypeError` too,
  skips set members that are not objects and checks the payload before caching it. `fixtures/langapp/jwksx.py`
  checks them against a JWKS endpoint.
