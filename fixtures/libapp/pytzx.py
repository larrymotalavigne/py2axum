"""pytz 2026.5 (dynrt/pytzrt.rs): timezone() and its lookups, every zone's localize() over transitions, ambiguous and
non-existent times with each is_dst, normalize(), fromutc()/astimezone(), utcoffset/dst/tzname with and without a
datetime, the LMT of an unlocalized zone, FixedOffset, utc, the zone lists, exceptions, comparisons and arithmetic."""
from datetime import datetime, timedelta, timezone
from zoneinfo import ZoneInfo

import pytz
from croniter import croniter
from dateutil import tz as dtz
from fastapi import APIRouter
from pydantic import BaseModel
from pytz.exceptions import AmbiguousTimeError, InvalidTimeError, NonExistentTimeError, UnknownTimeZoneError

router = APIRouter(prefix="/pytzx")


class Event(BaseModel):
    at: datetime


def _err(e):
    return {"err": type(e).__name__, "msg": str(e), "repr": repr(e),
            "is": [isinstance(e, UnknownTimeZoneError), isinstance(e, KeyError), isinstance(e, LookupError),
                   isinstance(e, pytz.exceptions.Error), isinstance(e, InvalidTimeError),
                   isinstance(e, AmbiguousTimeError), isinstance(e, NonExistentTimeError), isinstance(e, ValueError)]}


def _show(d):
    if not isinstance(d, datetime):
        return [repr(d), type(d).__name__]
    return [repr(d), d.isoformat(), str(d), d.tzname(), repr(d.utcoffset()), repr(d.dst()), d.fold,
            repr(d.tzinfo), str(d.tzinfo), type(d.tzinfo).__name__, d.timestamp() if d.tzinfo is not None else None,
            d.strftime("%Y-%m-%d %H:%M:%S %Z %z")]


def _tz(name):
    if name is None:
        return None
    if name == "utc":
        return pytz.utc
    if name.startswith("fixed:"):
        return pytz.FixedOffset(int(name[6:]))
    return pytz.timezone(name)


@router.post("/zone")
def zone(body: dict):
    """timezone(name): str, bytes, other types, case, munged names; the zone's repr/str/type and attributes."""
    name = body.get("name")
    if body.get("bytes"):
        name = name.encode("latin-1")
    try:
        tz = pytz.timezone(name)
    except Exception as e:  # noqa: BLE001
        return {"error": _err(e)}
    return {"repr": repr(tz), "str": str(tz), "type": type(tz).__name__, "zone": tz.zone,
            "utc": tz is pytz.utc, "same": tz is pytz.timezone(name), "tzname": tz.tzname(None),
            "utcoffset": repr(tz.utcoffset(None)), "dst": repr(tz.dst(None)),
            "kinds": [isinstance(tz, pytz.BaseTzInfo), isinstance(tz, pytz.tzinfo.DstTzInfo),
                      isinstance(tz, pytz.tzinfo.StaticTzInfo), isinstance(tz, timezone)]}


@router.post("/localize")
def localize(body: dict):
    """For one zone: localize() of each wall time with is_dst False/True/None, normalize() after an hour added,
    the instant back in UTC and in the zone, the unlocalized zone (LMT) attached by replace()."""
    tz = _tz(body["zone"])
    out = []
    for s in body["walls"]:
        w = datetime.fromisoformat(s)
        row = []
        for flag in body.get("flags", [False, True, None]):
            try:
                d = tz.localize(w, is_dst=flag)
                later = d + timedelta(hours=body.get("add", 1))
                row.append([_show(d), _show(later), _show(tz.normalize(later)), _show(d.astimezone(pytz.utc)),
                            _show(d.astimezone(pytz.utc).astimezone(tz)), d == later - timedelta(hours=body.get("add", 1)),
                            repr(later - d), d.tzinfo is tz, d.tzinfo is tz.localize(w, is_dst=flag).tzinfo])
            except Exception as e:  # noqa: BLE001
                row.append(_err(e))
        row.append(_show(w.replace(tzinfo=tz)))
        out.append(row)
    return out


@router.post("/fromutc")
def fromutc(body: dict):
    """astimezone()/fromutc()/datetime.fromtimestamp()/datetime(tzinfo=utc) of UTC instants into a zone."""
    tz = _tz(body["zone"])
    out = []
    for s in body["instants"]:
        u = datetime.fromisoformat(s)
        try:
            a = u.replace(tzinfo=pytz.utc).astimezone(tz)
            b = tz.fromutc(u)
            c = tz.fromutc(u.replace(tzinfo=tz))
            ts = u.replace(tzinfo=timezone.utc).timestamp()
            e = datetime.fromtimestamp(ts, tz)
            out.append([_show(a), _show(b), _show(c), _show(e), a == b, a.tzinfo is b.tzinfo, a.tzinfo is e.tzinfo])
        except Exception as e:  # noqa: BLE001
            out.append(_err(e))
    return out


@router.post("/methods")
def methods(body: dict):
    """utcoffset/dst/tzname(dt, is_dst) of a zone on naive values, on its own localized values, on foreign aware ones."""
    tz = _tz(body["zone"])
    out = []
    for s in body["walls"]:
        w = datetime.fromisoformat(s)
        for flag in (None, False, True):
            row = []
            for meth in ("utcoffset", "dst", "tzname"):
                try:
                    f = getattr(tz, meth)
                    simple = body["zone"] == "utc" or body["zone"].startswith("fixed:")
                    row.append(repr(f(w)) if simple else repr(f(w, is_dst=flag)))
                except Exception as e:  # noqa: BLE001
                    row.append(_err(e))
            out.append(row)
        try:
            loc = tz.localize(w)
            out.append([repr(loc.tzinfo.utcoffset(loc)), repr(loc.tzinfo.dst(loc)), loc.tzinfo.tzname(loc)])
        except Exception as e:  # noqa: BLE001
            out.append(_err(e))
        try:
            out.append(repr(tz.utcoffset(w.replace(tzinfo=timezone.utc))))
        except Exception as e:  # noqa: BLE001
            out.append(_err(e))
    return out


@router.get("/errors/{what}")
def errors(what: str):
    paris = pytz.timezone("Europe/Paris")
    w = datetime(2026, 3, 29, 2, 30)
    try:
        if what == "aware":
            return _show(paris.localize(w.replace(tzinfo=timezone.utc)))
        if what == "aware_utc":
            return _show(pytz.utc.localize(w.replace(tzinfo=paris)))
        if what == "naive_normalize":
            return _show(paris.normalize(w))
        if what == "naive_normalize_utc":
            return _show(pytz.utc.normalize(w))
        if what == "foreign_normalize":
            return _show(paris.normalize(w.replace(tzinfo=timezone.utc)))
        if what == "zoneinfo_normalize":
            return _show(paris.normalize(w.replace(tzinfo=ZoneInfo("Europe/Paris"))))
        if what == "fromutc_other":
            return _show(paris.fromutc(w.replace(tzinfo=pytz.timezone("Europe/Berlin"))))
        if what == "fromutc_sibling":
            return _show(paris.fromutc(paris.localize(w + timedelta(hours=5)).replace(hour=1)))
        if what == "fromutc_utc_other":
            return _show(pytz.utc.fromutc(w.replace(tzinfo=paris)))
        if what == "fromutc_utc_naive":
            return _show(pytz.utc.fromutc(w))
        if what == "nonexistent":
            return _show(paris.localize(w, is_dst=None))
        if what == "ambiguous":
            return _show(paris.localize(datetime(2026, 10, 25, 2, 30), is_dst=None))
        if what == "unknown":
            return _show(pytz.timezone("Mars/Olympus_Mons"))
        if what == "unknown_plus":
            return _show(pytz.timezone("Etc/GMT_plus_99"))
        if what == "unknown_type":
            return _show(pytz.timezone(3))
        if what == "unknown_none":
            return _show(pytz.timezone(None))
        if what == "nonascii":
            return _show(pytz.timezone("Europe/Zürich"))
        if what == "bytes_nonascii":
            return _show(pytz.timezone("Zürich".encode()))
        if what == "fixed_big":
            return _show(pytz.FixedOffset(1440))
        if what == "fixed_neg_big":
            return _show(pytz.FixedOffset(-1500))
        if what == "fixed_str":
            return _show(pytz.FixedOffset("60"))
        if what == "localize_date":
            return _show(paris.localize(w.date()))
        if what == "localize_kw":
            return _show(paris.localize(dt=w, is_dst=True))
        if what == "localize_bad_kw":
            return _show(paris.localize(w, dst=True))
        if what == "localize_missing":
            return _show(paris.localize())
        if what == "normalize_is_dst":
            return _show(paris.normalize(paris.localize(w, is_dst=True), is_dst=True))
        if what == "utc_utcoffset_is_dst":
            return repr(pytz.utc.utcoffset(w, is_dst=True))
        if what == "min":
            return _show(paris.localize(datetime.min))
        if what == "max":
            return _show(paris.localize(datetime.max))
        if what == "max_utc":
            return _show(datetime.max.replace(tzinfo=pytz.utc).astimezone(paris))
        if what == "min_utc":
            return _show(paris.fromutc(datetime.min))
        if what == "is_dst_two":
            return _show(paris.localize(datetime(2026, 10, 25, 2, 30), is_dst=2))
        if what == "is_dst_one":
            return _show(paris.localize(datetime(2026, 10, 25, 2, 30), is_dst=1))
        if what == "is_dst_str":
            return _show(paris.localize(datetime(2026, 3, 29, 2, 30), is_dst="x"))
        if what == "attr":
            return [paris.zone, repr(paris._utcoffset), paris._tzname, repr(paris._dst), pytz.utc.zone, repr(pytz.utc._utcoffset),
                    pytz.FixedOffset(90).zone, repr(pytz.FixedOffset(90)._utcoffset), pytz.FixedOffset(90)._minutes]
    except Exception as e:  # noqa: BLE001
        return {"error": _err(e)}
    return None


@router.get("/fixed")
def fixed(minutes: int):
    try:
        tz = pytz.FixedOffset(minutes)
    except Exception as e:  # noqa: BLE001
        return {"error": _err(e)}
    d = tz.localize(datetime(2026, 6, 1, 12))
    return {"repr": repr(tz), "str": str(tz), "type": type(tz).__name__, "same": tz is pytz.FixedOffset(minutes),
            "utc": tz is pytz.utc, "d": _show(d), "conv": _show(d.astimezone(pytz.utc)), "back": _show(pytz.utc.localize(datetime(2026, 6, 1)).astimezone(tz)),
            "norm": _show(tz.normalize(d.astimezone(pytz.timezone("Asia/Tokyo")))), "off": [repr(tz.utcoffset(None)), repr(tz.dst(None)), tz.tzname(None)]}


@router.get("/lists")
def lists():
    return {"all": len(pytz.all_timezones), "common": len(pytz.common_timezones), "first": pytz.all_timezones[:5],
            "last": pytz.common_timezones[-3:], "in": ["Europe/Paris" in pytz.all_timezones_set, "US/Eastern" in pytz.common_timezones_set,
                                                    "US/Eastern" in pytz.all_timezones, "Nope" in pytz.all_timezones_set],
            "types": [isinstance(pytz.all_timezones, list), isinstance(pytz.all_timezones_set, set)], "version": pytz.VERSION,
            "olson": pytz.OLSON_VERSION, "names": sorted(pytz.all_timezones)[100:110], "same": pytz.all_timezones is pytz.all_timezones}


@router.get("/compare")
def compare():
    """Values in different pytz zones and in zoneinfo: ==, <, -, sorting, dict keys, the same instant."""
    paris, ny, tokyo = pytz.timezone("Europe/Paris"), pytz.timezone("America/New_York"), pytz.timezone("Asia/Tokyo")
    a = paris.localize(datetime(2026, 10, 25, 2, 30), is_dst=True)
    b = paris.localize(datetime(2026, 10, 25, 2, 30), is_dst=False)
    c = a.astimezone(ny)
    d = datetime(2026, 10, 25, 2, 30, tzinfo=ZoneInfo("Europe/Paris"))
    e = d.replace(fold=1)
    vals = [a, b, c, d, e, a.astimezone(tokyo), b.astimezone(pytz.utc)]
    keyed = {a: "a", c: "c", b: "b"}
    return {"eq": [[x == y for y in vals] for x in vals], "lt": [[x < y for y in vals] for x in vals],
            "sub": [repr(x - y) for x in vals for y in vals], "sorted": [repr(x) for x in sorted(vals)],
            "keys": sorted(keyed.values()), "len": len(keyed), "hash": [hash(a) == hash(c), hash(pytz.utc) == hash(pytz.utc)],
            "tz": [a.tzinfo == b.tzinfo, a.tzinfo == paris, pytz.utc == timezone.utc, pytz.utc == pytz.timezone("UTC"),
                   pytz.FixedOffset(60) == pytz.FixedOffset(60), {pytz.utc: 1}[pytz.utc], len({paris: 1, ny: 2, paris: 3})]}


@router.get("/model")
def model(zone: str = "Europe/Paris"):
    """A pytz-aware value through Pydantic (model_dump, JSON) and FastAPI's encoder."""
    tz = pytz.timezone(zone)
    ev = Event(at=tz.localize(datetime(2026, 7, 14, 21, 30, 5, 123456)))
    return {"json": ev.model_dump_json(), "dump": ev.model_dump(mode="json"), "raw": ev.at, "py": repr(ev.model_dump()["at"]),
            "lmt": Event(at=datetime(2026, 7, 14, tzinfo=tz)).model_dump_json()}


@router.post("/cron")
def cron(body: dict):
    """croniter over a pytz zone (its `localize` branch of _add_tzinfo) or a dateutil one, across DST changes."""
    zone = pytz.timezone(body["zone"]) if body["lib"] == "pytz" else dtz.gettz(body["zone"])
    w = datetime.fromisoformat(body["start"])
    start = zone.localize(w) if body["lib"] == "pytz" else w.replace(tzinfo=zone)
    out = []
    try:
        c = croniter(body["expr"], start)
        for _ in range(body.get("n", 6)):
            d = c.get_prev(datetime) if body.get("prev") else c.get_next(datetime)
            out.append([repr(d), d.isoformat()])
    except Exception as e:  # noqa: BLE001
        out.append(_err(e))
    return out


@router.post("/strftime")
def strftime(body: dict):
    """strftime over a pytz zone, a fixed offset or naive values, with computed formats: every directive the runtime
    renders (glibc pads a year below 1000 nowhere before 3.14, and never in %c; macOS always), on date and time too."""
    zone = _tz(body.get("zone"))
    out = []
    for w in body["walls"]:
        d = datetime.fromisoformat(w)
        if zone is not None:
            d = zone.localize(d) if hasattr(zone, "localize") else d.replace(tzinfo=zone)
        row = []
        for f in body["formats"]:
            row.append([d.strftime(f), d.date().strftime(f), d.time().strftime(f), format(d, f)])
        row.append([f"{d:%Y|%G|%C|%F|%c}", f"{d.date():%c %x %X}", f"{d:%:z|%z|%Z}", d.strftime("%P %p %e %k %l %u %w %U %W %V %g")])
        out.append(row)
    return out
