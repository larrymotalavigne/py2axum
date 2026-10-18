"""python-dateutil 2.9 dateutil.parser (dynrt/duparse.rs): parse() over hand-picked and generated strings with each
option (default, dayfirst, yearfirst, fuzzy, fuzzy_with_tokens, ignoretz, tzinfos as a dict or a function), the
ParserError and the other exceptions, isoparse() / isoparser and its methods."""
from datetime import datetime, timedelta, timezone

from dateutil import parser, tz
from dateutil.parser import ParserError, isoparse, isoparser
from fastapi import APIRouter

router = APIRouter(prefix="/duparsex")
DEFAULT = datetime(2003, 9, 25, 10, 49, 41, 5000)
TZINFOS = {"BRST": -10800, "CST": tz.gettz("America/Chicago"), "EST": "EST5EDT", "XYZ": None, "PDT": tz.tzoffset("PDT", -25200),
           "CET": tz.gettz("Europe/Paris"), "UTC": timezone.utc}


def _tzinfos_fn(name, offset):
    if name is None:
        return None
    if name.startswith("X"):
        return 3600 if offset is None else offset
    return tz.tzoffset(name, -7200)


def _err(e):
    return {"err": type(e).__name__, "msg": str(e), "repr": repr(e), "args": [repr(a) for a in e.args],
            "is": [isinstance(e, ParserError), isinstance(e, ValueError), isinstance(e, OverflowError), isinstance(e, TypeError)]}


def _show(v):
    if isinstance(v, tuple):
        return [_show(v[0]), list(v[1])]
    if isinstance(v, datetime):
        return [repr(v), v.isoformat(), v.tzname(), repr(v.utcoffset())]
    return repr(v)


@router.post("/parse")
def parse(body: dict):
    out = []
    for case in body["cases"]:
        kw = dict(case.get("kw", {}))
        if kw.pop("nodefault", False):
            pass
        else:
            kw["default"] = DEFAULT
        if kw.pop("tzdict", False):
            kw["tzinfos"] = TZINFOS
        if kw.pop("tzfn", False):
            kw["tzinfos"] = _tzinfos_fn
        if kw.pop("aware_default", False):
            kw["default"] = DEFAULT.replace(tzinfo=tz.gettz("Asia/Tokyo"))
        s = case["s"]
        if case.get("bytes"):
            s = s.encode()
        try:
            out.append(_show(parser.parse(s, **kw)))
        except Exception as e:  # noqa: BLE001
            out.append(_err(e))
    return out


@router.post("/iso")
def iso(body: dict):
    out = []
    p = isoparser(sep=body.get("sep")) if body.get("sep") is not None else isoparser()
    for s in body["strs"]:
        row = []
        # parse_isotime of a time with an offset: an aware time, not supported (docs/supported.md)
        for f in (isoparse, p.isoparse, p.parse_isodate, p.parse_isotime if not any(c in s for c in "+-Zz") else p.parse_isodate,
                  p.parse_tzstr):
            try:
                row.append(_show(f(s)))
            except Exception as e:  # noqa: BLE001
                row.append(_err(e))
        try:
            row.append(_show(p.parse_tzstr(s, zero_as_utc=False)))
        except Exception as e:  # noqa: BLE001
            row.append(_err(e))
        try:
            row.append(_show(isoparse(s.encode())))
        except Exception as e:  # noqa: BLE001
            row.append(_err(e))
        out.append(row)
    return out


@router.get("/errors/{what}")
def errors(what: str):
    try:
        if what == "int":
            return _show(parser.parse(20260101))
        if what == "none":
            return _show(parser.parse(None))
        if what == "badkw":
            return _show(parser.parse("2026", fuzy=True))
        if what == "badkw_far":
            return _show(parser.parse("2026", zzzzzz=True))
        if what == "tzinfos_bad":
            return _show(parser.parse("10:00 ABC", tzinfos={"ABC": 1.5}))
        if what == "iso_sep":
            return repr(isoparser(sep="ab"))
        if what == "iso_sep_digit":
            return repr(isoparser(sep="1"))
        if what == "iso_int":
            return _show(isoparse(2026))
        if what == "nodefault":
            d = parser.parse("10:20")
            return [d.hour, d.minute, d.date() == datetime.now().date()]
        if what == "weekday_today":
            d = parser.parse("Friday")
            return [d.weekday(), d.hour, (d.date() - datetime.now().date()).days < 7]
        if what == "kwargs":
            return _show(parser.parse(timestr="2026-10-16 10:00", dayfirst=True, default=DEFAULT))
        if what == "err_attrs":
            try:
                parser.parse("not a date")
            except ParserError as e:
                return [e.args, str(e), repr(e), isinstance(e.__cause__, ValueError), e.__cause__ is None]
        if what == "err_cause":
            try:
                parser.parse("2026-02-30")
            except ParserError as e:
                return [e.args, str(e), type(e.__cause__).__name__, str(e.__cause__)]
    except Exception as e:  # noqa: BLE001
        return {"error": _err(e)}
    return None
