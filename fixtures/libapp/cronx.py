"""croniter 6.2 (dynrt/croniter.rs): next/prev sequences (5, 6 and 7 fields, L, W, #, names, aliases, H), time
zones and DST transitions, ret_type, generators, croniter_range, match/match_range, is_valid/expand, errors."""
from datetime import datetime, timedelta, timezone
from zoneinfo import ZoneInfo

import croniter as croniter_mod
from croniter import (CroniterBadCronError, CroniterBadDateError, CroniterBadTypeRangeError, CroniterError,
                      CroniterNotAlphaError, CroniterUnsupportedSyntaxError, croniter, croniter_range)
from fastapi import APIRouter
from pydantic import BaseModel

router = APIRouter(prefix="/cronx")


class Seq(BaseModel):
    expr: str
    start: str | int | float | None = None
    tz: str | None = None
    fold: int = 0
    ret: str | None = None
    n: int = 5
    mode: str = "next"
    hash_id: str | None = None
    hash_bytes: bool = False
    day_or: bool = True
    sab: bool = False
    bug: bool = False
    max_years: int | None = None
    efst: bool = False
    is_prev: bool = False


class Rng(BaseModel):
    expr: str
    start: str | int | float
    stop: str | int | float
    tz: str | None = None
    tz_stop: str | None = None
    ret: str | None = None
    exclude_ends: bool = False
    day_or: bool = True
    sab: bool = False
    n: int = 50
    lazy: bool = False


class Match(BaseModel):
    expr: str
    dates: list[str]
    to: str | None = None
    tz: str | None = None
    day_or: bool = True
    sab: bool = False
    precision: int | float | None = None


class Valid(BaseModel):
    exprs: list[str]
    hash_id: str | None = None
    sab: bool = False
    strict: bool = False
    strict_year: int | list[int] | None = None


def _tz(name):
    if name is None:
        return None
    if name == "utc":
        return timezone.utc
    if name[0] in "+-":
        return timezone(timedelta(minutes=int(name)))
    return ZoneInfo(name)


def _when(value, tz, fold=0):
    if isinstance(value, str):
        d = datetime.fromisoformat(value)
        z = _tz(tz)
        if z is not None:
            d = d.replace(tzinfo=z, fold=fold)
        return d
    return value


RETS = {"float": float, "datetime": datetime, "int": int, "str": str, "none": None}


def _show(v):
    if isinstance(v, datetime):
        return [repr(v), v.isoformat()]
    return [repr(v), type(v).__name__]


def _err(e):
    return {"error": type(e).__name__, "msg": str(e),
            "is": [isinstance(e, CroniterError), isinstance(e, CroniterBadCronError), isinstance(e, CroniterBadDateError),
                   isinstance(e, CroniterNotAlphaError), isinstance(e, CroniterUnsupportedSyntaxError),
                   isinstance(e, CroniterBadTypeRangeError), isinstance(e, ValueError), isinstance(e, TypeError)]}


@router.post("/seq")
def seq(s: Seq):
    """n steps of a croniter: get_next/get_prev (alternated with `alt`), next(), all_next()/all_prev(), with a datetime
    or the default return type."""
    out = []
    hid = s.hash_id.encode() if (s.hash_id is not None and s.hash_bytes) else s.hash_id
    try:
        if s.ret is None:
            c = croniter(s.expr, _when(s.start, s.tz, s.fold), day_or=s.day_or, max_years_between_matches=s.max_years,
                         is_prev=s.is_prev, hash_id=hid, implement_cron_bug=s.bug, second_at_beginning=s.sab,
                         expand_from_start_time=s.efst)
        else:
            c = croniter(s.expr, _when(s.start, s.tz, s.fold), ret_type=RETS[s.ret], day_or=s.day_or,
                         max_years_between_matches=s.max_years, is_prev=s.is_prev, hash_id=hid, implement_cron_bug=s.bug,
                         second_at_beginning=s.sab, expand_from_start_time=s.efst)
    except Exception as e:  # noqa: BLE001
        return {"init": _err(e)}
    gen = None
    if s.mode == "all_next":
        gen = c.all_next()
    elif s.mode == "all_prev":
        gen = c.all_prev(datetime)
    try:
        for i in range(s.n):
            if s.mode == "next":
                v = c.get_next()
            elif s.mode == "prev":
                v = c.get_prev()
            elif s.mode == "nextdt":
                v = c.get_next(datetime)
            elif s.mode == "prevdt":
                v = c.get_prev(ret_type=datetime)
            elif s.mode == "alt":
                v = c.get_next(datetime) if i % 2 == 0 else c.get_prev(datetime)
            elif s.mode == "iter":
                v = next(c)
            elif s.mode == "peek":
                v = c.get_next(datetime, update_current=False)
            else:
                v = next(gen, "done")
            out.append(_show(v))
    except Exception as e:  # noqa: BLE001
        out.append(_err(e))
    try:
        cur = [_show(c.get_current()), _show(c.get_current(datetime))]
    except Exception as e:  # noqa: BLE001
        cur = _err(e)
    return {"out": out, "cur": cur, "expanded": repr(c.expanded),
            "nth": sorted([k, sorted(str(x) for x in v)] for k, v in c.nth_weekday_of_month.items())}


@router.post("/restart")
def restart(s: Seq):
    """get_next/get_prev with start_time= (the croniter's zone is switched by an aware start), set_current."""
    out = []
    try:
        c = croniter(s.expr, _when(s.start, s.tz, s.fold), hash_id=s.hash_id, second_at_beginning=s.sab)
        out.append(_show(c.get_next(datetime)))
        out.append(_show(c.get_next(start_time=_when("2026-03-29T01:30:00", "Europe/Paris"))))
        out.append(_show(c.get_prev(datetime, _when("2026-10-25T02:30:00", "Europe/Paris", 1))))
        out.append(_show(c.get_next(datetime, 1767225600.0)))
        out.append(_show(c.set_current(_when("2027-01-01T00:00:00", None))))
        out.append(_show(c.get_next(datetime)))
        out.append(_show(c.set_current(1800000000, force=False)))
        out.append(_show(c.set_current(1800000000)))
        out.append(_show(c.get_prev(float)))
        out.append(_show(c.cur))
        out.append(_show(c.tzinfo))
    except Exception as e:  # noqa: BLE001
        out.append(_err(e))
    return out


@router.post("/range")
def rng(r: Rng):
    """croniter_range between two datetimes or timestamps, forward or backward."""
    out = []
    try:
        start, stop = _when(r.start, r.tz), _when(r.stop, r.tz_stop or r.tz)
        if r.ret is None:
            g = croniter_range(start, stop, r.expr, exclude_ends=r.exclude_ends, day_or=r.day_or, second_at_beginning=r.sab)
        else:
            g = croniter_range(start, stop, r.expr, ret_type=RETS[r.ret], exclude_ends=r.exclude_ends, day_or=r.day_or,
                               second_at_beginning=r.sab)
        if r.lazy:
            for _ in range(r.n):
                v = next(g, "done")
                out.append(_show(v))
                if v == "done":
                    break
        else:
            for v in g:
                out.append(_show(v))
    except Exception as e:  # noqa: BLE001
        out.append(_err(e))
    return out


@router.post("/match")
def match(m: Match):
    """croniter.match on each date, match_range from each date to `to`."""
    out = []
    for d in m.dates:
        try:
            when = _when(d, m.tz)
            if m.to is None:
                if m.precision is None:
                    out.append(croniter.match(m.expr, when, day_or=m.day_or, second_at_beginning=m.sab))
                else:
                    out.append(croniter.match(m.expr, when, m.day_or, m.sab, m.precision))
            else:
                out.append(croniter.match_range(m.expr, when, _when(m.to, m.tz), day_or=m.day_or,
                                                second_at_beginning=m.sab, precision_in_seconds=m.precision))
        except Exception as e:  # noqa: BLE001
            out.append(_err(e))
    return out


@router.post("/valid")
def valid(v: Valid):
    """is_valid and expand of each expression."""
    out = []
    for e in v.exprs:
        row = {}
        try:
            row["valid"] = croniter.is_valid(e, hash_id=v.hash_id, second_at_beginning=v.sab, strict=v.strict,
                                             strict_year=v.strict_year)
        except Exception as ex:  # noqa: BLE001
            row["valid"] = _err(ex)
        try:
            expanded, nth = croniter.expand(e, hash_id=v.hash_id.encode() if v.hash_id else None,
                                            second_at_beginning=v.sab, strict=v.strict, strict_year=v.strict_year)
            row["expand"] = [repr(expanded), sorted([k, sorted(str(x) for x in s)] for k, s in nth.items())]
        except Exception as ex:  # noqa: BLE001
            row["expand"] = _err(ex)
        out.append(row)
    return out


@router.get("/misuse")
def misuse():
    """Errors of the API itself: bad hash_id, ret_type, start_time with expand_from_start_time, range types."""
    out = []
    cases = [
        lambda: croniter("* * * * *", 0, hash_id=5),
        lambda: croniter("* * * * *", 0, ret_type=int).get_next(),
        lambda: croniter("* * * * *", 0, ret_type=str).get_prev(),
        lambda: croniter("* * * * *", 0).get_next("x"),
        lambda: croniter("* * * * *", 0).get_current(int),
        lambda: croniter("*/5 * * * *", 600, expand_from_start_time=True).get_next(start_time=60),
        lambda: croniter("*/7 * * * *", 600, expand_from_start_time=True).get_next(),
        lambda: croniter("* * * * * */7", 600, expand_from_start_time=True).get_next(),
        lambda: croniter("H * * * *", 0).get_next(),
        lambda: croniter("H H * * *", 0, hash_id="job-1").get_next(),
        lambda: croniter("H H * * *", 0, hash_id=b"job-1").get_next(),
        lambda: croniter.is_valid("H * * * *", hash_id=""),
        lambda: croniter.is_valid("H * * * *", hash_id=7),
        lambda: croniter.expand("H * * * *", hash_id="x"),
        lambda: list(croniter_range(0, 1.5, "* * * * *")),
        lambda: list(croniter_range(datetime(2026, 1, 1), 5, "* * * * *")),
        lambda: list(croniter_range(datetime(2026, 1, 1), datetime(2026, 1, 1, 0, 3, tzinfo=timezone.utc), "* * * * *")),
        lambda: list(croniter_range(True, 120, "* * * * *")),
        lambda: croniter("0 0 1 1 * 0 2030", 1900000000).get_next(),
        lambda: croniter("0 0 1 1 * 0 2030", 1700000000, max_years_between_matches=2).get_next(),
        lambda: [next(g, "end") for g in [croniter("0 0 1 1 * 0 2030", 1700000000, max_years_between_matches=10).all_next()] for _ in range(3)],
        lambda: croniter("0 0 30 2 *", 1700000000).get_next(),
        lambda: croniter("0 0 * * *", 253402214400).get_next(),
        lambda: croniter("0 0 * * *", float("nan")).get_next(),
        lambda: croniter("0 0 * * *", 1e20).get_next(),
        lambda: croniter("0 0 * * *", -62135596800).get_prev(),
        lambda: croniter.match("* * * * *", 1700000000.0),
        lambda: croniter.match_range("* * * * *", datetime(2026, 1, 1), datetime(2026, 1, 1, tzinfo=timezone.utc)),
        lambda: croniter(12345, 0),
        lambda: croniter("* * * * *".encode(), 0),
        lambda: croniter("* *\x1c* * *".encode(), 0),
        lambda: croniter("0 0 * * *", 0, max_years_between_matches="3").get_next(),
        lambda: croniter("0 0 * * *", 0, max_years_between_matches=0.5).get_prev(),
        lambda: croniter_mod.croniter.datetime_to_timestamp(datetime(2026, 5, 1, 12, tzinfo=ZoneInfo("Asia/Kolkata"))),
        lambda: croniter("0 0 * * *", 0).timestamp_to_datetime(86400.5, timezone.utc),
        lambda: croniter("0 0 * * *", True).get_current(),
        lambda: croniter("0 0 * * *", 7).get_current(),
        lambda: [type(croniter("0 0 * * *", 0, ret_type=datetime).get_next()) is datetime, RETS["datetime"](2026, 1, 2).isoformat()],
        lambda: [croniter("0 0 * * *", 0).get_next(datetime, update_current=False), croniter("@daily", 0).get_prev(datetime)],
        lambda: croniter("0 0 * * *", 0).timestamp_to_datetime(-0.5),
        lambda: croniter("0 0 * * *", 0).timestamp_to_datetime(1.0000005, None),
    ]
    for f in cases:
        try:
            v = f()
            out.append(_show(v) if not isinstance(v, list) else [_show(x) for x in v])
        except CroniterBadDateError as e:
            out.append({"bad_date": str(e)})
        except CroniterError as e:
            out.append(_err(e))
        except (TypeError, ValueError, OverflowError, AttributeError) as e:
            out.append(_err(e))
    return out
