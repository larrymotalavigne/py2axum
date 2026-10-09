# WebSockets

FastAPI declares WebSocket endpoints with `@app.websocket(...)`; Starlette's `WebSocket` object accepts the
connection, exchanges messages and closes it. The binary implements the server side of the protocol itself,
with Starlette's state checks and uvicorn's answers to the client.

Measured against Starlette 1.7, FastAPI 0.142 and uvicorn 0.54 (its default `websockets-sansio` protocol);
the conformance suite compares the handshake, the messages and the close codes with a WebSocket client.

An echo endpoint, and a room that checks a token before `accept()` (the client then gets an HTTP
403), iterates over JSON messages and closes with a code and a reason. Like every example on this site, it is compiled and compared with FastAPI in CI ([how](testing.md)).

```python title="docs_src/tutorial/websockets.py"
--8<-- "docs_src/tutorial/websockets.py"
```

## What is native

- `@app.websocket(path, name=, dependencies=)` and `@router.websocket(...)` (router prefixes, includes and
  their dependencies). Parameters: `WebSocket`, `HTTPConnection`, path/query/header parameters,
  `Depends` (generator dependencies run their exit code once the endpoint returns, the session dependency
  commits there). A validation error closes with 1008 before `accept`: the client gets HTTP 403.
- `WebSocket`: `accept(subprotocol=, headers=)`, `receive()`/`send(message)` (ASGI dicts),
  `receive_text/bytes/json(mode=)`, `send_text/bytes/json(mode=)` (`json.dumps` with compact separators
  and `ensure_ascii=False`), `close(code, reason)`, `iter_text/bytes/json()` (in `async for`),
  `send_denial_response(response)`, `client_state`/`application_state` (`WebSocketState`),
  `headers`, `query_params`, `path_params`, `cookies`, `client`, `url`, `state`, `scope`, `app`. Starlette's
  state checks and messages (`RuntimeError`, `WebSocketDisconnected`); `WebSocketDisconnect(code, reason)`
  when the client closes, `WebSocketDisconnect(1006)` on a send after it left.
- What the client sees, as uvicorn does it: `close()` before `accept()` (also `WebSocketException`, a
  failed validation, a path without WebSocket route) → HTTP 403 with an empty body; an exception or a
  return before `accept()` → HTTP 500; `HTTPException` before `accept()` → the JSON error response
  (FastAPI's handler); an exception or a return after `accept()` without `close()` → the connection is
  dropped without close frame (1006 for the client, not 1011); `WebSocketException` after `accept()` →
  a close frame with its code and reason.
- Exception handlers: `@app.exception_handler(...)` handlers are called with the `WebSocket` (by status
  code, then MRO); the `Exception`/500 handler is not (ServerErrorMiddleware lets WebSockets through, like
  CORS, GZip and `BaseHTTPMiddleware`).
- `async for` over async iterators in general (async generators, objects defining `__aiter__`/`__anext__`).

WebSocket routes need `starlette>=1.7.0` ([Versions](../reference/versions.md)). At shutdown, open sessions are
closed with code 1012 ([Graceful shutdown](../advanced/shutdown.md)).

## What stays in Python

- Refused: `Request`, `Response`, `BackgroundTasks`, body/`Form`/`File` parameters and security schemes
  on a WebSocket route or in one of its dependencies (FastAPI does not provide them there),
  `app.add_websocket_route`, `add_api_websocket_route`, `websocket_route` decorators, sync endpoints.
- WebSocket requests are never relayed to `PY2AXUM_PYTHON_URL`, and a WebSocket route cannot be declared
  `--python-side`: route it to Python with your ingress if it does not translate.

## Differences

- No `permessage-deflate` compression (the handshake answer has no
  `Sec-WebSocket-Extensions`; messages are the same); no server keepalive pings (uvicorn pings every 20 s
  and closes with 1011 after 20 s without pong); messages are limited to 16 MiB like uvicorn, but the
  error past it may differ; an invalid handshake request (missing key, version ≠ 13) is answered by axum
  (400/426, other text); uvicorn's access log lines are not printed.
