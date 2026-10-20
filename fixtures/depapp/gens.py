"""`yield` dependencies whose `yield` sits under `with`, `async with`, `if` and nested `try` blocks."""
from typing import Annotated

from fastapi import APIRouter, Depends, HTTPException

from .state import EVENTS

router = APIRouter(prefix="/g")


class Resource:
    """A project context manager: records how it is left (the request's exception thrown in at the `yield`)."""

    def __init__(self, name: str, swallow: bool = False):
        self.name = name
        self.swallow = swallow

    def __enter__(self):
        EVENTS.append(f"enter {self.name}")
        return self

    def __exit__(self, typ, exc, tb):
        EVENTS.append(f"exit {self.name} {typ.__name__ if typ else None} {getattr(exc, 'status_code', exc)!r}")
        return self.swallow


class AResource:
    def __init__(self, name: str):
        self.name = name

    async def __aenter__(self):
        EVENTS.append(f"aenter {self.name}")
        return {"res": self.name}

    async def __aexit__(self, typ, exc, tb):
        EVENTS.append(f"aexit {self.name} {typ.__name__ if typ else None}")
        return False


async def in_async_with():
    async with AResource("db") as conn:
        yield conn


def in_with():
    with Resource("file") as r:
        yield r.name


def in_with_try():
    with Resource("outer"):
        try:
            yield "x"
        except ValueError as e:
            EVENTS.append(f"caught {e}")
            raise HTTPException(409, "conflict from the dependency")
        finally:
            EVENTS.append("finally inner")


def swallowing():
    with Resource("quiet", swallow=True):
        yield 1


async def in_if(mode: str = "a"):
    if mode == "a":
        async with AResource("a"):
            yield "branch a"
    elif mode == "b":
        yield "branch b"
    else:
        EVENTS.append("no yield")


@router.post("/awith")
async def awith(conn: dict = Depends(in_async_with)):
    return conn


@router.post("/awith/fail")
async def awith_fail(conn: Annotated[dict, Depends(in_async_with)]):
    raise HTTPException(418, "teapot")


@router.post("/with")
def with_(name: str = Depends(in_with)):
    return {"name": name}


@router.post("/with/fail/{kind}")
def with_fail(kind: str, name: str = Depends(in_with)):
    if kind == "value":
        raise ValueError("bad value")
    raise HTTPException(400, f"failed {kind}")


@router.post("/wtry/{kind}")
def wtry(kind: str, v: str = Depends(in_with_try)):
    if kind == "value":
        raise ValueError("bad")
    if kind == "http":
        raise HTTPException(422, "plain")
    return {"v": v}


@router.post("/swallow/{kind}")
def swallow(kind: str, v: int = Depends(swallowing)):
    if kind == "fail":
        raise HTTPException(400, "swallowed by the dependency")
    return {"v": v}


@router.post("/if")
async def if_(v: str = Depends(in_if)):
    return {"v": v}
