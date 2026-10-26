- `starlette_context`'s `context`: the request's context dict filled by `RawContextMiddleware` (`context[key]`,
  `in`, `get`/`setdefault`/`pop`/`update`, `context.data`, `context.exists()`, `ContextDoesNotExistError`). The
  plugins' response headers are read back from the context, as starlette-context does.
