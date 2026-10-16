"""redis.asyncio: commands, replies (bytes / decode_responses), errors, scan_iter, a pickled cache entry."""
import os
import pickle

import redis.asyncio as redis
from fastapi import APIRouter
from redis.asyncio.retry import Retry
from redis.backoff import ExponentialBackoff
from redis.exceptions import ConnectionError as RedisConnectionError
from redis.exceptions import DataError, RedisError, TimeoutError as RedisTimeoutError

router = APIRouter(prefix="/rds")

URL = os.environ.get("REDIS_URL", "redis://127.0.0.1:6379/13")
CLIENT = redis.from_url(URL, health_check_interval=30, socket_keepalive=True, retry=Retry(ExponentialBackoff(), 3),
                        retry_on_error=[RedisConnectionError, RedisTimeoutError])
TEXT = redis.from_url(URL, decode_responses=True)


@router.post("/basic")
async def basic():
    c = CLIENT
    out = {}
    out["set"] = [await c.set("a", "é"), await c.set("a", "x", nx=True), await c.set("n", 5), await c.set("f", 1.5)]
    out["get"] = [await c.get("a"), await c.get("missing"), await TEXT.get("a"), await c.get("n"), await TEXT.get("f")]
    out["setex"] = [await c.setex("t", 100, b"\x00\x01"), 0 < await c.ttl("t") <= 100, await c.ttl("a"), await c.ttl("missing")]
    out["incr"] = [await c.incr("cnt"), await c.incr("cnt"), await c.incr("cnt", 5), await c.decr("cnt")]
    out["mget"] = [await c.mget(["a", "missing", "n"]), await TEXT.mget("a", "n")]
    out["exists"] = [await c.exists("a"), await c.exists("a", "n", "missing")]
    out["lock"] = [await c.set("lock:k", "1", ex=30, nx=True), await c.set("lock:k", "1", ex=30, nx=True)]
    out["scan"] = sorted([k async for k in c.scan_iter(match="lock:*")]) + sorted([k async for k in TEXT.scan_iter(match="n*")])
    out["delete"] = [await c.delete("a"), await c.delete("n", "f", "missing"), await c.delete(*[k async for k in c.scan_iter(match="lock:*")])]
    errs = []
    for bad in (None, True):
        try:
            await c.set("bad", bad)
        except DataError as e:
            errs.append(str(e))
    out["errs"] = errs
    out["ping"] = await c.ping()
    return out


@router.post("/pickled")
async def pickled():
    entry = {"value": [{"id": 1, "name": "x"}], "created_at": 1.5}
    await CLIENT.setex("cache:k", 60, pickle.dumps(entry))
    data = await CLIENT.get("cache:k")
    return {"back": pickle.loads(data), "type": type(data).__name__}


@router.get("/down")
async def down():
    c = redis.from_url("redis://127.0.0.1:1/0")
    try:
        await c.get("x")
        return {"ok": True}
    except RedisError as e:
        return {"error": type(e).__name__, "is_conn": isinstance(e, RedisConnectionError)}
    finally:
        await c.aclose()
