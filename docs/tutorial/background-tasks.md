# Background tasks

FastAPI's `BackgroundTasks` runs functions after the response has been sent. The binary runs them at the same
point.

A task added to `BackgroundTasks` runs after the response is sent; the second route shows
what it did. Like every example on this site, it is compiled and compared with FastAPI in CI ([how](testing.md)).

```python title="docs_src/tutorial/background.py"
--8<-- "docs_src/tutorial/background.py"
```

## What is native

- `BackgroundTasks`: run after the response (an error is logged), not after an error response.

For work that outlives a request, `asyncio.create_task`, threads and event loops are described in
[Python semantics § asyncio and threading](../reference/python.md#asyncio-and-threading).

## What stays in Python

- `BackgroundTasks` on a WebSocket route is refused ([WebSockets](websockets.md#what-stays-in-python)).
