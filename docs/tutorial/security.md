# Security

FastAPI's security utilities (`OAuth2PasswordBearer`, `HTTPBearer`, `HTTPBasic`) are dependencies that read
credentials from the request and answer 401 when they are missing. The binary reproduces those schemes with
the behaviour of the FastAPI version it is tested with. The libraries applications use behind them (JWT,
password hashing, signed tokens) are native too.

OAuth2 password flow (`OAuth2PasswordRequestForm`, `OAuth2PasswordBearer`), dependencies chained
on the current user, and HTTP Basic. The token is the user name, as in FastAPI's tutorial; the
[bookshelf example](https://github.com/larrymotalavigne/py2axum/tree/main/examples/bookshelf) shows bcrypt
hashes and signed JWTs, also native. Like every example on this site, it is compiled and compared with FastAPI in CI ([how](testing.md)).

```python title="docs_src/tutorial/security.py"
--8<-- "docs_src/tutorial/security.py"
```

## What is native

- `OAuth2PasswordBearer`, `HTTPBearer` (behaviour of FastAPI 0.142: 401 `Not authenticated`,
  `WWW-Authenticate: Bearer`), `HTTPBasic` (`realm=` a literal; `HTTPBasicCredentials`; a payload that is not
  base64 of ASCII `user:password` is a 401 even with `auto_error=False`, as in FastAPI),
  `OAuth2PasswordRequestForm`.
- Behind them, see the rows of [Libraries](../reference/libraries.md): PyJWT and python-jose (HMAC
  algorithms), bcrypt and pyotp, itsdangerous, cryptography's `Fernet`.

How the binary itself behaves in front of untrusted clients (threat model, input limits, the hybrid relay) is
described in [Security of the generated server](../advanced/security.md).
- `pwdlib`'s `PasswordHash.recommended()` (Argon2id, argon2-cffi's defaults): `hash()`, `verify()`,
  `verify_and_update()` (a hash of other parameters is rehashed); an unidentified hash is `UnknownHashError`.
  Other hashers and `hash(salt=...)` are refused.

## What stays in Python

- Other schemes (`APIKeyHeader`...) and a security scheme in `dependencies=[...]` are refused.
