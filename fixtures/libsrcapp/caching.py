"""aiocache 0.12, translated from its source: Cache(Cache.MEMORY / Cache.REDIS), serializers, @cached."""
import os

from aiocache import Cache, cached
from aiocache.serializers import JsonSerializer, PickleSerializer
from fastapi import APIRouter

router = APIRouter(prefix="/cache")

REDIS_DB = int(os.environ.get("LIBSRC_REDIS_DB", "12"))
MEM = Cache(Cache.MEMORY, namespace="mem")
RED = Cache(Cache.REDIS, endpoint="127.0.0.1", port=6379, db=REDIS_DB, namespace="lsrc", serializer=JsonSerializer())
PICKLED = Cache(Cache.REDIS, endpoint="127.0.0.1", port=6379, db=REDIS_DB, namespace="lsrcp", serializer=PickleSerializer())

CALLS = {"square": 0}


@cached(ttl=60, cache=Cache.MEMORY, key_builder=lambda f, *args, **kwargs: f"sq:{args[0]}")
async def slow_square(x: int) -> int:
    CALLS["square"] += 1
    return x * x


@router.get("/square/{x}")
async def square(x: int):
    return {"v": await slow_square(x), "calls": CALLS["square"]}


@router.post("/mem")
async def mem():
    out = [await MEM.set("a", {"x": [1, 2]}), await MEM.get("a"), await MEM.exists("a"), await MEM.get("missing", default=7)]
    out += [await MEM.add("b", 1), await MEM.increment("b", 4), await MEM.multi_get(["a", "b", "zz"])]
    try:
        await MEM.add("b", 2)
    except ValueError as e:
        out.append(str(e))
    out += [await MEM.delete("a"), await MEM.delete("a"), await MEM.clear()]
    return out


@router.post("/redis")
async def redis_cache():
    await RED.delete("k")
    out = [await RED.set("k", {"name": "é", "n": [1, 2.5, None]}, ttl=30), await RED.get("k"), await RED.exists("k")]
    out += [await RED.multi_set([("m1", 1), ("m2", "two")], ttl=30), await RED.multi_get(["m1", "m2", "nope"])]
    out += [await RED.expire("k", 10), await RED.delete("k"), await RED.get("k", default="gone")]
    await PICKLED.set("p", {"t": (1, 2), "s": "x"}, ttl=30)
    out.append(await PICKLED.get("p"))
    out += [await RED.delete("m1"), await RED.delete("m2"), await PICKLED.delete("p")]
    return out
