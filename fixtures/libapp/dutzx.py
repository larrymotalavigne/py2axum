"""python-dateutil 2.9 dateutil.tz (dynrt/dutz.rs): gettz over zone names, paths and TZ strings (the system's zoneinfo
files, dateutil's own tarball, tzstr), tzutc/tzoffset/tzlocal, utcoffset/dst/tzname/fold over transitions, fromutc
through astimezone, is_ambiguous, datetime_exists/datetime_ambiguous/resolve_imaginary, enfold, ==, the tzstr parser."""
from datetime import datetime, timedelta, timezone
from zoneinfo import ZoneInfo

from dateutil import tz
from dateutil.tz import UTC, datetime_ambiguous, datetime_exists, enfold, gettz, resolve_imaginary, tzlocal, tzoffset, tzstr, tzutc
from fastapi import APIRouter

router = APIRouter(prefix="/dutzx")


def _err(e):
    return {"err": type(e).__name__, "msg": str(e), "is": [isinstance(e, ValueError), isinstance(e, TypeError),
                                                         isinstance(e, KeyError), isinstance(e, OSError)]}


def _show(d):
    if not isinstance(d, datetime):
        return [repr(d), type(d).__name__]
    return [repr(d), d.isoformat(), d.tzname(), repr(d.utcoffset()), repr(d.dst()), d.fold, repr(d.tzinfo),
            type(d.tzinfo).__name__, d.timestamp() if d.tzinfo is not None else None, d.strftime("%Y-%m-%d %H:%M:%S %Z %z")]


def _tzinfo(spec):
    if spec == "utc":
        return tz.tzutc()
    if spec == "UTC":
        return UTC
    if spec == "local":
        return tzlocal()
    if spec.startswith("off:"):
        name, secs = spec[4:].split("/")
        return tzoffset(None if name == "None" else name, int(secs))
    if spec.startswith("str:"):
        return tzstr(spec[4:])
    if spec == "stdlib":
        return timezone(timedelta(hours=2))
    if spec == "zoneinfo":
        return ZoneInfo("Europe/Paris")
    return gettz(spec)


@router.post("/gettz")
def get(body: dict):
    """gettz(name): the object's repr, type, cache identity, and its offsets at a few instants."""
    out = []
    for name in body["names"]:
        try:
            z = gettz(name)
        except Exception as e:  # noqa: BLE001
            out.append(_err(e))
            continue
        if z is None:
            out.append(None)
            continue
        row = [repr(z), str(z), type(z).__name__, z is gettz(name), z == gettz(name),
               isinstance(z, tz.tzfile), isinstance(z, tz.tzstr), isinstance(z, tz.tzutc), isinstance(z, tz.tzlocal)]
        for s in body.get("walls", []):
            w = datetime.fromisoformat(s)
            try:
                row.append([z.utcoffset(w) == w.replace(tzinfo=z).utcoffset(), _show(w.replace(tzinfo=z)),
                            _show(w.replace(tzinfo=z, fold=1)), z.is_ambiguous(w), datetime_exists(w, z)])
            except Exception as e:  # noqa: BLE001
                row.append(_err(e))
        out.append(row)
    return out


@router.post("/walls")
def walls(body: dict):
    """One tzinfo over wall times: both folds, the tzinfo methods themselves, ambiguity and existence."""
    z = _tzinfo(body["tz"])
    out = []
    for s in body["walls"]:
        w = datetime.fromisoformat(s)
        try:
            a = w.replace(tzinfo=z)
            b = enfold(a, fold=1)
            row = [_show(a), _show(b), a == b, repr(b - a), repr(z.utcoffset(w)), repr(z.dst(w)), z.tzname(w),
                   repr(z.utcoffset(b)), z.tzname(b), datetime_ambiguous(a), datetime_exists(a), _show(resolve_imaginary(a)),
                   datetime_ambiguous(w, tz=z), datetime_exists(w, z), _show(a + timedelta(hours=1)), _show(a - timedelta(days=200))]
            if hasattr(z, "is_ambiguous"):
                row.append(z.is_ambiguous(w))
            out.append(row)
        except Exception as e:  # noqa: BLE001
            out.append(_err(e))
    return out


@router.post("/instants")
def instants(body: dict):
    """UTC instants into a tzinfo: astimezone(), fromutc(), datetime.fromtimestamp(), back to UTC."""
    z = _tzinfo(body["tz"])
    out = []
    for s in body["instants"]:
        u = datetime.fromisoformat(s)
        try:
            a = u.replace(tzinfo=UTC).astimezone(z)
            b = z.fromutc(u.replace(tzinfo=z))
            c = datetime.fromtimestamp(u.replace(tzinfo=timezone.utc).timestamp(), z)
            out.append([_show(a), _show(b), _show(c), _show(a.astimezone(timezone.utc)), a == b, repr(a.astimezone(z))])
        except Exception as e:  # noqa: BLE001
            out.append(_err(e))
    return out


def _none(v):
    """utcoffset/dst/tzname(None): tzlocal's tzname(None) raises when the local zone has DST"""
    out = []
    for f in (v.utcoffset, v.dst, v.tzname):
        try:
            out.append(repr(f(None)))
        except Exception as e:  # noqa: BLE001
            out.append(_err(e))
    return out


@router.get("/objects")
def objects():
    """tzutc / tzoffset / tzlocal / tzstr reprs, singletons and caches, ==, hashing, None arguments."""
    vals = [tzutc(), UTC, tzoffset(None, 0), tzoffset("UTC", 0), tzoffset("BRST", -10800), tzoffset("BRST", timedelta(hours=-3)),
            tzoffset(None, 3600.0), tzlocal(), tzlocal(), tzstr("EST5EDT"), tzstr("EST5EDT"), tzstr("UTC+3"), tzstr("UTC+3", posix_offset=True),
            gettz("UTC"), gettz("Europe/Paris"), timezone.utc, ZoneInfo("UTC")]
    out = {"repr": [repr(v) for v in vals], "str": [str(v) for v in vals], "type": [type(v).__name__ for v in vals],
           "eq": [[x == y for y in vals] for x in vals], "ne": [vals[0] != vals[2], vals[7] != vals[8]],
           "is": [tzutc() is UTC, tzoffset(None, 60) is tzoffset(None, 60), tzlocal() is tzlocal(), tzstr("EST5EDT") is tzstr("EST5EDT")],
           "none": [_none(v) for v in vals[:13]]}
    hashes = []
    for v in vals[:4]:
        try:
            hashes.append(hash(v))
        except TypeError as e:
            hashes.append(str(e))
    out["hash"] = hashes
    return out


@router.post("/tzstr")
def tz_strings(body: dict):
    """tzstr(s) of TZ strings, valid and not: repr, offsets in January and July, transitions."""
    out = []
    for s in body["strs"]:
        try:
            z = tzstr(s, posix_offset=body.get("posix", False))
        except Exception as e:  # noqa: BLE001
            out.append(_err(e))
            continue
        row = [repr(z), z.hasdst]
        for w in ("2026-01-15T12:00:00", "2026-07-15T12:00:00", "2026-03-08T02:30:00", "2026-11-01T01:30:00",
                  "2026-04-05T01:59:00", "2026-10-25T00:30:00", "2024-02-29T03:00:00"):
            d = datetime.fromisoformat(w).replace(tzinfo=z)
            row.append(_show(d))
            row.append(_show(d.astimezone(UTC).astimezone(z)))
        out.append(row)
    return out


@router.get("/errors/{what}")
def errors(what: str):
    paris = gettz("Europe/Paris")
    w = datetime(2026, 3, 29, 2, 30)
    try:
        if what == "fromutc_other":
            return _show(paris.fromutc(w.replace(tzinfo=UTC)))
        if what == "fromutc_naive":
            return _show(paris.fromutc(w))
        if what == "fromutc_date":
            return _show(paris.fromutc(w.date()))
        if what == "utc_fromutc_other":
            return _show(UTC.fromutc(w.replace(tzinfo=paris)))
        if what == "offset_fromutc":
            z = tzoffset("X", 3600)
            return _show(z.fromutc(w.replace(tzinfo=z)))
        if what == "exists_naive":
            return datetime_exists(w)
        if what == "ambiguous_naive":
            return datetime_ambiguous(w)
        if what == "gettz_bytes":
            return repr(gettz(b"Europe/Paris"))
        if what == "gettz_abs":
            return repr(gettz("/usr/share/zoneinfo/Europe/Paris"))
        if what == "gettz_abs_missing":
            return repr(gettz("/nonexistent/Europe/Paris"))
        if what == "gettz_abs_bad":
            return repr(gettz("/etc/hosts"))
        if what == "gettz_colon":
            return repr(gettz(":Europe/Paris"))
        if what == "gettz_space":
            return repr(gettz("America/New York"))
        if what == "gettz_none":
            return repr(gettz())
        if what == "tzstr_bad":
            return repr(tzstr("not a tz"))
        if what == "tzoffset_kw":
            return repr(tzoffset(name="X", offset=60))
        if what == "enfold_bad":
            return repr(enfold(w, fold=2))
        if what == "enfold_zero":
            return _show(enfold(w.replace(tzinfo=paris, fold=1), fold=0))
        if what == "unhashable":
            return {gettz("Europe/Paris"): 1}
        if what == "missing":
            return repr(paris.utcoffset())
        if what == "max":
            return _show(datetime.max.replace(tzinfo=UTC).astimezone(paris))
        if what == "min":
            return _show(datetime.min.replace(tzinfo=UTC).astimezone(gettz("America/New_York")))
    except Exception as e:  # noqa: BLE001
        return {"error": _err(e)}
    return None


@router.get("/compare")
def compare():
    """dateutil values beside zoneinfo and pytz-free stdlib ones: same-tzinfo wall arithmetic, PEP 495 equality."""
    paris = gettz("Europe/Paris")
    a = datetime(2026, 10, 25, 2, 30, tzinfo=paris)
    b = enfold(a)
    c = datetime(2026, 10, 25, 2, 30, tzinfo=ZoneInfo("Europe/Paris"), fold=1)
    d = datetime(2026, 3, 30, 12, tzinfo=paris)
    e = datetime(2026, 3, 28, 12, tzinfo=paris)
    vals = [a, b, c, d, e, a.astimezone(UTC), b.astimezone(tzoffset(None, 3600))]
    return {"eq": [[x == y for y in vals] for x in vals], "lt": [[x < y for y in vals] for x in vals],
            "sub": [repr(x - y) for x in vals for y in vals], "sorted": [repr(x) for x in sorted(vals)],
            "zi": [repr(d - e), repr(d.astimezone(UTC) - e.astimezone(UTC)),
                   repr(datetime(2026, 3, 30, 12, tzinfo=ZoneInfo("Europe/Paris")) - datetime(2026, 3, 28, 12, tzinfo=ZoneInfo("Europe/Paris")))]}
