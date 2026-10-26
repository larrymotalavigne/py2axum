- redis.asyncio pub/sub: `r.pubsub()`, `subscribe`/`psubscribe`/`unsubscribe`/`punsubscribe`, `get_message(timeout=)`,
  `async for m in ps.listen()`, `subscribed`, `aclose()` / `async with`, with redis-py's message dicts (confirmations
  included) in the server's order.
