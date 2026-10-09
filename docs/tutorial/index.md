# Tutorial - User Guide

The tutorial follows the chapters of FastAPI's own tutorial. Each page takes one FastAPI feature and says what
py2axum does with it, so you can read it next to the FastAPI page you already know. Every page has the same
layout:

- a short introduction naming the feature it covers;
- an example application using it, which py2axum translates and whose binary answers like FastAPI;
- **What is native**: what the binary reproduces, and how precisely;
- **What stays in Python**: what is refused at compile time (with `file:line`), and can be left to a Python
  process with `--python-side` ([Hybrid mode](../getting-started/hybrid.md));
- **Differences**: where the binary is knowingly not identical to the Python application, when there are any.

py2axum refuses rather than guesses: a construct is either translated with the behaviour described in these
pages or not translated at all. The full list of pages that make up the supported subset is in
[Supported subset](../supported.md).

| Page | FastAPI feature |
|---|---|
| [Parameters](parameters.md) | path, query and header parameters |
| [Request body](body.md) | JSON bodies |
| [Models (Pydantic)](models.md) | Pydantic v2 models, validators, settings |
| [Responses](responses.md) | `response_model`, response classes, streaming |
| [Handling errors](errors.md) | `HTTPException`, exception handlers |
| [Dependencies](dependencies.md) | `Depends`, `yield` dependencies, the session dependency |
| [Security](security.md) | OAuth2, HTTP Bearer and Basic |
| [Middleware](middleware.md) | Starlette's middleware stack, raw ASGI middleware |
| [CORS](cors.md) | `CORSMiddleware` |
| [SQL databases](sql.md) | SQLAlchemy 2.0, async and sync |
| [Forms and files](files.md) | `Form()`, `File()`, `UploadFile` |
| [Background tasks](background-tasks.md) | `BackgroundTasks` |
| [WebSockets](websockets.md) | `@app.websocket` |
| [Lifespan events](lifespan.md) | `FastAPI(lifespan=...)` |
| [Bigger applications](bigger-applications.md) | routers, factories, routing, mounts |
| [Testing](testing.md) | your tests, and testing the binary |
