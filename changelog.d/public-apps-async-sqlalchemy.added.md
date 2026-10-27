- `await session.stream(stmt)` and `await session.stream_scalars(stmt)` (an `AsyncResult` / `AsyncScalarResult`
  read by `async for`, its methods awaited), and `joinedload(..., innerjoin=True)` on a many-to-one whose foreign
  key is NOT NULL: rhoboro/async-fastapi-sqlalchemy goes from 1 to 11 native routes out of 11
  (`fixtures/dynapp/streamx.py` reproduces its patterns).
- `FastAPI(middleware=middleware)` in an app factory, with `middleware = [...]` at module level, and
  `model_config = config` with `config = ConfigDict(...)` shared by several models (both bound once and never
  changed).
- `include_router(router, prefix=settings.API_PREFIX)` inside the app factory, when the prefix reads only module
  globals: read at startup, as at module level.
