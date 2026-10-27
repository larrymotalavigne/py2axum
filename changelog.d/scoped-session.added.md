- `async_scoped_session(session_factory=, scopefunc=)` / `scoped_session(..., scopefunc=)`: a session per value of
  the scope function (a `ContextVar` set per request by a middleware, as teamhide/fastapi-boilerplate does), the
  proxy's methods the current session's, `session()`, `await session.remove()` (`fixtures/dynapp/scoped.py`).
