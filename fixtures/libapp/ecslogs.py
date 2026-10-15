"""ecs-logging's StdlibFormatter: the ECS fields of a record, `extra=` (nested and dotted, global `extra`),
`exclude_fields`, `stack_trace_limit`, `ensure_ascii`, exceptions (`stack_info=True` differs, docs/advanced/logging.md), the formatter from
dictConfig. Compared line for line by the log conformance (tests/logcmp.py)."""
import logging
import logging.config
import sys

import ecs_logging
from fastapi import APIRouter

router = APIRouter(prefix="/ecs")


class Tenant(logging.Filter):
    def filter(self, record):
        record.tenant = {"id": 7, "plan": "pro"}
        return True


def _logger(name, formatter, stream=sys.stdout, filters=()):
    lg = logging.getLogger(name)
    lg.handlers = []
    h = logging.StreamHandler(stream)
    h.setFormatter(formatter)
    for f in filters:
        h.addFilter(f)
    lg.addHandler(h)
    lg.setLevel(logging.DEBUG)
    lg.propagate = False
    return lg


plain = _logger("libapp.ecs", ecs_logging.StdlibFormatter())
tuned = _logger("libapp.ecs.tuned", ecs_logging.StdlibFormatter(
    extra={"service.name": "libapp", "labels": {"env": "test"}}, exclude_fields=["log.origin", "error.stack_trace", "x.secret"],
    ensure_ascii=False), stream=sys.stderr, filters=[Tenant()])
short = _logger("libapp.ecs.short", ecs_logging.StdlibFormatter(stack_trace_limit=1))
silent = _logger("libapp.ecs.silent", ecs_logging.StdlibFormatter(stack_trace_limit=0, exclude_fields=("message",)))

logging.config.dictConfig({
    "version": 1,
    "disable_existing_loggers": False,
    "formatters": {"ecs": {"()": "ecs_logging.StdlibFormatter", "exclude_fields": ["process"]}},
    "handlers": {"ecs_out": {"class": "logging.StreamHandler", "stream": "ext://sys.stdout", "formatter": "ecs"}},
    "loggers": {"libapp.ecsdc": {"handlers": ["ecs_out"], "level": "INFO", "propagate": False}},
})
dc = logging.getLogger("libapp.ecsdc")


def _fail(n):
    return [0][n]


def _wrap(n):
    try:
        return _fail(n)
    except IndexError as e:
        raise ValueError(f"wrapped {n}") from e


@router.get("/basic")
async def basic(n: int = 2):
    plain.debug("debug %d", n)
    plain.info("info %s é", "ü")
    plain.warning({"not": "a string"})
    plain.info("with extra", extra={"http": {"request": {"method": "GET"}, "status": 200}, "user.id": n, "tags": ["a", 1],
                                    "none": None, "obj": {1}, "when": __import__("datetime").date(2020, 1, 2), "f": 1.5, "flag": True})
    plain.info("elastic apm ids", extra={"elasticapm_trace_id": "t1", "elasticapm_span_id": "s1",
                                          "elasticapm_labels.k": "dropped", "elasticapm_service_name": "svc"})
    tuned.info("tuned é %s", n, extra={"x": {"secret": 1, "kept": 2}})
    dc.info("from dictConfig %d", n)
    return {"n": n}


@router.get("/errors")
async def errors(n: int = 3):
    try:
        _wrap(n)
    except ValueError:
        plain.exception("caught %d", n)
        short.error("short", exc_info=True)
        silent.error("silent", exc_info=True)
        tuned.exception("tuned")
    plain.error("outside", exc_info=True)
    return {"n": n}


@router.get("/bad")
async def bad(what: str):
    try:
        if what == "limit":
            ecs_logging.StdlibFormatter(stack_trace_limit=-1)
        elif what == "limit_type":
            ecs_logging.StdlibFormatter(stack_trace_limit="1")
        elif what == "exclude":
            ecs_logging.StdlibFormatter(exclude_fields="message")
        elif what == "exclude_items":
            ecs_logging.StdlibFormatter(exclude_fields=[1])
    except (TypeError, ValueError) as e:
        return {"err": type(e).__name__, "msg": str(e)}
    return {}
