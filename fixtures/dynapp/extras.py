"""Small library ports: dateutil.relativedelta, xmltodict; __getattr__ proxies; computed fields."""
import datetime
from xml.parsers.expat import ExpatError

import xmltodict

from dateutil.relativedelta import relativedelta
from fastapi import APIRouter, Form
from pydantic import BaseModel, PrivateAttr, computed_field

from .enums import Group, Relation

router = APIRouter(prefix="/extras")


def _try(f):
    try:
        return f()
    except Exception as e:  # noqa: BLE001
        return [type(e).__name__, str(e)]


@router.get("/reldelta")
async def reldelta(day: datetime.date, months: int = 0, weeks: int = 0, hours: int = 0):
    r = relativedelta(months=months, weeks=weeks, hours=hours)
    stamp = datetime.datetime(day.year, day.month, day.day, 12, 30, tzinfo=datetime.UTC)
    return {
        "repr": repr(r), "str": str(r), "bool": bool(r),
        "minus": str(day - r), "plus": str(day + r), "radd": str(r + day),
        "stamp": [stamp.isoformat(), (stamp - r).isoformat(), (stamp + relativedelta(years=1, seconds=-1)).isoformat()],
        "attrs": [r.years, r.months, r.days, r.weeks, r.hours, r.minutes],
        "neg": repr(-r), "sum": repr(r + relativedelta(days=2, months=-3)), "diff": repr(r - relativedelta(hours=30)),
        "mul": [repr(r * 2), repr(3 * r)], "eq": [r == relativedelta(months=months, days=7 * weeks, hours=hours), r != r, r == 1],
        "norm": [repr(relativedelta(hours=25, minutes=-70)), repr(relativedelta(months=14)), repr(relativedelta(seconds=1000000)),
                 repr(relativedelta(microseconds=-1000001)), repr(relativedelta(days=1000000)), repr(relativedelta()),
                 repr(relativedelta(months=2.0))],
        # CPython 3.14 reworded the year message: the type only
        "errors": [_try(lambda: datetime.date(1, 1, 1) - relativedelta(years=1))[0], _try(lambda: relativedelta(months=1.5)),
                   _try(lambda: r - day)],
    }


class Inner:
    def __init__(self):
        self.calls = []
        self.label = "inner"

    def record(self, x):
        self.calls.append(x)
        return len(self.calls)


class Proxy:
    """Forwards unknown attributes and every assignment to the wrapped object."""

    __slots__ = ("_target",)

    def __init__(self, target):
        object.__setattr__(self, "_target", target)

    def record(self, x):
        return self._target.record(x * 10)

    def __getattr__(self, item):
        return getattr(self._target, item)

    def __setattr__(self, key, value):
        setattr(self._target, key, value)


@router.get("/proxy")
async def proxy():
    inner = Inner()
    p = Proxy(inner)
    out = [p.record(1), p.label, p.calls]
    p.label = "changed"
    out += [inner.label, p.label, getattr(p, "nope", "dflt"), hasattr(p, "calls"), p._target is inner]
    out.append(_try(lambda: p.missing))
    return out


class Run(BaseModel):
    a: int
    b: int | None = None
    _seen: list = PrivateAttr(default_factory=list)
    _count: int = 3
    _later: str

    @computed_field
    @property
    def total(self) -> int | None:
        return None if self.b is None else self.a + self.b

    @computed_field
    @property
    def label(self) -> str:
        return f"{self.a}/{len(self._seen)}/{self._count}"


class Child(Run):
    c: str = "x"


@router.post("/computed")
async def computed(run: Run) -> Run:
    run._seen.append(1)
    return run


@router.post("/computed/raw", response_model=Child)
async def computed_raw(body: dict):
    return body


@router.get("/computed/dump")
async def computed_dump():
    m = Run(a=1)
    m._seen.append("x")
    m._count = 5
    m._free = 1
    other = Run.model_validate({"a": 2, "b": 3, "total": 9, "_seen": [1]})
    return {
        "dump": m.model_dump(), "none": m.model_dump(exclude_none=True), "unset": m.model_dump(exclude_unset=True),
        "inc": m.model_dump(include={"a", "label"}), "exc": m.model_dump(exclude={"total"}), "json": m.model_dump_json(),
        "repr": repr(m), "priv": [m._seen, m._count, m._free, Run(a=1)._seen, m.model_copy()._seen],
        "other": other.model_dump(), "dict": dict(m), "later": _try(lambda: m._later),
        "set": _try(lambda: setattr(m, "total", 1)),
    }


NS = {"http://example.com/a": None, "http://example.com/p": "pp"}


@router.post("/xml")
async def xml(body: dict):
    out = []
    for doc in body["docs"]:
        try:
            out.append([xmltodict.parse(doc), xmltodict.parse(doc.encode(), process_namespaces=True),
                        xmltodict.parse(doc, process_namespaces=True, namespaces=NS, dict_constructor=dict)])
        except ExpatError as e:
            out.append(["ExpatError", str(e)])
    return out


@router.post("/xml/form")
async def xml_form(doc: str = Form()):
    return xmltodict.parse(doc)


class GroupIn(BaseModel):
    group: Group
    rel: Relation = Relation.CHILD


@router.post("/enums")
async def enums(body: GroupIn):
    return {
        "in": body, "values": [m.value for m in Group], "rel": [Relation.MASTER.value, Relation.MASTER == "MASTER", Relation("CHILD").name],
        "lookup": [Group(["lat", "lon"]).name, Group((1, "a")).name, _try(lambda: Group(("lat", "lon")))],
        "in_text": "city" in Group.TEXT.value, "repr": [repr(Group.PAIR), str(Relation.MASTER)],
    }


# ---- break/continue out of try/finally and with: the finally (or __exit__) runs first, then the jump
class Track:
    def __init__(self, log, name):
        self.log, self.name = log, name

    def __enter__(self):
        self.log.append(f"enter {self.name}")
        return self

    def __exit__(self, *exc):
        self.log.append(f"exit {self.name}")
        return False


@router.get("/jumps")
async def jumps(stop: int = 2):
    log = []
    for i in range(5):
        try:
            if i == stop:
                break
            if i % 2:
                continue
            log.append(f"body {i}")
        finally:
            log.append(f"finally {i}")
    else:
        log.append("for-else")
    n = 0
    while True:
        n += 1
        try:
            try:
                with Track(log, f"w{n}"):
                    if n < 3:
                        continue
                    break
            finally:
                log.append(f"inner {n}")
        finally:
            log.append(f"outer {n}")
    for i in range(3):
        try:
            raise ValueError(i)
        except ValueError:
            if i == 1:
                break
            continue
        else:
            log.append("never")
        finally:
            log.append(f"handled {i}")
    found = None
    for path in ("/nonexistent/py2axum", __file__):
        try:
            with open(path, encoding="utf-8") as f:
                found = f.read(3) != ""
                if found:
                    break
        except OSError:
            log.append("oserror")
            continue
    out = {"log": log, "found": found}
    try:
        for i in range(2):
            try:
                break
            finally:
                raise KeyError("from finally")
    except KeyError as e:
        out["finally_raise"] = repr(e)
    for i in range(3):
        try:
            if i == 1:
                return {**out, "returned": i}
        finally:
            log.append(f"ret {i}")
    return out
