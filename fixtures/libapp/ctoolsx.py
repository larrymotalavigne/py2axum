"""cachetools 7.2 (dynrt/ctools.rs): Cache, FIFOCache, LRUCache and TTLCache driven by scripts of operations (eviction
orders, sizes with getsizeof=len, expiry with ttl=0 and a long ttl, errors and their messages, repr), the @cached
decorator (default and custom keys, lock, info, a dict as cache, cache=None), cachetools.func's lru_cache / fifo_cache /
ttl_cache, keys.hashkey / methodkey."""
import threading
import time

from cachetools import Cache, FIFOCache, LRUCache, TTLCache, cached, cachedmethod, keys
from cachetools.func import fifo_cache, lru_cache, ttl_cache
from fastapi import APIRouter

router = APIRouter(prefix="/ctoolsx")
CALLS: list = []


def _make(kind, maxsize, ttl, sized):
    if kind == "Cache":
        return Cache(maxsize, getsizeof=len) if sized else Cache(maxsize)
    if kind == "FIFOCache":
        return FIFOCache(maxsize, getsizeof=len) if sized else FIFOCache(maxsize)
    if kind == "LRUCache":
        return LRUCache(maxsize, getsizeof=len) if sized else LRUCache(maxsize)
    if kind == "TTLWall":
        return TTLCache(maxsize, ttl, timer=time.time)
    return TTLCache(maxsize, ttl, getsizeof=len) if sized else TTLCache(maxsize=maxsize, ttl=ttl)


def _err(e):
    return {"err": type(e).__name__, "msg": str(e), "args": [repr(a) for a in e.args]}


@router.post("/script")
def script(body: dict):
    """Run (op, key, value) steps on one cache; after each, the outcome and the cache's state."""
    try:
        c = _make(body["kind"], body["maxsize"], body.get("ttl", 3600), body.get("sized", False))
    except Exception as e:  # noqa: BLE001
        return {"init": _err(e)}
    out = []
    for op, k, v in body["ops"]:
        try:
            if op == "set":
                c[k] = v
                r = None
            elif op == "get":
                r = c[k]
            elif op == "getm":
                r = c.get(k, "missing")
            elif op == "get1":
                r = c.get(k)
            elif op == "del":
                del c[k]
                r = None
            elif op == "in":
                r = k in c
            elif op == "pop":
                r = c.pop(k)
            elif op == "popd":
                r = c.pop(k, "dflt")
            elif op == "setdefault":
                r = c.setdefault(k, v)
            elif op == "popitem":
                r = list(c.popitem())
            elif op == "clear":
                c.clear()
                r = None
            elif op == "update":
                c.update({k: v, "u2": v})
                r = None
            elif op == "update_kw":
                c.update(kw=v)
                r = None
            elif op == "expire":
                r = [list(p) for p in c.expire()]
            elif op == "len":
                r = len(c)
            elif op == "bool":
                r = bool(c)
            elif op == "eq":
                r = [c == dict(c.items()), c == {}, c == {"x": 1}]
            else:
                r = "?"
            out.append([op, r if not isinstance(r, (bytes,)) else repr(r), list(c), len(c), c.currsize, repr(c)])
        except Exception as e:  # noqa: BLE001
            out.append([op, _err(e), list(c), len(c), c.currsize, repr(c)])
    return {"out": out, "maxsize": c.maxsize, "keys": list(c.keys()), "values": list(c.values()),
            "items": [list(i) for i in c.items()], "ttl": getattr(c, "ttl", None), "size": c.getsizeof([1, 2, 3]),
            "types": [type(c).__name__, isinstance(c, Cache), isinstance(c, TTLCache), isinstance(c, LRUCache)]}


@router.get("/errors/{what}")
def errors(what: str):
    try:
        if what == "negative":
            return repr(Cache(-1))
        if what == "too_large":
            c = Cache(2, getsizeof=len)
            c["k"] = "abc"
        if what == "kw":
            return repr(LRUCache(maxsize=3, getsizeof=None))
        if what == "missing":
            return repr(TTLCache(3))
        if what == "popitem_empty":
            return list(Cache(3).popitem())
        if what == "popitem_empty_lru":
            return list(LRUCache(3).popitem())
        if what == "popitem_empty_ttl":
            return list(TTLCache(3, 10).popitem())
        if what == "float_max":
            c = LRUCache(2.5)
            c[1] = 1
            c[2] = 2
            c[3] = 3
            return [repr(c), c.maxsize]
        if what == "unhashable":
            Cache(3)[[1]] = 1
        if what == "zero":
            c = Cache(0)
            c["a"] = 1
    except Exception as e:  # noqa: BLE001
        return {"error": _err(e)}
    return None


@cached(cache=TTLCache(maxsize=4, ttl=3600))
def square(x, scale=1):
    CALLS.append(("square", x, scale))
    return x * x * scale


@cached(LRUCache(maxsize=2), info=True)
def lru_fn(x):
    CALLS.append(("lru", x))
    return [x]


@cached(cache={}, key=lambda a, b=0: (a, "k"))
def custom_key(a, b=0):
    CALLS.append(("custom", a, b))
    return a + b


@cached(cache=Cache(10), key=keys.hashkey, lock=threading.Lock(), info=True)
def locked(x):
    CALLS.append(("locked", x))
    return {"x": x}


@cached(cache=None, info=True)
def uncached(x):
    CALLS.append(("uncached", x))
    return x


@cached(TTLCache(maxsize=10, ttl=0))
def expiring(x):
    CALLS.append(("expiring", x))
    return x


@cached(Cache(maxsize=3, getsizeof=len))
def sized_fn(s):
    CALLS.append(("sized", s))
    return s


@lru_cache(maxsize=2)
def func_lru(x):
    CALLS.append(("func_lru", x))
    return x + 1


@lru_cache
def func_lru_bare(x):
    CALLS.append(("func_lru_bare", x))
    return x + 2


@ttl_cache(maxsize=None, ttl=3600)
def func_ttl(x):
    CALLS.append(("func_ttl", x))
    return x * 3


@fifo_cache(maxsize=None)
def func_fifo(x):
    CALLS.append(("func_fifo", x))
    return x * 4


@router.post("/decorated")
def decorated(body: dict):
    """Calls of the decorated functions; their cache_info(), cache, clear; what the functions ran."""
    CALLS.clear()
    out = []
    for name, args, kwargs in body["calls"]:
        f = {"square": square, "lru": lru_fn, "custom": custom_key, "locked": locked, "uncached": uncached,
             "expiring": expiring, "sized": sized_fn, "func_lru": func_lru, "func_lru_bare": func_lru_bare,
             "func_ttl": func_ttl, "func_fifo": func_fifo}[name]
        try:
            if args == "clear":
                f.cache_clear()
                out.append("cleared")
                continue
            out.append(f(*args, **kwargs))
        except Exception as e:  # noqa: BLE001
            out.append(_err(e))
    infos = []
    for f in (lru_fn, locked, uncached, func_lru, func_lru_bare, func_ttl, func_fifo):
        infos.append([repr(f.cache_info()), list(f.cache_info()), f.cache_info().hits])
    params = [func_lru.cache_parameters(), func_ttl.cache_parameters(), func_fifo.cache_parameters()]
    return {"out": out, "calls": [list(c) for c in CALLS], "infos": infos, "params": params,
            "caches": [repr(square.cache), repr(lru_fn.cache), repr(custom_key.cache), repr(locked.cache), repr(sized_fn.cache),
                       repr(func_lru.cache), repr(func_ttl.cache), repr(func_fifo.cache)],
            "meta": [square.__name__, square.__wrapped__.__name__, square.cache_info is None, uncached.cache is None,
                     repr(square.cache_key(1, scale=2)), repr(keys.hashkey(1, 2, b=3, a=4)), repr(keys.methodkey("self", 1, z=2)),
                     func_ttl.cache.maxsize]}


class Repo:
    """@cachedmethod: an instance's cache, the default methodkey, a custom key, a lock taking the instance."""

    def __init__(self, name, size):
        self.name = name
        self.cache = LRUCache(maxsize=size)
        self.other = {}
        self.lock = threading.Lock()
        self.ran = []

    @cachedmethod(lambda self: self.cache)
    def find(self, x, y=0):
        self.ran.append(["find", x, y])
        return [self.name, x, y]

    @cachedmethod(lambda self: self.other, key=lambda self, x: (self.name, x))
    def label(self, x):
        self.ran.append(["label", x])
        return f"{self.name}:{x}"

    @cachedmethod(cache=lambda self: self.cache, key=keys.methodkey, lock=lambda self: self.lock)
    def locked(self, x):
        self.ran.append(["locked", x])
        return x * 10


@router.post("/methods")
def cached_methods(body: dict):
    a, b = Repo("a", body.get("size", 2)), Repo("b", body.get("size", 2))
    out = []
    for who, meth, args in body["calls"]:
        r = a if who == "a" else b
        try:
            f = getattr(r, meth)
            out.append(f(*args))
        except Exception as e:  # noqa: BLE001
            out.append(_err(e))
    direct = [a.find(1), a.find(1, y=2), a.find(1, 2), a.label(3), b.label(3), a.locked(4), a.locked(4)]
    return {"out": out, "direct": direct, "ran": [a.ran, b.ran], "caches": [repr(a.cache), repr(b.cache), repr(a.other), repr(b.other)]}
