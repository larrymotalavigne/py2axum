"""The `logging` module as real applications use it (docs/advanced/logging.md): loggers named after
their module or a dotted name, levels and effective levels, `propagate`, handlers on stdout and stderr,
formatters in the three styles with every LogRecord attribute, `datefmt`, `defaults`, a project
`logging.Filter` adding an attribute (a correlation id), `extra=`, `exc_info=`, `logger.exception`,
chained exceptions, `LoggerAdapter`, `dictConfig`, python-json-logger (renamed
fields with `rename_fields`, the old import path), a formatting error (`--- Logging error ---`). What the servers
print is compared line for line by the log conformance (tests/logcmp.py)."""
import logging
import logging.config
import sys

from fastapi import APIRouter
from pythonjsonlogger import json as jsonlogger
from pythonjsonlogger import jsonlogger as legacy_jsonlogger

router = APIRouter(prefix="/logs")

# no handler of its own: up to the root (lastResort until a root handler exists)
log = logging.getLogger(__name__)


class CorrelationFilter(logging.Filter):
    """A CorrelationIdFilter: an attribute every record gets on the handler it passes."""

    def filter(self, record):
        record.corr = "c-42"
        return True


class DropNoisy(logging.Filter):
    def filter(self, record):
        return "noisy" not in record.getMessage()


_ALL = ("%(name)s|%(levelname)-8s|%(levelno)d|%(module)s|%(filename)s|%(funcName)s|%(lineno)d|%(processName)s|"
        "%(threadName)s|%(corr)s|%(message)s")
std = logging.getLogger("dynapp.std")
_h = logging.StreamHandler(sys.stdout)
_h.setFormatter(logging.Formatter("%(asctime)s " + _ALL))
_h.addFilter(CorrelationFilter())
_h.addFilter(DropNoisy())
std.addHandler(_h)
std.setLevel(logging.DEBUG)
std.propagate = False

brace = logging.getLogger("dynapp.brace")
_hb = logging.StreamHandler(sys.stdout)
_hb.setFormatter(logging.Formatter("{asctime} [{levelname:^9}] {name}.{funcName}:{lineno} {corr} {message!r}", datefmt="%Y/%m/%d %H:%M",
                                   style="{", defaults={"corr": "-"}))
brace.addHandler(_hb)
brace.propagate = False

dollar = logging.getLogger("dynapp.dollar")
_hd = logging.StreamHandler()
_hd.setFormatter(logging.Formatter("$levelname ${name}: $message", style="$"))
dollar.addHandler(_hd)
dollar.propagate = False
dollar.setLevel("INFO")

# a JSON formatter: python-json-logger 4.0, renamed fields, on stdout
jlog = logging.getLogger("dynapp.json")
jlog.setLevel(logging.INFO)
jlog.handlers = []
_hj = logging.StreamHandler(sys.stdout)
_hj.setLevel(logging.INFO)
_hj.setFormatter(jsonlogger.JsonFormatter(
    fmt="%(asctime)s %(name)s %(levelname)s %(message)s %(pathname)s %(funcName)s %(lineno)d",
    rename_fields={"asctime": "timestamp", "levelname": "level", "pathname": "file", "funcName": "function", "lineno": "line"},
))
jlog.addHandler(_hj)
jlog.propagate = False

# another: the old import path, a field a filter adds, `extra`
jlog2 = logging.getLogger("dynapp.json2")
_hj2 = logging.StreamHandler(sys.stdout)
_hj2.setFormatter(legacy_jsonlogger.JsonFormatter("%(asctime)s %(name)s %(levelname)s %(corr)s %(message)s"))
_hj2.addFilter(CorrelationFilter())
jlog2.addHandler(_hj2)
jlog2.setLevel(logging.DEBUG)
jlog2.propagate = False

# a hierarchy: dynapp.tree has a handler, its descendants propagate to it (and on to the root)
tree = logging.getLogger("dynapp.tree")
_ht = logging.StreamHandler(sys.stdout)
_ht.setFormatter(logging.Formatter("tree %(name)s %(levelname)s %(message)s"))
tree.addHandler(_ht)
tree.setLevel(logging.WARNING)
leaf = logging.getLogger("dynapp.tree.a.b")
mid = logging.getLogger("dynapp.tree.a")

logging.addLevelName(25, "NOTICE")

logging.config.dictConfig({
    "version": 1,
    "disable_existing_loggers": False,
    "formatters": {"short": {"format": "dc:%(levelname)s:%(name)s:%(message)s"}},
    "filters": {"only_dc": {"name": "dynapp.dc"}},
    "handlers": {"out": {"class": "logging.StreamHandler", "stream": "ext://sys.stdout", "formatter": "short",
                         "filters": ["only_dc"], "level": "DEBUG"}},
    "loggers": {"dynapp.dc": {"handlers": ["out"], "level": "DEBUG", "propagate": False}},
})
dc = logging.getLogger("dynapp.dc")

adapter = logging.LoggerAdapter(std, {"corr": "adapted"})
merging = logging.LoggerAdapter(jlog2, {"tenant": 7})

log.warning("logs imported (%s)", "lastResort: the message only")


class Boom(Exception):
    pass


def _inner(n):
    return {"k": n}["missing"]


def _middle(n):
    try:
        return _inner(n)
    except KeyError as e:
        raise Boom(f"middle {n}") from e


@router.get("/levels")
async def levels():
    std.debug("debug %s", "shown")
    std.info("info %d", 2)
    std.warning("warning %(a)s", {"a": "mapping"})
    std.error("error")
    std.critical("critical %s %s", 1, [2])
    std.log(25, "custom level %s", "NOTICE")
    std.info("noisy, filtered out")
    leaf.info("below the tree's WARNING: dropped")
    leaf.warning("leaf warning: tree handler, then root")
    mid.error("mid error")
    brace.warning("brace %s", "styled")
    dollar.info("dollar %s", "styled")
    dollar.debug("dollar debug: below INFO")
    dc.debug("dictConfig'd")
    return {
        "effective": [std.getEffectiveLevel(), leaf.getEffectiveLevel(), log.getEffectiveLevel(), dollar.level],
        "enabled": [leaf.isEnabledFor(logging.INFO), leaf.isEnabledFor(logging.ERROR), std.isEnabledFor(5)],
        "handlers": [std.hasHandlers(), leaf.hasHandlers(), len(std.handlers), len(leaf.handlers)],
        "names": [logging.getLevelName(25), logging.getLevelName("NOTICE"), logging.getLevelName(33), leaf.name, mid.parent.name],
        "propagate": [std.propagate, leaf.propagate],
    }


@router.get("/extra")
async def extra(tenant: int = 3):
    std.info("with extra", extra={"corr": "from-extra"})
    jlog2.info("payment %s", "ok", extra={"tenant": tenant, "amount": 12.5, "tags": ["a", "b"], "who": None})
    jlog2.warning({"event": "dict message", "n": 1})
    jlog.info("json style %s", tenant)
    jlog.warning("unicode é ✓ %s", "ü")
    adapter.info("through an adapter %d", tenant)
    merging.info("adapter extra replaces the call's", extra={"call": True})
    try:
        std.info("clash", extra={"name": "x"})
        clash = None
    except KeyError as e:
        clash = str(e)
    return {"clash": clash}


@router.get("/exc")
async def exc(n: int = 1):
    try:
        _middle(n)
    except Boom:
        std.exception("caught %d", n)
        jlog.error("json with traceback", exc_info=True)
    try:
        1 / (n - n)
    except ZeroDivisionError as e:
        err = e
    std.error("exc_info given an exception", exc_info=err)
    std.error("exc_info outside except", exc_info=True)
    try:
        try:
            [][n]
        except IndexError:
            int("x")
    except ValueError:
        log.exception("context chain")
        jlog2.error("json chain", exc_info=True)
    return {"n": n}


@router.get("/bad-format")
async def bad_format():
    std.info("two %s %s", "only one")
    std.info("%d", "not a number")
    return {"after": True}


@router.get("/root")
async def root_functions():
    logging.info("root info: the root is WARNING")
    logging.warning("root warning %s", "via the module function")
    logging.error("root error")
    return {"root": logging.getLogger().level, "handlers": len(logging.getLogger().handlers)}


@router.get("/raise")
async def raise_unhandled(n: int = 0):
    """an exception out of the endpoint: uvicorn's "Exception in ASGI application" and its traceback"""
    if n:
        _middle(n)
    raise RuntimeError("unhandled")


@router.get("/sync")
def sync_endpoint():
    """a `def` endpoint: AnyIO's worker thread"""
    std.info("from a worker thread")
    return {"sync": True}


@router.get("/objects")
async def objects():
    std.info(42)
    std.info({"a": 1})
    std.info(["%s", "not formatted"])
    std.info("%s", (1, 2))
    std.info("%s %s", *(1, 2))
    std.info(f"f-string {1 + 1}")
    return {}


@router.get("/config")
async def config():
    """a logger reconfigured at run time: level by name, handler level, disabled, propagate"""
    t = logging.getLogger("dynapp.runtime")
    t.handlers.clear()
    h = logging.StreamHandler(sys.stdout)
    h.setFormatter(logging.Formatter("rt %(levelname)s %(message)s"))
    h.setLevel(logging.WARNING)
    t.addHandler(h)
    t.propagate = False
    t.setLevel("DEBUG")
    t.info("handler level drops it")
    t.warning("handler level lets it through")
    t.disabled = True
    t.error("disabled logger: dropped")
    t.disabled = False
    t.removeHandler(h)
    t.error("no handler at all: lastResort")
    return {"level": t.level, "handlers": len(t.handlers)}
