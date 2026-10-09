"""Constructions of the FastAPI documentation lifted for the 0.6 (corpus/): the request's exception raised
at a dependency's `yield` (try/except around it), Depends(scope="function"), unannotated parameters, a Response
class as the return annotation, response_model_include/exclude/exclude_unset/exclude_none, an instance with
`__call__` as a dependency, val_json_bytes/ser_json_bytes, anyio.sleep."""
from typing import Annotated

import anyio
from fastapi import APIRouter, Depends, Header, HTTPException, Response
from collections.abc import AsyncIterable, Iterable

from fastapi.responses import JSONResponse, RedirectResponse, StreamingResponse
from fastapi.sse import EventSourceResponse, ServerSentEvent
from pwdlib import PasswordHash
from pydantic import BaseModel

router = APIRouter(prefix="/corpusx")

LOG: list[str] = []


class OwnerError(Exception):
    pass


class InternalError(Exception):
    pass


def owner_dep():
    try:
        yield "Rick"
    except OwnerError as e:
        raise HTTPException(status_code=400, detail=f"Owner error: {e}")


def swallow_dep():
    try:
        yield "Rick"
    except InternalError:
        LOG.append("swallowed")


def reraise_dep():
    try:
        yield "Rick"
    except InternalError:
        LOG.append("reraised")
        raise


async def any_dep():
    try:
        yield "any"
    except HTTPException as e:
        raise HTTPException(status_code=409, detail={"was": e.status_code, "detail": e.detail})
    except Exception as e:
        raise HTTPException(status_code=418, detail=type(e).__name__)
    else:
        LOG.append("else")
    finally:
        LOG.append("finally")


# function scope: the exit code runs before the response is sent (a following /log never races with it)
async def outer_dep(inner: Annotated[str, Depends(any_dep, scope="function")]):
    try:
        yield inner + "+outer"
    except HTTPException as e:
        LOG.append(f"outer saw {e.status_code}")
        raise


@router.get("/owner/{item}")
def owner(item: str, username: Annotated[str, Depends(owner_dep)]):
    if item == "plumbus":
        raise OwnerError(username)
    if item == "missing":
        raise HTTPException(status_code=404, detail="Item not found")
    return {"item": item, "owner": username}


@router.get("/swallow/{item}")
def swallow(item: str, username: Annotated[str, Depends(swallow_dep)]):
    if item == "portal-gun":
        raise InternalError(username)
    if item != "plumbus":
        raise HTTPException(status_code=404, detail="only a plumbus here")
    return item


@router.get("/reraise/{item}")
def reraise(item: str, username: Annotated[str, Depends(reraise_dep)]):
    if item == "portal-gun":
        raise InternalError(username)
    return item


@router.get("/any/{mode}")
async def any_route(mode: str, v: Annotated[str, Depends(outer_dep, scope="function")], n: int = 0):
    LOG.clear()
    if mode == "http":
        raise HTTPException(status_code=403, detail="no")
    if mode == "value":
        raise ValueError("bad")
    if mode == "key":
        return {"k": {}["missing"]}
    return {"v": v, "n": n}


@router.get("/log")
async def log():
    out = list(LOG)
    LOG.clear()
    return out


def fn_scoped():
    LOG.append("open")
    try:
        yield "fn"
    finally:
        LOG.append("closed")


def stream_log():
    yield ",".join(LOG)


@router.get("/scope/function")
def scope_function(v: Annotated[str, Depends(fn_scoped, scope="function")]):
    LOG.clear()
    LOG.append(v)
    return StreamingResponse(stream_log(), media_type="text/plain")


@router.get("/scope/request")
def scope_request(v: Annotated[str, Depends(fn_scoped, scope="request")]):
    LOG.clear()
    LOG.append(v)
    return StreamingResponse(stream_log(), media_type="text/plain")


def fn_raises():
    yield 1
    raise HTTPException(status_code=503, detail="closing failed")


@router.get("/scope/raises", dependencies=[Depends(fn_raises, scope="function")])
def scope_raises():
    return {"ok": True}


@router.get("/untyped/{item_id}")
def untyped(item_id, q=None, n=3):
    return {"item_id": item_id, "q": q, "n": n, "types": [type(item_id).__name__, type(q).__name__, type(n).__name__]}


@router.get("/teleport")
async def teleport(to: str = "/corpusx/log") -> RedirectResponse:
    return RedirectResponse(url=to)


@router.get("/json-ann")
async def json_ann() -> JSONResponse:
    return {"plain": True}


@router.get("/resp-ann")
async def resp_ann(raw: bool = False) -> Response:
    if raw:
        return Response(content="raw", media_type="text/plain")
    return {"k": [1, 2]}


class Item(BaseModel):
    name: str
    description: str | None = None
    price: float
    tax: float = 10.5
    tags: list[str] = []


ITEMS = {
    "foo": {"name": "Foo", "price": 50.2},
    "bar": {"name": "Bar", "description": "The bartenders", "price": 62, "tax": 20.2},
    "baz": {"name": "Baz", "description": None, "price": 50.2, "tax": 10.5, "tags": []},
}


@router.get("/rm/unset/{key}", response_model=Item, response_model_exclude_unset=True)
async def rm_unset(key: str):
    return ITEMS[key]


@router.get("/rm/unset-model/{key}", response_model=Item, response_model_exclude_unset=True)
async def rm_unset_model(key: str):
    return Item(**ITEMS[key])


@router.get("/rm/none/{key}", response_model=Item, response_model_exclude_none=True)
async def rm_none(key: str):
    return ITEMS[key]


@router.get("/rm/include/{key}", response_model=Item, response_model_include={"name", "description"})
async def rm_include(key: str):
    return ITEMS[key]


@router.get("/rm/exclude/{key}", response_model=Item, response_model_exclude=["tax"], status_code=201)
async def rm_exclude(key: str):
    return ITEMS[key]


@router.get("/rm/ann/{key}", response_model_exclude={"tags", "price"}, response_model_exclude_unset=True)
async def rm_ann(key: str) -> Item:
    return Item(**ITEMS[key])


class FixedChecker:
    def __init__(self, fixed: str):
        self.fixed = fixed

    def __call__(self, q: str = ""):
        if q:
            return self.fixed in q
        return False


xchecker = FixedChecker("x")


class AsyncChecker:
    def __init__(self, n: int):
        self.n = n

    async def __call__(self, k: Annotated[bool, Depends(xchecker)], m: int = 1):
        await anyio.sleep(0)
        return {"k": k, "m": m * self.n}


checker = FixedChecker("bar")
achecker = AsyncChecker(10)


@router.get("/checker")
async def check(included: Annotated[bool, Depends(checker)], other: Annotated[dict, Depends(achecker)]):
    return {"included": included, "other": other}


@router.get("/checker-pre", dependencies=[Depends(achecker)])
async def check_pre():
    return {"ok": True}


class B64(BaseModel):
    description: str
    data: bytes
    model_config = {"val_json_bytes": "base64", "ser_json_bytes": "base64"}


class HexIn(BaseModel):
    data: bytes
    more: list[bytes] = []
    opt: bytes | None = None
    inner: B64 | None = None
    model_config = {"val_json_bytes": "hex"}


class Plain(BaseModel):
    data: bytes
    inner: B64


@router.post("/bytes/b64")
def bytes_b64(body: B64) -> B64:
    return body


@router.post("/bytes/hex")
def bytes_hex(body: HexIn):
    return {"data": list(body.data), "more": [list(m) for m in body.more], "opt": body.opt is None,
            "inner": body.inner}


@router.post("/bytes/plain")
def bytes_plain(body: Plain) -> Plain:
    return body


@router.get("/bytes/out")
def bytes_out(raw: str = "hi"):
    return B64(description="d", data=raw.encode() + b"\xfe\xff")


password_hash = PasswordHash.recommended()
SECRET_HASH = "$argon2id$v=19$m=65536,t=3,p=4$wagCPXjifgvUFBzq4hqe3w$CYaIb8sB+wtD+Vu/P4uod1+Qof8h+1g7bbDlBID48Rc"
WEAK_HASH = "$argon2id$v=19$m=8,t=1,p=1$93e8vKTXJdAeAb8AwtESig$X/9e4bAyrW6o1118TMk6yAqUlabRaUEqArp+jLy4VoM"


@router.get("/pwd")
def pwd(password: str = "secret", which: str = "secret"):
    h = {"secret": SECRET_HASH, "weak": WEAK_HASH, "bcrypt": "$2b$12$abc", "own": password_hash.hash(password)}[which]
    ok, updated = password_hash.verify_and_update(password, h)
    return {"verify": password_hash.verify(password, h), "ok": ok, "rehashed": updated is not None,
            "prefix": password_hash.hash(password.encode())[:31], "verify_bytes": password_hash.verify(password.encode(), h)}


# generator endpoints (FastAPI >= 0.134): JSON Lines, Server-Sent Events, raw StreamingResponse
class Point(BaseModel):
    x: int
    label: str = "p"


@router.get("/gen/jsonl")
async def gen_jsonl(n: int = 3) -> AsyncIterable[Point]:
    for i in range(n):
        yield {"x": i, "label": "été" if i % 2 else "p", "extra": True}


@router.get("/gen/jsonl-bad")
async def gen_jsonl_bad() -> AsyncIterable[Point]:
    yield {"x": 1}
    yield {"x": "nope"}


@router.get("/gen/jsonl-any", status_code=201)
def gen_jsonl_any(response: Response):
    response.headers["x-gen"] = "1"
    yield {"é": [1, 2.5, None]}
    yield "s"
    yield Point(x=3)


@router.get("/gen/sse", response_class=EventSourceResponse)
async def gen_sse(last_event_id: Annotated[int | None, Header()] = None) -> AsyncIterable[ServerSentEvent]:
    yield ServerSentEvent(comment="start\nsecond line")
    for i in range(3):
        if last_event_id is not None and i <= last_event_id:
            continue
        yield ServerSentEvent(data=Point(x=i), event="point", id=str(i), retry=1000)
    yield ServerSentEvent(data={"k": "ü"}, event="dict")
    yield ServerSentEvent(data="text")
    yield ServerSentEvent(raw_data="raw\r\nmulti", event="done")


@router.get("/gen/sse-items", response_class=EventSourceResponse)
def gen_sse_items() -> Iterable[Point]:
    yield Point(x=1)
    yield {"x": 2}


@router.get("/gen/sse-plain", response_class=EventSourceResponse)
async def gen_sse_plain():
    yield {"a": 1}
    yield [1, "deux"]
    yield ServerSentEvent()


@router.get("/gen/sse-bad", response_class=EventSourceResponse)
async def gen_sse_bad(kind: str = "both"):
    yield ServerSentEvent(data="ok")
    if kind == "both":
        yield ServerSentEvent(data="a", raw_data="b")
    else:
        yield ServerSentEvent(event="a\nb")


@router.get("/gen/raw", response_class=StreamingResponse)
async def gen_raw() -> AsyncIterable[str]:
    yield "line one\n"
    yield b"bytes\n"
    yield "end"


@router.get("/gen/raw-bad", response_class=StreamingResponse)
def gen_raw_bad():
    yield "a"
    yield 3
