"""prometheus_client: metrics, labels, timers, registries and the text exposition format."""
import asyncio

import httpx
from fastapi import APIRouter, Request, Response
from prometheus_client import (
    CONTENT_TYPE_LATEST,
    GC_COLLECTOR,
    PLATFORM_COLLECTOR,
    PROCESS_COLLECTOR,
    REGISTRY,
    CollectorRegistry,
    Counter,
    Enum,
    Gauge,
    Histogram,
    Info,
    Summary,
    disable_created_metrics,
    enable_created_metrics,
    generate_latest,
    start_http_server,
)
from prometheus_client.openmetrics.exposition import CONTENT_TYPE_LATEST as OPENMETRICS
from prometheus_client.openmetrics.exposition import generate_latest as openmetrics_latest
from prometheus_client.registry import DuplicateTimeseries

router = APIRouter(prefix="/prom")

# the default registry without CPython's own collectors (gc counts, interpreter version, /proc)
for _c in (PROCESS_COLLECTOR, PLATFORM_COLLECTOR, GC_COLLECTOR):
    REGISTRY.unregister(_c)

REG = CollectorRegistry()
JOBS = Counter("jobs_total", "Jobs done", ["kind", "status"], registry=REG)
DEPTH = Gauge("queue_depth", "Depth of the\nqueue \\ now", registry=REG)
LATENCY = Histogram("job_seconds", "Job time", ["kind"], buckets=(0.1, 0.5, 1, "2.5"), registry=REG)
PAYLOAD = Summary("payload", "Payload size", namespace="app", subsystem="io", unit="bytes", registry=REG)
BUILD = Info("build", "Build information", registry=REG)
PHASE = Enum("phase", "Current phase", states=["starting", "running", "stopped"], registry=REG)
ITEMS: list[int] = []
SIZE = Gauge("items", "Items held", ["shelf"], registry=REG)
SIZE.labels("a").set_function(lambda: len(ITEMS))
ODD = Gauge("odd_values", "Float formatting", ["case"], registry=REG)
NAMES = Counter("dotted.name", "Escaped names", ["label.x", "ok"], registry=REG)
DEFAULT = Counter("fixture_default", "In the default registry", ["route"])
TIMED = Histogram("timed_seconds", "Timed blocks", ["how"], registry=REG)
FAILS = Counter("fails", "Exceptions counted", registry=REG)
BUSY = Gauge("busy", "In progress", registry=REG)

# module-level statements run at import: a duplicate registration and its fallback, a condition
try:
    SAME = Gauge("jobs_total", "Same series as JOBS", registry=REG)
except ValueError as dup:
    SAME = REG._names_to_collectors.get("jobs_total")
    DUP_MESSAGE = str(dup)
if REG.get_sample_value("queue_depth") == 0:
    STARTUP = "empty"
    with DEPTH.track_inprogress():
        STARTUP_DEPTH = REG.get_sample_value("queue_depth")
else:
    STARTUP = "busy"

TARGETED = CollectorRegistry(target_info={"env": "test", "zone": "eu\"1"})
Counter("hits", "Hits", registry=TARGETED).inc(3)


@TIMED.labels("deco").time()
def timed_sync(n: int) -> int:
    return n * 2


@TIMED.labels("async").time()
async def timed_async(n: int) -> int:
    await asyncio.sleep(0)
    return n + 1


@FAILS.count_exceptions(KeyError)
def maybe_fail(fail: bool) -> str:
    if fail:
        raise KeyError("missing")
    return "fine"


@BUSY.track_inprogress()
def tracked() -> float:
    return REG.get_sample_value("busy")


def text(registry) -> Response:
    return Response(generate_latest(registry), media_type=CONTENT_TYPE_LATEST)


@router.post("/run")
async def run():
    JOBS.labels("a", "ok").inc()
    JOBS.labels(kind="b", status="ko").inc(2.5)
    JOBS.labels("a", "ok").inc(0)
    JOBS.labels(status=404, kind=True).inc(exemplar={"trace_id": "abc"})
    DEPTH.inc()
    DEPTH.inc(4)
    DEPTH.dec(0.5)
    LATENCY.labels("x").observe(0.3)
    LATENCY.labels("x").observe(0.1)
    LATENCY.labels("y").observe(7)
    LATENCY.labels("y").observe(-1)
    PAYLOAD.observe(512)
    PAYLOAD.observe(1.5)
    BUILD.info({"version": "1.2.3", "host": "a\\b\nc"})
    PHASE.state("running")
    ITEMS.extend([1, 2, 3])
    for case, v in (("big", 12345678.0), ("round", 10000000.0), ("small", 1e-07), ("sum", 0.1 + 0.2), ("huge", 1e22),
                    ("neg", -12345678.5), ("inf", float("inf")), ("ninf", float("-inf")), ("nan", float("nan")), ("int", 7)):
        ODD.labels(case).set(v)
    ODD.labels("str").set("4.25")
    NAMES.labels("v", "q\"uote").inc()
    DEFAULT.labels("/run").inc()
    return {"ok": True}


@router.get("/metrics")
async def metrics():
    return text(REG)


@router.get("/default")
async def default():
    return text(REGISTRY)


@router.get("/targeted")
async def targeted():
    return text(TARGETED)


@router.get("/single")
async def single():
    return Response(generate_latest(JOBS), media_type="text/plain")


@router.get("/nocreated")
async def nocreated():
    disable_created_metrics()
    try:
        return text(REG)
    finally:
        enable_created_metrics()


@router.get("/timers")
async def timers():
    with TIMED.labels("with").time() as t:
        before = t.duration
    with DEPTH.time():
        pass
    out = {"before": before, "after": t.duration is not None and t.duration >= 0,
           "sync": timed_sync(4), "async": await timed_async(4),
           "ok": maybe_fail(False), "busy_inside": tracked(), "busy_after": REG.get_sample_value("busy")}
    try:
        maybe_fail(True)
    except KeyError as e:
        out["raised"] = repr(e)
    with FAILS.count_exceptions() as nothing:
        out["entered"] = nothing
    try:
        with FAILS.count_exceptions((ValueError, TypeError)):
            raise TypeError("t")
    except TypeError:
        pass
    timer = LATENCY.time()
    timer.labels("z")
    with timer:
        pass
    for how in ("with", "deco", "async"):
        out[how] = [REG.get_sample_value("timed_seconds_bucket", {"how": how, "le": "0.005"}),
                    REG.get_sample_value("timed_seconds_count", {"how": how})]
    out["fails"] = REG.get_sample_value("fails_total")
    out["z"] = REG.get_sample_value("job_seconds_count", {"kind": "z"})
    out["depth_set"] = REG.get_sample_value("queue_depth") < 1
    out["missing"] = REG.get_sample_value("nope")
    out["wrapped"] = timed_sync.__wrapped__(1)
    return out


def _err(f) -> list:
    try:
        f()
    except (ValueError, TypeError, KeyError, RuntimeError, AttributeError) as e:
        return [type(e).__name__, str(e)]
    return ["no error"]


@router.get("/errors")
async def errors():
    scratch = CollectorRegistry()
    Counter("taken", "x", registry=scratch)
    lab = Gauge("lab", "x", ["a"], registry=scratch)
    recent = Gauge("recent", "x", registry=scratch, multiprocess_mode="livemostrecent")
    cases = {
        "dup_counter": lambda: Gauge("taken_total", "x", registry=scratch),
        "dup_created": lambda: Summary("taken_created", "x", registry=scratch),
        "dup_info": lambda: Info("taken", "x", registry=scratch),
        "dup_is_value_error": lambda: isinstance(DuplicateTimeseries("m"), ValueError) or 1 / 0,
        "no_doc": lambda: Counter("a"),
        "no_args": lambda: Histogram(),
        "too_many": lambda: Counter("a", "b", (), "", "", "", None, None, 1),
        "bad_kw": lambda: Counter("a", "b", label=["x"]),
        "twice": lambda: Counter("a", "b", name="c"),
        "empty_name": lambda: Counter("", "b"),
        "info_unit": lambda: Info("i", "b", unit="s", registry=None),
        "reserved": lambda: Counter("r", "b", ["__x"], registry=None),
        "reserved_le": lambda: Histogram("r", "b", ["le"], registry=None),
        "reserved_q": lambda: Summary("r", "b", ["quantile"], registry=None),
        "label_not_str": lambda: Counter("r", "b", [1], registry=None),
        "label_chars": lambda: Counter("r", "b", "ab", registry=None).labels("x", "y").inc(),
        "unsorted": lambda: Histogram("h", "b", buckets=[1, 0.5], registry=None),
        "one_bucket": lambda: Histogram("h", "b", buckets=[float("inf")], registry=None),
        "no_bucket": lambda: Histogram("h", "b", buckets=[], registry=None),
        "bad_bucket": lambda: Histogram("h", "b", buckets=["x"], registry=None),
        "mode": lambda: Gauge("g", "b", multiprocess_mode="nope", registry=None),
        "enum_overlap": lambda: Enum("e", "b", ["e"], states=["x"], registry=None),
        "enum_no_states": lambda: Enum("e", "b", registry=None),
        "enum_bad_state": lambda: PHASE.state("flying"),
        "no_labels": lambda: DEPTH.labels("x"),
        "chained": lambda: JOBS.labels("a", "ok").labels("b"),
        "both": lambda: JOBS.labels("a", status="ok"),
        "names": lambda: JOBS.labels(kind="a", state="ok"),
        "count": lambda: JOBS.labels("a"),
        "parent_inc": lambda: JOBS.inc(),
        "parent_observe": lambda: LATENCY.observe(1),
        "parent_track": lambda: lab.track_inprogress(),
        "negative": lambda: DEFAULT.labels("/x").inc(-1),
        "inc_str": lambda: DEFAULT.labels("/x").inc("1"),
        "gauge_str": lambda: DEPTH.inc("1"),
        "dec_str": lambda: DEPTH.dec("1"),
        "set_bad": lambda: DEPTH.set("x"),
        "recent_inc": lambda: recent.inc(),
        "recent_set": lambda: recent.set(3),
        "no_dec": lambda: JOBS.labels("a", "ok").dec(),
        "exemplar": lambda: DEFAULT.labels("/x").inc(1, {"__bad": "x"}),
        "exemplar_long": lambda: DEFAULT.labels("/x").inc(1, {"k": "v" * 200}),
        "remove_count": lambda: JOBS.remove("a"),
        "remove_none": lambda: DEPTH.remove("a"),
        "by_labels_bad": lambda: JOBS.remove_by_labels({"nope": 1}),
        "by_labels_type": lambda: JOBS.remove_by_labels([1]),
        "info_overlap": lambda: Info("i2", "b", ["a"], registry=None).labels("x").info({"a": "1"}),
        "info_none": lambda: BUILD.info({"a": None}),
        "unregister_twice": lambda: scratch.unregister(lab) or scratch.unregister(lab),
        "target_dup": lambda: Gauge("target_info", "x", registry=TARGETED),
        "set_target": lambda: scratch.set_target_info({"a": "b"}) or scratch.get_target_info(),
        "dup_target_info": lambda: Gauge("target_info", "x", registry=scratch),
    }
    out = {k: _err(f) for k, f in cases.items()}
    out["str"] = [str(JOBS), repr(JOBS), str(JOBS.labels("a", "ok")), str(PHASE), str(BUILD), repr(LATENCY)]
    out["names_map"] = sorted(scratch._names_to_collectors)
    return out


@router.post("/mutate")
async def mutate():
    JOBS.remove("b", "ko")
    JOBS.labels("c", "ok").inc()
    JOBS.labels("d", "ok").inc()
    JOBS.remove_by_labels({"status": "ok", "kind": "c"})
    JOBS.remove_by_labels({})
    NAMES.clear()
    DEPTH.set_to_current_time()
    ok = DEPTH._name, DEPTH._documentation, JOBS._labelnames
    DEPTH.set(2)
    return {"ok": ok, "now": REG.get_sample_value("queue_depth"), "isinstance": [isinstance(JOBS, Counter), isinstance(JOBS, Gauge),
                                                                                 isinstance(REG, CollectorRegistry)]}


@router.post("/reset")
async def reset():
    c = Counter("resettable", "x", registry=None)
    c.inc(5)
    c.reset()
    inc = c.inc
    inc(2)
    return {"value": generate_latest(c).decode().split("\n")[2]}


@router.get("/module")
async def module_level():
    return {"same": str(SAME), "message": DUP_MESSAGE, "startup": [STARTUP, STARTUP_DEPTH], "loop": type(_c).__name__}


EXPORTER: dict = {}


@router.get("/exporter")
async def exporter(request: Request):
    """start_http_server on the request's port + 11 (each server its own), scraped with various headers."""
    port = int(request.headers["host"].rsplit(":", 1)[1]) + 11
    if "server" not in EXPORTER:
        EXPORTER["server"] = start_http_server(port, addr="127.0.0.1", registry=REG)
    out = []
    async with httpx.AsyncClient() as client:
        for path, headers in (
            ("/metrics", {}),
            ("/", {"Accept": "text/plain;version=1.0.0"}),
            ("/", {"Accept": "application/openmetrics-text;version=1.0.0"}),
            ("/", {"Accept": "application/openmetrics-text; version=0.0.1, text/plain;version=0.0.4;q=0.5"}),
            ("/", {"Accept": "application/openmetrics-text"}),
            ("/", {"Accept": "application/openmetrics-text;version=1.0.0;escaping=allow-utf-8"}),
            ("/", {"Accept": "text/plain;version=1.0.0;escaping=dots"}),
            ("/?name[]=jobs_total&name[]=queue_depth&name[]=phase", {"Accept-Encoding": "identity"}),
            ("/favicon.ico", {}),
        ):
            r = await client.get(f"http://127.0.0.1:{port}{path}", headers=headers)
            out.append([path, r.status_code, r.headers.get("content-type"), r.headers.get("content-encoding"), r.text])
        r = await client.post(f"http://127.0.0.1:{port}/")
        out.append([r.status_code, r.headers.get("allow"), r.text])
        r = await client.options(f"http://127.0.0.1:{port}/")
        out.append([r.status_code, r.headers.get("allow"), r.text])
    restricted = REG.restricted_registry(["app_io_payload_bytes_sum", "build_info", "nope"])
    return {"texts": out, "returned": len(EXPORTER["server"]), "om": openmetrics_latest(REG).decode(), "om_type": OPENMETRICS,
            "restricted": [generate_latest(restricted).decode(), openmetrics_latest(restricted).decode()],
            "esc": [generate_latest(NAMES, escaping=e).decode() for e in ("allow-utf-8", "underscores", "dots", "values")]}
