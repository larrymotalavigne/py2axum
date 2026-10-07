"""Project decorators (@raise_504_on_failure-like wrappers, step registries, factories): functools.wraps, closures,
*args/**kwargs, function attributes, inspect.iscoroutinefunction."""
import functools
import inspect
import typing
from types import GenericAlias
from functools import wraps

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel, TypeAdapter

router = APIRouter(prefix="/decos")

CALLS: list[str] = []


class Flaky(Exception):
    pass


def raise_504_on_failure(func):
    @wraps(func)
    async def wrapper(*args, **kwargs):
        try:
            return await func(*args, **kwargs)
        except (TimeoutError, Flaky) as e:
            raise HTTPException(status_code=504, detail=f"Service timeout or error: {e}")

    return wrapper


def traced(label: str, *, sep: str = ":"):
    """A decorator factory."""
    def decorator(func):
        @functools.wraps(func)
        async def wrapper(*args, **kwargs):
            CALLS.append(f"{label}{sep}enter{sep}{func.__name__}")
            out = await func(*args, **kwargs)
            CALLS.append(f"{label}{sep}exit")
            return out

        return wrapper

    return decorator


def bare(func):
    """No functools.wraps: the wrapper keeps its own name."""
    def inner(x, y=10, *rest, scale=1, **extra):
        return {"sum": (func(x, y) + sum(rest)) * scale, "extra": sorted(extra)}

    return inner


def step(kind: str):
    def decorator(func):
        async def wrapped(a: int, b: int):
            try:
                return {"kind": kind, "value": await func(a=a, b=b)}
            except ZeroDivisionError as e:
                return {"kind": kind, "error": str(e)}

        return wrapped

    return decorator


@raise_504_on_failure
async def fetch(n: int, fail: bool = False) -> dict:
    """Fetch something.

    Indented docstring.
    """
    if fail:
        raise Flaky(f"boom {n}")
    return {"n": n}


@traced("outer")
@traced("inner", sep="/")
async def compute(x: int, *, double: bool = False) -> int:
    return x * 2 if double else x


@bare
def add(a, b):
    return a + b


@step("div")
async def divide(a: int, b: int) -> float:
    return a / b


async def plain(x):
    return x


def sync_plain(x):
    return x


@router.get("/fetch")
async def fetch_route(n: int = 1, fail: bool = False):
    return await fetch(n, fail=fail)


@router.get("/attrs")
async def attrs():
    return {
        "name": fetch.__name__,
        "qualname": fetch.__qualname__,
        "module": fetch.__module__,
        "doc": fetch.__doc__,
        "wrapped": fetch.__wrapped__.__name__,
        "wrapped_wrapped": hasattr(fetch.__wrapped__, "__wrapped__"),
        "compute_wrapped": compute.__wrapped__.__wrapped__.__name__,
        "bare": add.__name__,
        "bare_qual": add.__qualname__,
        "bare_doc": add.__doc__,
        "bare_has_wrapped": hasattr(add, "__wrapped__"),
        "plain": plain.__name__,
        "plain_doc": plain.__doc__,
        "coro": [inspect.iscoroutinefunction(f) for f in (fetch, compute, add, divide, plain, sync_plain)],
    }


@router.get("/compute")
async def compute_route(x: int = 3):
    CALLS.clear()
    r1 = await compute(x)
    r2 = await compute(x=x, double=True)
    return {"r": [r1, r2], "calls": list(CALLS)}


@router.get("/bare")
async def bare_route():
    out = [add(1), add(1, 2), add(1, 2, 3, 4, scale=2), add(1, z=1, a2=2)]
    errs = []
    for f in (lambda: add(), lambda: add(1, scale=1, x=2), lambda: compute.__wrapped__.__wrapped__(1, 2)):
        try:
            f()
        except TypeError as e:
            errs.append(str(e))
    return {"out": out, "errs": errs}


@router.get("/step")
async def step_route(a: int = 6, b: int = 3):
    return await divide(a, b)


@router.get("/nested")
async def nested(k: int = 2):
    base = k * 10

    def scale(v, factor=k, *more, **named):
        return [v * factor + base, list(more), dict(named)]

    async def later(v):
        return scale(v, 3)

    f = scale
    g = functools.wraps(scale)(lambda v: v)
    return {
        "a": scale(1),
        "b": scale(1, 2, 3, 4, q=5),
        "c": await later(2),
        "d": f(v=4),
        "name": scale.__qualname__,
        "lam": [g.__name__, g.__qualname__, g.__wrapped__(7)[0]],
        "dflt": scale.__name__,
    }


class Item(BaseModel):
    id: int
    tags: list[str] = []


def _is_model(tp):
    return inspect.isclass(tp) and issubclass(tp, BaseModel)


def _contains(tp):
    """A caching decorator's question: does a return type contain a Pydantic model?"""
    if _is_model(tp):
        return True
    args = typing.get_args(tp)
    return any(_contains(a) for a in args) if args else False


def revalidate(func):
    """Builds a TypeAdapter from the function's return annotation (typing.get_type_hints)."""
    ret = typing.get_type_hints(func).get("return")
    adapter = TypeAdapter(ret) if ret is not None and _contains(ret) else None

    @wraps(func)
    async def wrapper(*args, **kwargs):
        raw = await func(*args, **kwargs)
        return adapter.validate_python(raw) if adapter is not None else raw

    wrapper.has_adapter = adapter is not None
    return wrapper


@revalidate
async def items(n: int) -> list[Item]:
    return [{"id": i, "tags": ["t"] * i} for i in range(n)]


@revalidate
async def count(n: int) -> int:
    return n


@router.get("/types")
async def types_route():
    session_types = tuple(t for t in (BaseModel, None) if t is not None)
    got = await items(2)
    return {
        "hints": sorted(typing.get_type_hints(items)),
        "adapters": [items.has_adapter, count.has_adapter],
        "items": got,
        "is_model": [isinstance(got[0], session_types), isinstance(3, session_types), isinstance(got[0], BaseModel)],
        "contains": [_contains(t) for t in (Item, list[Item], int, dict[str, Item], Item | None, list[int])],
        "generic": [issubclass(type(list[Item]), GenericAlias), issubclass(type(Item), GenericAlias), inspect.isclass(list[Item])],
        "dyn": TypeAdapter(list[Item]).validate_python([{"id": "7"}]),
        "count": await count(4),
    }
