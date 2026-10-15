"""PyYAML: `yaml.safe_load` (str, bytes, files) with the values, types and errors it gives (messages, marks,
snippets), `yaml.safe_dump` / `yaml.dump` with each of their options, to a string or a stream."""
import base64
import datetime as dt
import io
import os
import uuid

import yaml
from fastapi import APIRouter
from pydantic import BaseModel
from yaml.composer import ComposerError
from yaml.constructor import ConstructorError
from yaml.parser import ParserError
from yaml.reader import ReaderError
from yaml.representer import RepresenterError
from yaml.scanner import ScannerError

router = APIRouter(prefix="/yamlx")


class Cplx(BaseModel):
    """a complex value (yaml.dump writes !!python/complex), parsed by pydantic from its text"""
    z: complex


def describe(v, seen):
    """A JSON form of a loaded value that keeps its Python types (and cycles)."""
    for s in seen:
        if s is v:
            return {"cycle": type(v).__name__}
    t = type(v).__name__
    if v is None or isinstance(v, (bool, str)):
        return v
    if isinstance(v, int):
        return {"int": str(v)}
    if isinstance(v, float):
        return {"float": repr(v)}
    if isinstance(v, bytes):
        return {"bytes": repr(v)}
    if isinstance(v, (dt.date, dt.datetime)):
        return {t: repr(v), "iso": v.isoformat()}
    if isinstance(v, tuple):
        return {"tuple": [describe(x, seen) for x in v]}
    if isinstance(v, list):
        return {"list": [describe(x, seen + [v]) for x in v]}
    if isinstance(v, set):
        return {"set": sorted(repr(x) for x in v)}
    if isinstance(v, dict):
        return {"dict": [[describe(k, seen), describe(x, seen + [v])] for k, x in v.items()]}
    return {"other": t}


def mark(m):
    if m is None:
        return None
    return {"name": m.name, "index": m.index, "line": m.line, "column": m.column, "pointer": m.pointer,
            "str": str(m), "snippet": m.get_snippet(), "short": m.get_snippet(2, 20)}


def failure(e):
    out = {"exc": type(e).__name__, "msg": str(e), "yaml": isinstance(e, yaml.YAMLError)}
    if isinstance(e, yaml.MarkedYAMLError):
        out.update(context=e.context, problem=e.problem, note=e.note, context_mark=mark(e.context_mark),
                   problem_mark=mark(e.problem_mark))
    if isinstance(e, ReaderError):
        out.update(name=e.name, position=e.position, character=e.character, encoding=e.encoding, reason=e.reason)
    out["kinds"] = [isinstance(e, c) for c in (ScannerError, ParserError, ComposerError, ConstructorError, ReaderError)]
    return out


def loaded(f):
    try:
        return {"ok": describe(f(), [])}
    except yaml.YAMLError as e:
        return failure(e)
    except (ValueError, TypeError, KeyError, IndexError, AttributeError, OverflowError, RecursionError) as e:
        return {"exc": type(e).__name__, "msg": str(e)}


@router.post("/load")
async def load(body: dict):
    return loaded(lambda: yaml.safe_load(body["text"]))


@router.post("/load/safeloader")
async def load_safeloader(body: dict):
    return loaded(lambda: yaml.load(body["text"], Loader=yaml.SafeLoader))


@router.post("/load/bytes")
async def load_bytes(body: dict):
    data = base64.b64decode(body["b64"])
    return loaded(lambda: yaml.safe_load(data))


@router.post("/load/stream")
def load_stream(body: dict):
    """The same document through a file object: open() in text or binary mode, io.StringIO, io.BytesIO."""
    data = base64.b64decode(body["b64"])
    how = body["how"]
    if how == "stringio":
        return loaded(lambda: yaml.safe_load(io.StringIO(data.decode())))
    if how == "bytesio":
        return loaded(lambda: yaml.safe_load(io.BytesIO(data)))
    path = f"/tmp/yamlx-{uuid.uuid4().hex}.yaml"
    with open(path, "wb") as f:
        f.write(data)
    try:
        with open(path, "rb" if how == "binary" else "r") as f:
            out = loaded(lambda: yaml.safe_load(f))
    finally:
        os.remove(path)
    if isinstance(out.get("ok"), dict) or "msg" in out:
        out = {k: (x.replace(path, "PATH") if isinstance(x, str) else x) for k, x in out.items()}
        for m in ("context_mark", "problem_mark"):
            if out.get(m):
                out[m] = {k: (x.replace(path, "PATH") if isinstance(x, str) else x) for k, x in out[m].items()}
    return out


def value(j):
    """A Python value from its JSON description (the types JSON lacks)."""
    if isinstance(j, list):
        return [value(x) for x in j]
    if not isinstance(j, dict):
        return j
    if "$tuple" in j:
        return tuple(value(x) for x in j["$tuple"])
    if "$set" in j:
        return {value(x) for x in j["$set"]}
    if "$bytes" in j:
        return base64.b64decode(j["$bytes"])
    if "$date" in j:
        return dt.date.fromisoformat(j["$date"])
    if "$dt" in j:
        return dt.datetime.fromisoformat(j["$dt"])
    if "$float" in j:
        return float(j["$float"])
    if "$int" in j:
        return int(j["$int"])
    if "$complex" in j:
        return Cplx(z=j["$complex"]).z
    if "$map" in j:
        return {value(k): value(x) for k, x in j["$map"]}
    return {k: value(x) for k, x in j.items()}


def dumped(f):
    try:
        out = f()
    except yaml.YAMLError as e:
        return {"exc": type(e).__name__, "msg": str(e), "args": len(e.args)}
    except (ValueError, TypeError, UnicodeEncodeError, RecursionError) as e:
        return {"exc": type(e).__name__, "msg": str(e)}
    if isinstance(out, bytes):
        return {"bytes": base64.b64encode(out).decode()}
    return {"text": out}


def kwargs(body):
    kw = {k: value(x) for k, x in (body.get("kw") or {}).items()}
    if "version" in kw and kw["version"] is not None:
        kw["version"] = tuple(kw["version"])
    return kw


def sdump(data, stream, kw, full=False):
    """yaml.safe_dump / yaml.dump with every option spelled out (the defaults are PyYAML's)."""
    g = kw.get
    if full:
        return yaml.dump(data, stream, default_style=g("default_style"), default_flow_style=g("default_flow_style", False),
                         canonical=g("canonical"), indent=g("indent"), width=g("width"), allow_unicode=g("allow_unicode"),
                         line_break=g("line_break"), encoding=g("encoding"), explicit_start=g("explicit_start"),
                         explicit_end=g("explicit_end"), version=g("version"), tags=g("tags"), sort_keys=g("sort_keys", True))
    return yaml.safe_dump(data, stream, default_style=g("default_style"), default_flow_style=g("default_flow_style", False),
                          canonical=g("canonical"), indent=g("indent"), width=g("width"), allow_unicode=g("allow_unicode"),
                          line_break=g("line_break"), encoding=g("encoding"), explicit_start=g("explicit_start"),
                          explicit_end=g("explicit_end"), version=g("version"), tags=g("tags"), sort_keys=g("sort_keys", True))


@router.post("/dump")
async def dump(body: dict):
    data, kw = value(body["value"]), kwargs(body)
    return dumped(lambda: sdump(data, None, kw, bool(body.get("full"))))


@router.post("/dump/dumper")
async def dump_dumper(body: dict):
    data = value(body["value"])
    if body.get("safe"):
        return dumped(lambda: yaml.dump(data, Dumper=yaml.SafeDumper, sort_keys=False))
    return dumped(lambda: yaml.dump(data, Dumper=yaml.Dumper, default_flow_style=None))


@router.post("/dump/all")
async def dump_all(body: dict):
    docs = [value(x) for x in body["docs"]]
    if body.get("full"):
        return dumped(lambda: yaml.dump_all(docs, explicit_end=True))
    return dumped(lambda: yaml.safe_dump_all(docs, explicit_start=body.get("start", False)))


@router.post("/dump/stream")
def dump_stream(body: dict):
    """To a stream: io.StringIO, io.BytesIO, a text or binary file (with or without `encoding=`)."""
    data, kw, how = value(body["value"]), kwargs(body), body["how"]
    if how == "stringio":
        s = io.StringIO()
        r = dumped(lambda: sdump(data, s, kw))
        return {"r": r, "got": s.getvalue()}
    if how == "bytesio":
        b = io.BytesIO()
        r = dumped(lambda: sdump(data, b, kw))
        return {"r": r, "got": base64.b64encode(b.getvalue()).decode()}
    path = f"/tmp/yamlx-{uuid.uuid4().hex}.yaml"
    try:
        with open(path, "wb" if how == "binary" else "w") as f:
            r = dumped(lambda: sdump(data, f, kw))
        with open(path, "rb") as f:
            got = base64.b64encode(f.read()).decode()
    finally:
        os.remove(path)
    return {"r": r, "got": got}


@router.get("/shared")
async def shared():
    """The same list/dict object twice: an anchor and an alias; a cycle; the identity of loaded aliases."""
    inner = [1, 2]
    d = {"x": inner}
    loop = [0]
    loop.append(loop)
    back = yaml.safe_load("a: &x [1, 2]\nb: *x\nc: &m {k: v}\nd: *m\n")
    back["a"].append(3)
    cyc = yaml.safe_load("&r [1, *r]")
    return {"dump": yaml.safe_dump({"a": inner, "b": inner, "c": d, "e": d}), "loop": yaml.safe_dump(loop),
            "flow": yaml.safe_dump([d, d], default_flow_style=True), "aliased": back["b"], "same": back["c"] is back["d"],
            "cycle": cyc[1] is cyc, "len": len(cyc)}


@router.get("/deep/{kind}/{n}")
async def deep(kind: str, n: int):
    """Nesting: the composer and the representer recurse (RecursionError past a depth)."""
    if kind == "load":
        return loaded(lambda: len(str(yaml.safe_load("[" * n + "]" * n))))
    if kind == "loadmap":
        return loaded(lambda: len(str(yaml.safe_load("{a: " * n + "1" + "}" * n))))
    v = []
    for _ in range(n):
        v = [v]
    return dumped(lambda: yaml.safe_dump(v))


@router.post("/unrepresentable")
async def unrepresentable(body: dict):
    """SafeRepresenter's RepresenterError for a type it lacks: args are ("cannot represent an object", data)."""
    out = []
    for kind in body["kinds"]:
        data = {"time": dt.time(1, 2, 3), "delta": dt.timedelta(days=1, seconds=5), "frozen": [dt.time(5)],
                "nested": {"k": [1, dt.timedelta(0)]}}[kind]
        try:
            yaml.safe_dump(data)
            out.append("dumped")
        except RepresenterError as e:
            out.append([str(e), e.args[0], repr(e.args[1])])
    return out


@router.post("/roundtrip")
async def roundtrip(body: dict):
    """safe_load(safe_dump(x)) == x for the generated values, and the dump itself."""
    data = value(body["value"])
    try:
        text = yaml.safe_dump(data, allow_unicode=body.get("unicode", False), width=body.get("width", 80))
        return {"text": text, "back": describe(yaml.safe_load(text), [])}
    except (yaml.YAMLError, ValueError, TypeError, RecursionError) as e:
        return {"exc": type(e).__name__, "msg": str(e)}
