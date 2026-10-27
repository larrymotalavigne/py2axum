"""Cases of the silently wrong values of 17/10/2026 (frozenset held as a set, a TypeAdapter's error title, model_type
against model_attributes_type, bytes.decode(errors=), what Starlette's Request offers): each one is the exact value or
a refusal at generation, never a wrong value. Every route answers {"ok": value} or {"err": "Type: message"}."""
import collections
import json
import pickle
from typing import Optional, Union

from fastapi import APIRouter, Request
from pydantic import BaseModel, TypeAdapter, ValidationError

router = APIRouter(prefix="/silent")


class Sub(BaseModel):
    x: int


class Outer(BaseModel):
    s: Sub
    n: Optional[Sub] = None


class Tags(BaseModel):
    names: frozenset[int]
    plain: set[int] = set()


def fail(e: Exception):
    return {"err": f"{type(e).__name__}: {e}"}


@router.get("/frozen/{what}")
async def frozen(what: str):
    fs = frozenset([3])
    s = {3}
    two = frozenset([1, 2])
    try:
        if what == "type":
            return {"ok": [type(fs).__name__, type(s).__name__, type(fs | s).__name__, type(s | fs).__name__,
                           type(fs & s).__name__, type(s - fs).__name__, type(fs ^ s).__name__, type(fs.union(s)).__name__,
                           type(fs.copy()).__name__, type(frozenset(fs)).__name__, type(frozenset()).__name__]}
        if what == "isinstance":
            return {"ok": [isinstance(fs, frozenset), isinstance(fs, set), isinstance(s, frozenset), isinstance(s, set)]}
        if what == "repr":
            return {"ok": [repr(fs), str(fs), f"{fs}", repr(frozenset()), repr(set()), repr(frozenset(["a"])), repr({"k": fs})]}
        if what == "eq":
            return {"ok": [fs == s, s == fs, fs == frozenset([3]), fs != frozenset([4]), fs <= two, bool(frozenset()), bool(fs), len(two), 1 in two, 5 in two]}
        if what == "iter":
            return {"ok": [sorted(two), list(frozenset([7])), max(two), sum(two), sorted(x * 2 for x in two)]}
        if what == "add":
            fs.add(1)  # type: ignore[attr-defined]
        if what == "update":
            fs.update([1])  # type: ignore[attr-defined]
        if what == "remove":
            fs.discard(1)  # type: ignore[attr-defined]
        if what == "methods":
            return {"ok": [sorted(two.union([5])), sorted(two.intersection([2, 9])), sorted(two.difference([1])), sorted(two.symmetric_difference([2, 3])),
                           two.issubset({1, 2, 3}), two.issuperset([1]), two.isdisjoint([8])]}
        if what == "key":
            d = {frozenset([1, 2]): "a", frozenset(["x"]): "b"}
            return {"ok": [d[frozenset([2, 1])], frozenset(["x"]) in d, len(d), len({frozenset([1]), frozenset([1]), frozenset([2])})]}
        if what == "setkey":
            return {"ok": len({s})}
        if what == "json":
            return {"ok": two}
        if what == "dumps":
            return {"ok": json.dumps(two)}
        if what == "pickle":
            back = pickle.loads(pickle.dumps(two))
            return {"ok": [type(back).__name__, sorted(back), pickle.dumps(frozenset([1])).hex()]}
        if what == "model":
            m = Tags(names=[3, 1, 2], plain=[5])
            return {"ok": [type(m.names).__name__, type(m.plain).__name__, sorted(m.names), repr(Tags(names=frozenset([4])).names),
                           json.dumps(sorted(json.loads(m.model_dump_json())["names"])), type(m.model_dump()["names"]).__name__]}
        if what == "modelerr":
            Tags(names="x")
        if what == "fromfrozen":
            return {"ok": [sorted(Tags(names=frozenset([2, 1]), plain=frozenset([9])).plain), type(Tags(names=[1], plain=frozenset([9])).plain).__name__]}
        if what == "hash":
            return {"ok": [hash(frozenset([1, 2])) == hash(frozenset([2, 1])), isinstance(hash(fs), int)]}
        if what == "set":
            return {"ok": [type(s | {1}).__name__, type({1, 2} ^ {2, 3}).__name__, sorted({1, 2} ^ {2, 3})]}
        if what == "counter":
            return {"ok": sorted(collections.Counter(two).items())}
    except Exception as e:  # noqa: BLE001
        return fail(e)
    return {"ok": None}


ADAPTER_CASES = {
    "int": int, "str": str, "float": float, "bool": bool, "listint": list[int], "setint": set[int], "dict": dict[str, int],
    "tuple": tuple[int, str], "tuplevar": tuple[int, ...], "opt": Optional[int], "union": Union[int, str], "listsub": list[Sub], "sub": Sub,
    "listopt": list[Optional[int]], "frozen": frozenset[int],
}


@router.get("/adapter/{what}")
async def adapter(what: str):
    try:
        if what == "listint":
            TypeAdapter(list[int]).validate_python(["a", 2, "c"])
        if what == "int":
            TypeAdapter(int).validate_python("x")
        if what == "str":
            TypeAdapter(str).validate_python(5)
        if what == "float":
            TypeAdapter(float).validate_python("zz")
        if what == "bool":
            TypeAdapter(bool).validate_python("maybe")
        if what == "setint":
            TypeAdapter(set[int]).validate_python(["a"])
        if what == "dict":
            TypeAdapter(dict[str, int]).validate_python({"a": "x"})
        if what == "tuple":
            TypeAdapter(tuple[int, str]).validate_python(("a", 1))
        if what == "tuplevar":
            TypeAdapter(tuple[int, ...]).validate_python(("a",))
        if what == "opt":
            TypeAdapter(Optional[int]).validate_python("x")
        if what == "union":
            TypeAdapter(Union[int, str]).validate_python(None)
        if what == "listsub":
            TypeAdapter(list[Sub]).validate_python([{"x": "a"}, 5])
        if what == "sub":
            TypeAdapter(Sub).validate_python({"x": "a"})
        if what == "listopt":
            TypeAdapter(list[Optional[int]]).validate_python(["a"])
        if what == "frozen":
            TypeAdapter(frozenset[int]).validate_python(["a"])
        if what == "json":
            TypeAdapter(list[int]).validate_json(b"[1, \"a\"]")
        if what == "errors":
            try:
                TypeAdapter(list[int]).validate_python(["a"])
            except ValidationError as e:
                return {"ok": [e.title, e.error_count(), [x["type"] for x in e.errors()]]}
        if what == "ok":
            return {"ok": [TypeAdapter(list[int]).validate_python(["1", 2]), TypeAdapter(Sub).validate_python({"x": 3}).x]}
    except ValidationError as e:
        return {"err": f"{type(e).__name__}: {e}"}
    except Exception as e:  # noqa: BLE001
        return fail(e)
    return {"ok": None}


@router.post("/modeltype/{what}")
async def modeltype(what: str, body: dict):
    try:
        v = body.get("v")
        if what == "outer":
            Outer.model_validate({"s": v})
        if what == "optional":
            Outer.model_validate({"s": {"x": 1}, "n": v})
        if what == "top":
            Outer.model_validate(v)
        if what == "init":
            Outer(s=v)
        if what == "attrs":
            Outer.model_validate({"s": v}, from_attributes=True)
        if what == "adapter":
            TypeAdapter(Outer).validate_python({"s": v})
        if what == "ok":
            return {"ok": Outer.model_validate({"s": {"x": 1}}).model_dump()}
    except ValidationError as e:
        return {"err": [(x["type"], x["loc"], x["msg"]) for x in e.errors()]}
    except Exception as e:  # noqa: BLE001
        return fail(e)
    return {"ok": None}


@router.get("/decode/{what}")
async def decode(what: str):
    bad = b"ab\xffcd\xe2\x82"
    ok = "é€".encode()
    try:
        if what == "strict":
            return {"ok": bad.decode()}
        if what == "replace":
            return {"ok": [bad.decode("utf-8", "replace"), bad.decode(errors="replace"), b"\xe0\x80\x80x".decode(errors="replace"), b"\xf0\x9f".decode(errors="replace")]}
        if what == "ignore":
            return {"ok": [bad.decode(errors="ignore"), b"\xff\xfe".decode("utf-8", errors="ignore")]}
        if what == "backslash":
            return {"ok": [bad.decode(errors="backslashreplace"), b"\xc3".decode("utf-8", "backslashreplace")]}
        if what == "ascii":
            return {"ok": [ok.decode("ascii", "replace"), ok.decode("ascii", errors="ignore"), ok.decode("ascii", "backslashreplace")]}
        if what == "latin":
            return {"ok": [ok.decode("latin-1", "replace"), ok.decode("latin1")]}
        if what == "valid":
            return {"ok": [ok.decode(errors="replace"), ok.decode("utf-8", "ignore"), ok.decode(errors="strict")]}
        if what == "sig":
            return {"ok": [b"\xef\xbb\xbfhi\xff".decode("utf-8-sig", "replace"), str(b"a\xffb", "utf-8", "replace"), str(b"a\xffb", "ascii", "ignore")]}
        if what == "encode":
            t = "aé€😀b"
            return {"ok": [t.encode("ascii", "replace").decode(), t.encode("ascii", "ignore").decode(), t.encode("ascii", "backslashreplace").decode(),
                           t.encode("ascii", "xmlcharrefreplace").decode(), t.encode("latin-1", "backslashreplace").decode(), t.encode("latin-1", "xmlcharrefreplace").decode()]}
        if what == "encodestrict":
            return {"ok": "é".encode("ascii", "strict")}
        if what == "surrogate":
            return {"ok": "é€".encode("ascii", "surrogateescape")}
    except Exception as e:  # noqa: BLE001
        return fail(e)
    return {"ok": None}


@router.get("/req_basic")
async def req_basic(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": [request.method, request.url.path, request.url.query, request.url.scheme, request.url.hostname, str(request.url).replace(host, "H"), str(request.base_url).replace(host, "H"), type(request.client.host).__name__]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_url")
async def req_url(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": [request.url.path, request.url.query, request.url.fragment, request.url.scheme, request.url.netloc.replace(host, "H"), request.url.port is not None, request.url.username, request.url.password]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_url_cmp")
async def req_url_cmp(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": [request.url == str(request.url), repr(request.url).replace(host, "H"), request.url == request.url]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_query_dict")
async def req_query_dict(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": [dict(request.query_params), list(request.query_params), len(request.query_params), bool(request.query_params)]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_query_get")
async def req_query_get(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": [request.query_params.get("a"), request.query_params["a"], request.query_params.get("zz"), request.query_params.get("zz", "d"), "a" in request.query_params]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_query_iter")
async def req_query_iter(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": [list(request.query_params.keys()), list(request.query_params.values()), list(request.query_params.items())]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_query_neg")
async def req_query_neg(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": request.query_params["zz"]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_headers_get")
async def req_headers_get(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": [request.headers.get("x-a"), request.headers.get("X-A"), request.headers["x-a"], "x-a" in request.headers, request.headers.get("zz", "d")]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_headers_type")
async def req_headers_type(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": type(request.headers).__name__}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_headers_getlist")
async def req_headers_getlist(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": request.headers.getlist("x-multi")}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_headers_multi")
async def req_headers_multi(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": request.headers.get("x-multi")}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_headers_keys")
async def req_headers_keys(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": [k for k in request.headers.keys() if k.startswith("x-")]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_headers_items")
async def req_headers_items(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": sorted(k for k, v in request.headers.items() if k.startswith("x-"))}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_headers_values")
async def req_headers_values(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": sorted(v for v in request.headers.values() if v.startswith("val"))}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_headers_iter")
async def req_headers_iter(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": [k for k in request.headers if k.startswith("x-")]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_headers_dict")
async def req_headers_dict(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": dict(request.headers).get("x-a")}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_headers_len")
async def req_headers_len(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": len(request.headers) > 3}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_headers_neg")
async def req_headers_neg(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": request.headers["zz"]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_headers_repr")
async def req_headers_repr(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": repr(request.headers)[:8]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_cookies")
async def req_cookies(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": [request.cookies.get("a"), request.cookies["b"], dict(request.cookies), len(request.cookies), "a" in request.cookies, sorted(request.cookies), type(request.cookies).__name__]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_cookies_neg")
async def req_cookies_neg(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": request.cookies["zz"]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_client")
async def req_client(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": [request.client.host, isinstance(request.client.port, int), request.client[0], len(request.client), repr(request.client).startswith("Address(host=")]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_scope")
async def req_scope(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": [request.scope["type"], request.scope["method"], request.scope["path"], request.scope["scheme"], request.scope["http_version"], request.scope["root_path"], request.scope["query_string"]]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_state")
async def req_state(request: Request):
    host = request.headers["host"]
    try:
        request.state.x = 1
        return {"ok": [request.state.x, hasattr(request.state, "y"), getattr(request.state, "y", "d"), type(request.state).__name__]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_app_router")
async def req_app_router(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": request.app.router is not None}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_path_params")
async def req_path_params(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": [request.path_params, type(request.path_params).__name__]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_body")
async def req_body(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": [(await request.body()).decode(), (await request.body()).decode(), await request.is_disconnected()]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_json")
async def req_json(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": await request.json()}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_receive")
async def req_receive(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": callable(request.receive)}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_disconnected")
async def req_disconnected(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": await request.is_disconnected()}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_eq")
async def req_eq(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": [request == request, hash(request) is not None]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_body_cache")
async def req_body_cache(request: Request):
    host = request.headers["host"]
    try:
        return {"ok": hasattr(request, "_body")}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_app")
async def req_app(request: Request):
    try:
        return {"ok": [type(request.app).__name__, request.app is request.scope["app"], len(request.app.routes) > 5, request.app.state is not None]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_scope_known")
async def req_scope_known(request: Request):
    try:
        return {"ok": [request.scope.get("raw_path"), request.scope.get("app") is not None, request.scope.get("route") is not None,
                       request.scope.get("path_params"), request.scope.get("client") is not None, type(request.scope["headers"][0][0]).__name__, request.scope.get("session")]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_scope_mw_keys")
async def req_scope_mw_keys(request: Request):
    """keys only a middleware sets (SessionMiddleware, AuthenticationMiddleware): absent without one"""
    try:
        return {"ok": [request.scope.get("session"), request.scope.get("user", "anon"), "auth" in request.scope,
                       getattr(request.state, "session", None) or request.scope.get("session")]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/req_scope_mw_key_missing")
async def req_scope_mw_key_missing(request: Request):
    return {"v": request.scope["session"]}


@router.get("/req_query_str")
async def req_query_str(request: Request):
    try:
        q = request.query_params
        return {"ok": [dict(q), list(q), len(q), bool(q), q.get("a"), q["a"], "a" in q, list(q.items())]}
    except Exception as e:  # noqa: BLE001
        return fail(e)


@router.get("/nan/{case}")
def nan_dumps(case: str):
    """json.dumps writes NaN/Infinity/-Infinity by default (allow_nan=True), raises ValueError with allow_nan=False"""
    values = [float("inf"), float("-inf"), float("nan"), 1.5]
    try:
        if case == "default":
            return {"ok": json.dumps(values)}
        if case == "indent":
            return {"ok": json.dumps({"a": float("nan"), "b": [float("-inf")]}, indent=2)}
        if case == "compact":
            return {"ok": json.dumps({"x": float("inf")}, separators=(",", ":"), allow_nan=True)}
        if case == "strict":
            return {"ok": json.dumps([1.0, float("-inf")], allow_nan=False)}
        return {"ok": json.dumps([1.0, 2.0], allow_nan=False)}
    except ValueError as e:
        return fail(e)
