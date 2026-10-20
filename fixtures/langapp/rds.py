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


# ---------------------------------------------------------------- redis (synchronous client), pools, pipelines
import redis as sredis  # noqa: E402
from redis import ResponseError  # noqa: E402

SYNC = sredis.Redis.from_url(URL, decode_responses=True)
SYNC_BYTES = sredis.from_url(URL)
SYNC_HOST = sredis.Redis(host="127.0.0.1", port=int(URL.rsplit(":", 1)[1].split("/")[0]), db=int(URL.rsplit("/", 1)[1]),
                         decode_responses=True)
SYNC_POOL = sredis.ConnectionPool.from_url(URL, max_connections=4, decode_responses=True)
SYNC_ON_POOL = sredis.Redis(connection_pool=SYNC_POOL)
APOOL = redis.ConnectionPool.from_url(URL, max_connections=4)
ASYNC_ON_POOL = redis.Redis(connection_pool=APOOL)
ATEXT = redis.Redis.from_url(URL, encoding="utf-8", decode_responses=True)


def sync_commands(r) -> dict:
    """The commands of the banc's applications, on a synchronous client (decode_responses=True)."""
    r.delete("s:a", "s:h", "s:l", "s:s", "s:z", "s:n", "s:otp")
    out = {}
    out["str"] = [r.set("s:a", "é"), r.get("s:a"), r.set("s:a", "x", nx=True), r.setex("s:n", 100, 7), r.get("s:n"),
                  0 < r.ttl("s:n") <= 100, r.ttl("s:a"), r.expire("s:a", 50), r.expire("s:missing", 50), r.persist("s:a"),
                  r.incr("s:cnt"), r.incr("s:cnt", 4), r.decr("s:cnt"), r.exists("s:a", "s:n", "s:missing")]
    out["hash"] = [r.hset("s:otp", mapping={"hash": "abc", "attempts": "0"}), r.hset("s:otp", "attempts", str(2)),
                   r.hgetall("s:otp"), r.hgetall("s:missing"), r.hget("s:otp", "hash"), r.hget("s:otp", "nope"),
                   r.hexists("s:otp", "hash"), r.hincrby("s:otp", "attempts"), r.hincrby("s:otp", "attempts", 5),
                   r.hset("s:h", "k", "v", items=["i1", 1, "i2", 2.5]), r.hmget("s:h", ["k", "i1", "zz"]),
                   sorted(r.hkeys("s:h")), r.hlen("s:h"), r.hdel("s:h", "k", "zz")]
    out["list"] = [r.lpush("s:l", "a", "b"), r.rpush("s:l", "c"), r.llen("s:l"), r.lrange("s:l", 0, -1), r.ltrim("s:l", 0, 1),
                   r.lrange("s:l", 0, -1), r.lpop("s:l"), r.rpop("s:l"), r.lpop("s:l"), r.llen("s:missing")]
    out["set"] = [r.sadd("s:s", "x", "y", "x"), sorted(r.smembers("s:s")), r.smembers("s:missing"), r.sismember("s:s", "y"),
                  r.sismember("s:s", "q"), r.scard("s:s"), r.srem("s:s", "x", "q")]
    out["zset"] = [r.zadd("s:z", {"m1": 1, "m2": 2.5, "m3": 3}), r.zadd("s:z", {"m1": 9}, nx=True), r.zadd("s:z", {"m1": 0.5}, ch=True),
                   r.zcard("s:z"), r.zrange("s:z", 0, -1), r.zrange("s:z", 0, -1, withscores=True),
                   r.zrange("s:z", 0, 0, desc=True), r.zrevrange("s:z", 0, 1, withscores=True), r.zscore("s:z", "m2"),
                   r.zscore("s:z", "zz"), r.zremrangebyscore("s:z", 0, 1), r.zremrangebyrank("s:z", 0, 0), r.zrem("s:z", "m3", "q"),
                   r.zcard("s:z")]
    out["misc"] = [sorted(r.keys("s:*")), sorted(r.scan_iter(match="s:[hl]*")), type(r.scan_iter()).__name__ != "list",
                   r.publish("s:chan", "hi"), r.type("s:otp"), r.ping()]
    errs = []
    for bad in (lambda: r.hset("s:h"), lambda: r.set("s:a", None), lambda: r.lpush("s:otp", "x"), lambda: r.zadd("s:z", {}),
                lambda: r.zadd("s:z", {"a": 1}, nx=True, xx=True)):
        try:
            bad()
        except sredis.RedisError as e:
            errs.append([type(e).__name__, str(e), isinstance(e, sredis.DataError), isinstance(e, ResponseError)])
    out["errs"] = errs
    return out


@router.post("/sync")
async def sync_basic():
    out = {"text": sync_commands(SYNC), "host": SYNC_HOST.get("s:a"), "pool": SYNC_ON_POOL.hget("s:otp", "hash")}
    out["bytes"] = [SYNC_BYTES.get("s:a"), sorted(SYNC_BYTES.hgetall("s:otp").items()), SYNC_BYTES.zrange("s:z", 0, -1, withscores=True)]
    try:
        await SYNC.get("s:a")
    except TypeError as e:
        out["await"] = str(e)
    try:
        SYNC.aclose()
    except AttributeError as e:
        out["aclose"] = str(e)
    with sredis.Redis.from_url(URL) as r:
        out["with"] = r.exists("s:a")
    return out


@router.get("/sync-def")
def sync_def():
    r = sredis.Redis.from_url(URL, decode_responses=True)
    r.setex("s:def", 30, "v")
    out = [r.get("s:def"), r.delete("s:def"), r.get("s:def")]
    r.close()
    return out


@router.post("/pipe")
async def pipelines():
    out = {}
    await ATEXT.delete("p:z", "p:n", "p:s")
    async with ATEXT.pipeline(transaction=True) as pipe:
        pipe.zremrangebyscore("p:z", 0, 10)
        pipe.zcard("p:z")
        pipe.zadd("p:z", {"u1": 20})
        pipe.expire("p:z", 60)
        out["tx"] = await pipe.execute()
    pipe = ATEXT.pipeline(transaction=False)
    pipe.incr("p:n").incr("p:n", 3).get("p:n").set("p:s", "v").hgetall("p:missing")
    out["chain"] = await pipe.execute()
    out["empty"] = await pipe.execute()
    pipe = ATEXT.pipeline()
    pipe.set("p:s", "w")
    pipe.lpush("p:s", "x")
    pipe.get("p:s")
    try:
        await pipe.execute()
    except ResponseError as e:
        out["err"] = str(e)
    out["after"] = await ATEXT.get("p:s")
    with SYNC.pipeline() as sp:
        sp.set("p:t", "1").incr("p:t").get("p:t").delete("p:t")
        out["sync"] = sp.execute()
    sp = SYNC.pipeline(transaction=False)
    sp.hset("p:h", mapping={"a": 1}).lpush("p:h", "x")
    try:
        sp.execute()
    except ResponseError as e:
        out["sync_err"] = str(e)
    out["sync_after"] = SYNC.hgetall("p:h")
    return out


@router.post("/apool")
async def async_pool():
    await ASYNC_ON_POOL.set("ap:k", "v")
    out = [await ASYNC_ON_POOL.get("ap:k"), await ATEXT.get("ap:k"), await ATEXT.hgetall("ap:missing")]
    await ATEXT.hset("ap:h", mapping={"x": "1", "y": "2"})
    out.append(await ATEXT.hgetall("ap:h"))
    async with redis.Redis.from_url(URL, decode_responses=True) as r:
        out.append(await r.zadd("ap:z", {"a": 1.5}))
        out.append(await r.zrange("ap:z", 0, -1, withscores=True))
        try:
            await r.lpush("ap:h", "q")
        except ResponseError as e:
            out.append(str(e))
    return out
