# Security of the generated server

This page describes what the binary py2axum generates guarantees when it faces untrusted clients. It covers
the threat model, the protections the runtime (`py2axum/runtime/dynrt/`, copied into every generated crate)
puts in place, how they compare with uvicorn + Starlette + FastAPI, and the knobs you can turn. It is the
result of the security review done before 1.0. Each guarantee is pinned by a test, named in **Tests** at the
end.

To report a vulnerability, see [SECURITY.md](https://github.com/larrymotalavigne/py2axum/blob/main/SECURITY.md).

**Contents:** [Threat model](#threat-model) · [SQL](#sql) · [Request input](#request-input) · [Files and paths](#files-and-paths)
· [Errors and logs](#errors-and-logs) · [Cryptography](#cryptography) · [Hybrid deployments](#hybrid-deployments-the-python-side-relay)
· [Per-request state](#per-request-state) · [Rust dependencies](#rust-dependencies) · [`unsafe` code](#unsafe-code)
· [Settings](#settings) · [Tests](#tests)

## Threat model

- **Attacker:** any HTTP or WebSocket client of the binary, sending arbitrary bytes: request line, headers,
  cookies, query string, body (JSON, forms, multipart), WebSocket frames.
- **Trusted:** the application's source (it is compiled), its configuration and environment, the database,
  the Python sidecar of a hybrid deployment, and the machine itself (files the binary reads,
  `PROMETHEUS_MULTIPROC_DIR`).
- **Goal:** the binary must not be *less safe* than the Python application it replaces. A behaviour that is
  unsafe in the Python application (an endpoint that splices a request value into `text()`, a guard written
  with `==` on a secret) stays the same in the binary: py2axum reproduces your application, it does not
  audit it.
- **Availability:** a single request must never take the whole process down. CPython turns most resource
  exhaustion into an exception (RecursionError, MemoryError) that costs one 500; a Rust stack overflow or a
  failed allocation aborts the process, and every in-flight request with it. The runtime therefore converts
  those cases into the same exceptions (see [Request input](#request-input)).

## SQL

- **Values are always bound.** Every value that reaches a statement (comparisons, `in_()`, `like`/`ilike`/
  `contains`/`startswith`/`endswith`, `regexp_match`, JSONB and ARRAY operators, `limit`/`offset`,
  `insert().values()`, `update().values()`, the unit of work, `text()` with parameters or `.bindparams()`) is
  sent as a `$n` parameter. A value is never re-parsed for placeholders.
- **Names given as strings are quoted like SQLAlchemy quotes them.** A label (`.label(name)`), a subquery or
  alias name (`.subquery(name)`, `.alias(name)`), an `index_elements` string of `on_conflict_do_*`, and the
  label or column named by `order_by("…")` are written as is only when they are plain lowercase identifiers
  that are not reserved words; anything else is put between double quotes with `"` doubled (SQLAlchemy's
  `IdentifierPreparer`).
- **Equivalent by design:** `text("…")` built from a request string, `.op(string)` and `literal_column`-style
  constructs are raw SQL in SQLAlchemy too; the binary sends exactly what the application wrote.
  `order_by("string")` only accepts a label or column of the statement, else it raises SQLAlchemy's
  CompileError. `cast()` accepts a closed list of types, `extract()` a field made of letters.
- **Connection settings** (`connect_args` options, the session time zone) are applied with
  `set_config($1, $2)`, never spliced into `SET`. Savepoint names are generated integers.
- **DDL** (`create_all`) is produced by SQLAlchemy itself at compile time from your models; the existence
  checks run at start-up are parameterised.
- Table and column names come from the models at compile time and are not quoted (a reserved word or a
  mixed-case column name is a correctness gap, not an injection: no request value reaches them).

## Request input

Limits that FastAPI and uvicorn do not have stay off by default, so a translated application answers the
same requests. The ones Starlette applies are applied the same way.

| Input | uvicorn / Starlette / FastAPI | The binary |
| --- | --- | --- |
| Body size | no limit | no limit by default; `PY2AXUM_MAX_BODY` (bytes) answers **413** past it, checked on `Content-Length` and while reading a chunked body |
| JSON nesting | CPython's decoder (~10 000 levels on 3.12/3.13, ~50 000 on 3.14), then RecursionError → 400 | iterative decoder, 400 past 10 000 levels; see below |
| Numbers | int → str limited to 4 300 digits | integers beyond 64 bits are an OverflowError (documented in [Python semantics](../reference/python.md)); decoding is linear |
| Invalid UTF-8, lone surrogates | 400 / kept | 400 / U+FFFD (documented) |
| Multipart | `max_files` 1 000, `max_fields` 1 000, 1 MiB per non-file part | same limits, same 400 messages |
| URL-encoded forms | 1 000 fields, 1 MiB per field (Starlette ≥ 1.4) | same |
| Upload file names | as sent, an IE-style `C:\…` path cut to its last part (python-multipart) | same; files are held in memory, never written under the client's name |
| Headers | h11/httptools limits | hyper: at most 100 headers (431 past it), ~400 KB in total |
| Cookies | Starlette's parser, a dict | same parser, linear in the header size |
| WebSocket messages | 16 MiB (`ws_max_size`) | 16 MiB per frame and per message |
| Regular expressions | `Field(pattern=)` with pydantic-core's linear engine; `re` backtracks | `Field(pattern=)` with Rust's linear `regex`; `re` uses `fancy-regex` (backtracking, as CPython) |

**Deeply nested values.** The decoder accepts up to 10 000 levels, as CPython does, and the runtime then walks
the value recursively (validation, the 422 body that echoes the input, serialisation, `repr`, copies). To keep
a small request from overflowing the stack, which would abort the process:

- the runtime threads get a 256 MiB stack (`PY2AXUM_STACK_SIZE`). It is reserved address space: memory is only
  touched as deep as a request actually recurses, and the resident size of an idle server does not change;
- every project function and the runtime's recursive walks check the remaining stack and raise
  `RecursionError("maximum recursion depth exceeded")` instead of overflowing, so even with a small stack the
  outcome is a 500, never an abort. Unbounded recursion in your own code also ends in RecursionError, as in
  Python (the depth at which it happens differs: CPython counts frames, the binary measures stack).

Difference: between ~1 000 and 10 000 levels, FastAPI often answers 500 (its recursive `jsonable_encoder` or
Pydantic hits Python's recursion limit while echoing the input or the result); the binary usually answers
normally. Neither process goes down.

**Sizes chosen by the request.** `str * n`, `bytes * n`, `list * n`, `secrets.token_*(n)` raise OverflowError or
MemoryError (and ValueError for a negative token size) like CPython, instead of a failed allocation that would
abort. A request body is buffered whole before routing (uvicorn reads it lazily, when the endpoint asks); behind
a reverse proxy, cap bodies there (ingress-nginx `proxy-body-size`) or set `PY2AXUM_MAX_BODY`.

**Response headers.** A header value the application builds from request data with CR, LF or NUL in it never
reaches the client: the response becomes a plain 500. Header injection (response splitting) is not possible.
Difference: uvicorn refuses such a header at send time and closes the connection without an answer.

**Panics.** The generated crates do not set `panic = "abort"`: a bug that panics loses its own connection only,
and the runtime's locks do not poison.

## Files and paths

- `FileResponse(path)` checks nothing about `path`, exactly like Starlette: confining it is the application's
  job. A missing path or a directory raise the RuntimeError Starlette raises (500). The file is read off the
  event-loop threads. `Content-Disposition` file names are percent-encoded (`filename*=`), so they cannot
  inject a header.
- `StaticFiles` is not translated: a mount is refused at compile time or left to the Python side
  (`--python-side mount`), where Starlette's own checks (`realpath`, `..`, symlinks) apply unchanged.
- `Path.resolve()` and `os.path.realpath()` resolve symlinks component by component like CPython's
  `_joinrealpath`, including below a final part that does not exist yet. The common upload guard
  `(BASE / name).resolve().relative_to(BASE.resolve())` therefore refuses `name = "link/new"` when `BASE/link`
  points outside `BASE`, as in Python.
- No temporary files: `UploadFile` is in memory, `tempfile` is not translated.

## Errors and logs

- An unhandled exception gives Starlette's response: status 500, `text/plain; charset=utf-8`, body
  `Internal Server Error`. No message, traceback, SQL text, constraint name or driver error reaches the client.
  `FastAPI(debug=True)` (and any non-literal `debug=`) is refused at compile time, so a debug page is never
  served.
- On stderr, an unhandled exception is one line, `ERROR:py2axum:Exception in ASGI application: Class: message`.
  Request headers, cookies and bodies are not logged. The SQL text of a failing statement is logged only with
  `PY2AXUM_SQL_DEBUG` set (bound values are never logged). Nothing from the environment or the configuration is logged at
  start-up; a malformed `DATABASE_URL` stops the binary without printing it.
- The Python-side relay logs a failing upstream with the request path only (a query string can carry a token)
  and answers the client a bare `502 Bad Gateway`.
- Sentry (`sentry-sdk` translated natively) follows the Python SDK: `send_default_pii` is false by default;
  without it, `Authorization`, `Cookie`, `Set-Cookie`, `X-Forwarded-For`, `X-Real-Ip` and API-key headers are
  `[Filtered]`, cookies and the client IP are not sent, and the SDK's default denylists (passwords, tokens,
  secrets, session ids…) scrub headers, cookies, form data, `extra`, `user` and breadcrumbs. As in Python,
  bodies follow `max_request_body_size`, nested request data is scrubbed at the first level only and the query
  string is sent as is. No local variables, `sys.argv` or environment are sent.

## Cryptography

- **Constant-time comparisons** (the `subtle` crate) in `hmac.compare_digest`, `secrets.compare_digest`,
  itsdangerous signatures, python-jose and PyJWT HMAC verification, Fernet tokens, `bcrypt.checkpw` and pyotp's
  `verify`. A comparison your code writes with `==` stays a plain comparison, as in Python.
- **Randomness** comes from the operating system: `secrets.*`, `uuid.uuid4`, Fernet IVs and keys, bcrypt salts,
  pyotp secrets, Web Push keys and salts and RSA key generation use `OsRng` or `rand`'s `thread_rng` (ChaCha12
  seeded and reseeded from the OS). Python's `random` module maps to the same generator (stronger than
  CPython's Mersenne Twister, which only makes it unpredictable).
- **JWT.** PyJWT: `decode` without `algorithms=` (or with an empty list) raises PyJWT's DecodeError; a header
  `alg` outside the list is InvalidAlgorithmError; `alg: none` verifies only if `"none"` is in the list and the
  key is `None`, and even then the signature check fails as in PyJWT; HMAC keys that look like a PEM, SSH or
  JWK public key are refused (algorithm confusion); `exp`/`nbf`/`iat`/`aud`/`iss`/`leeway`/`require` follow
  `api_jwt.py`. Asymmetric algorithms are refused at run time with a clear error. python-jose: `none` and
  asymmetric algorithms raise JWKError; `algorithms=None` accepts any HS* algorithm, as python-jose does.
- **itsdangerous**: same key derivation and salts, newest-to-oldest key rotation, signature checked before the
  payload is decoded or decompressed, `max_age` enforced (also for timestamps in the future).
- **Fernet**: version byte, HMAC checked before decryption, padding errors are InvalidToken (no padding oracle).
  `ttl=` is refused rather than ignored.
- **bcrypt**: cost 4–31, `2a`/`2b` prefixes, passwords over 72 bytes raise ValueError (bcrypt 5 behaviour, no
  silent truncation). Hashing runs off the event loop (pyca/bcrypt releases the GIL), so concurrent logins do
  not stall other requests.
- Key material is never printed: the runtime's key types have no debug representation and their errors carry
  no key bytes.

## Hybrid deployments: the Python-side relay

Routes left to Python (`--python-side`) are relayed by the binary to `PY2AXUM_PYTHON_URL`.

- **No SSRF:** the upstream URL is `PY2AXUM_PYTHON_URL` followed by the request's path and query, taken from
  the origin-form target; an absolute-form request line (`GET http://other/ HTTP/1.1`), `//host` or `@` in the
  path cannot change the host. Redirects are not followed. The relay ignores `HTTP_PROXY`/`HTTPS_PROXY`/
  `ALL_PROXY`, so requests with their cookies and tokens never leave through an outbound proxy.
- **Headers:** hop-by-hop headers (`Connection`, `Keep-Alive`, `Transfer-Encoding`, `TE`, `Trailer`,
  `Upgrade`, `Proxy-*`) and every header named in `Connection` are dropped in both directions;
  `Content-Length` is recomputed from the body the binary read, so a request carrying both `Content-Length`
  and `Transfer-Encoding` cannot desynchronise the sidecar. `Host` is passed as received, as are
  `X-Forwarded-*` and `Forwarded`: the translated routes see them unchanged too, and applications that read
  them (`X-Real-Ip`, the last `X-Forwarded-For` hop set by the ingress) behave the same on both sides.
- **Run the sidecar with the trust you intend.** uvicorn trusts `X-Forwarded-For`/`X-Forwarded-Proto` from
  `127.0.0.1` by default (`--forwarded-allow-ips`), and the relay connects from `127.0.0.1`. Behind an ingress
  that overwrites these headers (ingress-nginx does) nothing changes. If clients can reach the binary directly,
  start the sidecar with `--forwarded-allow-ips ''` (or `FORWARDED_ALLOW_IPS=''`), otherwise a client chooses
  the `request.client.host` and scheme the Python routes see. The translated routes always use the TCP peer.
- The connection to the sidecar times out after 10 s if it cannot be established; a slow response is waited
  for, as uvicorn waits for the application.
- WebSocket upgrades are never relayed.

## Per-request state

ContextVars, `request.state`, the ASGI scope, the derived contexts of raw ASGI middlewares and
starlette-context's per-request dictionary live in the request's own context: concurrent requests never see
each other's values. Process-wide state is what the application declares at module level, shared exactly as
in a uvicorn worker. Thread-locals used during validation are set and consumed without an `await` in between.

Known difference: a task created by a request (`asyncio.create_task`) shares the request's ContextVar map
instead of a copy, so a `set()` in the child is visible to its parent within the same request. A rare race on
`@property` values of one module-level object validated by two requests at once can give one of them the other's
value; it never crosses request data from distinct objects.

## Rust dependencies

Every generated crate ships `py2axum/runtime/Cargo.lock` and is built with `--locked`. CI runs `cargo audit`
on it (`tools/cargo_audit.sh`, job `audit`) and fails on any RustSec advisory not listed here:

| Advisory | Crate | Why it is accepted |
| --- | --- | --- |
| RUSTSEC-2023-0071 (Marvin) | `rsa` 0.9 | timing side channel on RSA *decryption* with a private key. The runtime only generates keys (`cryptography`'s `rsa.generate_private_key`, serialised for DKIM) and verifies PKCS#1 v1.5 signatures with public keys (Google ID tokens); it never decrypts or signs with RSA. No fixed version exists yet. |

## `unsafe` code

Every `unsafe` block of the runtime, and why it is sound:

| Where | What | Invariant |
| --- | --- | --- |
| `prom.rs` `MFile` | `mmap` of prometheus_client's multiprocess files (same format as CPython) | the header (`used`) and every offset read from a file another process writes are checked against the mapping size before any access; a corrupted file is an error, never an out-of-bounds read; the pointer is only used under the files mutex (`Send`, not `Sync`) |
| `net.rs` `getaddrinfo` | libc resolver for `socket.getaddrinfo` | host and service go through `CString` (an interior NUL is ValueError); each result's address is checked non-null and long enough for its family before it is read; the list is freed once |
| `net.rs` `strerror_r` | error text of `socket` errors | thread-safe variant, buffer and length passed together |
| `sysmon.rs` | `statvfs`, `sysctlbyname` (psutil) | NUL-checked path, zeroed output structures |
| `sentry.rs` | `gethostname` | fixed 256-byte buffer, a missing terminator handled |
| `web.rs` | restores the default SIGTERM/SIGINT action and re-raises at exit (exit status like uvicorn) | called once from the main task, outside any signal handler |

The generated application code contains no `unsafe`.

## Settings

| Variable | Default | Effect |
| --- | --- | --- |
| `PY2AXUM_MAX_BODY` | unset (no limit, like uvicorn) | largest request body in bytes; past it, 413 |
| `PY2AXUM_STACK_SIZE` | 268435456 (256 MiB) | stack of the runtime threads (address space reserved, not memory used) |
| `PY2AXUM_SQL_DEBUG` | unset | logs the SQL text of a failing statement (not its bound values) |
| `PY2AXUM_SHUTDOWN_TIMEOUT` | see [Graceful shutdown](shutdown.md) | grace period of open connections at shutdown |

## Tests

- `fixtures/dynapp/sec.py` with its cases in `tests/scenarios/dynapp.py` (conformance against FastAPI, normal and
  forced-streaming passes): request values used as labels, subquery names, `index_elements` and `order_by`
  strings, values in every comparison and in `text()`, with the table checked intact after each; symlinks under
  `resolve()`; CR/LF in a response header; the body of a 500 (an exception message with a password, an
  IntegrityError); `token_urlsafe(-1)`; `str * n` past memory; Starlette's multipart limits and Windows file
  names.
- `tests/security_check.py` (against the binary alone, in CI after the conformance): JSON nested up to 50 000
  levels on model, `dict` and `Any` bodies with the process still serving; 30 000 cookies in one header; 5 000
  multipart files; eight concurrent bcrypt hashes at cost 12 while other requests stay under 0.5 s; the relay
  dropping `Connection`-named headers; `PY2AXUM_MAX_BODY` with `Content-Length` and chunked bodies.
- The existing PyJWT, python-jose, itsdangerous and Fernet cases of the dynapp scenario (wrong algorithms,
  `none`, PEM keys used as HMAC secrets, tampered and expired tokens).
- The `audit` CI job (`cargo audit`).
