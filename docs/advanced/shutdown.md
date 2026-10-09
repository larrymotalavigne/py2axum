# Graceful shutdown

On SIGTERM or SIGINT the binary does what uvicorn does: it stops accepting connections, closes idle
keep-alive connections, lets in-flight requests finish (their response carries `connection: close`), closes
WebSockets with code 1012, runs the lifespan's shutdown code, closes the database pool and exits with the
signal's status (143 for SIGTERM). A second signal exits at once.

The one difference: the wait is bounded by `PY2AXUM_SHUTDOWN_TIMEOUT` (25 s by default), where uvicorn waits
without limit, so an endless stream (an SSE feed) no longer holds the process until the orchestrator's
SIGKILL. Keep it below your orchestrator's grace period (Kubernetes: `terminationGracePeriodSeconds`, 30 s by
default).

## Details

Measured against uvicorn 0.54 (both servers, same clients), for every generated
binary, with or without a lifespan:

- As uvicorn: the listening socket is closed at once (new connections are refused), idle keep-alive
  connections are closed, in-flight requests finish and their response carries `connection: close`,
  WebSocket sessions get a close frame 1012 and the application receives `websocket.disconnect` with code
  1012, then the lifespan's code after `yield` runs, and the process ends by the signal it received (exit
  status 143 for SIGTERM, uvicorn re-raises it). A second signal skips the wait (uvicorn's force exit).
- Differences: the wait for in-flight requests and streams is bounded by `PY2AXUM_SHUTDOWN_TIMEOUT`
  seconds (default 25; uvicorn waits without a limit unless `--timeout-graceful-shutdown`, so an endless
  `StreamingResponse` such as an SSE feed holds it until the orchestrator's SIGKILL), after which the
  remaining connections are dropped; the database pool is closed before the exit (PostgreSQL gets a
  Terminate message; the Python process leaves its connections to be dropped with it).

The lifespan itself is described in [Lifespan events](../tutorial/lifespan.md); Sentry's queue is flushed at
shutdown ([Sentry](sentry.md)).
