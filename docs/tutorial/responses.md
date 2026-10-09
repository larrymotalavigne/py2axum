# Responses

FastAPI turns what an endpoint returns into a response: filtered through `response_model`, encoded by
`jsonable_encoder`, or sent as is when it is a `Response` object. The binary reproduces those rules and
Starlette's response classes, header order included.

A `response_model` that filters a field out, status codes, response classes (plain text, HTML,
redirect, JSON with a custom status), a cookie and a header set on the injected `Response`, and a server-sent
events stream. Like every example on this site, it is compiled and compared with FastAPI in CI ([how](testing.md)).

```python title="docs_src/tutorial/responses.py"
--8<-- "docs_src/tutorial/responses.py"
```

!!! note "Left to Python in this example"
    `GET /responses/items/{item_id}` uses the route option `response_model_exclude_unset=True`, which py2axum
    does not translate yet (nor `response_model_exclude_none=`): `py2axum check` reports it, and with
    `--python-side auto` the binary relays it to the Python application ([hybrid mode](../getting-started/hybrid.md)).
    Calling `model_dump(exclude_unset=True)` yourself is native.

## What is native

- `response_model` (filtering, aliases), status codes, returned dicts, lists, models,
  ORM objects (`from_attributes`).
- Returned `Response`, `JSONResponse`, `PlainTextResponse`, `HTMLResponse`, `RedirectResponse`,
  `FileResponse` are sent like Starlette 1.7 (header order, `ETag`/`Last-Modified` of `FileResponse`;
  no `Range`/partial `HEAD`). `JSONResponse(content=...)` serializes like Starlette (strict `json.dumps`:
  a model, a datetime or NaN raise). `set_cookie`/`delete_cookie` in `http.cookies` format, headers set on
  the injected `Response`.
- `response_class=` on a route (`HTMLResponse`, `PlainTextResponse`, `Response`, `RedirectResponse`,
  `FileResponse`, `JSONResponse`): a returned non-response value is encoded (`response_model` or
  `jsonable_encoder`), then wrapped like FastAPI (`str`/`bytes`/`None` body, a returned URL redirects with 307
  unless `status_code=` is given, a returned path is served as a file, empty body for 204/304, the injected
  `Response`'s headers appended). `response_class=StreamingResponse` with a non-response return value raises
  (500).
- `jsonable_encoder` semantics for returned values (models by alias, `bytes.decode()`, `Decimal` via
  FastAPI's `decimal_encoder`).
- Large lists returned straight from the session are streamed: see [Large list responses](../advanced/streaming.md).
- `StreamingResponse` and async generators (SSE). `await request.is_disconnected()` always returns `False`:
  a disconnected client is noticed at the next send, then the generator's `finally` runs.
- Response status codes: uvicorn has a status line for 100..599 only and drops the connection, unanswered, for
  any other `status_code`; the binary does the same.

Errors (`HTTPException`, exception handlers) are in [Handling errors](errors.md); templates
(`Jinja2Templates.TemplateResponse`) in [Libraries](../reference/libraries.md).

## What stays in Python

- The route options `response_model_exclude_unset=` and `response_model_exclude_none=` are refused (the
  `model_dump` options of the same names are native).
- Other `response_class=` classes are refused at transpile time.

## Differences

- **Known difference:** a mapped object returned without `response_model` is encoded like FastAPI does
  (`vars(obj)` without SQLAlchemy's `_sa_*` keys: its loaded attributes), but its keys come in column order.
  CPython's order follows SQLAlchemy's iteration over a set of columns, which depends on memory addresses:
  it changes from one process to the next, so no order can be reproduced. Same keys and values.
- **Known difference:** a final 1xx status is a 500 (hyper does not send an informational status as the final
  response).
