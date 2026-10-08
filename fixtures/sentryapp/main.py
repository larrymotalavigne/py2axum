"""Sentry fixture: the sentry_sdk usage of real FastAPI projects (init in a setup module, tags and user per request, frontend error reports), run by
the reference and by the binary against the fake Sentry server (tests/sentry_sink.py, tests/sentry_check.py).

SENTRY_DSN points at the sink (project 1 for Python, 2 for the binary); SENTRY_PII toggles send_default_pii."""
import asyncio
import logging
import os
from datetime import UTC, datetime

import sentry_sdk
from fastapi import FastAPI, HTTPException, Request
from fastapi.responses import JSONResponse
from pydantic import BaseModel
from sentry_sdk.integrations.asyncio import AsyncioIntegration
from sentry_sdk.integrations.fastapi import FastApiIntegration
from sentry_sdk.integrations.logging import LoggingIntegration
from sentry_sdk.integrations.sqlalchemy import SqlalchemyIntegration

logger = logging.getLogger(__name__)

_TEST_EMAIL_PATTERNS = ("@example.com", "@example.org")
_HEALTH_CHECK_PATHS = {"/", "/health", "/api/health"}


def _before_send(event: dict, hint: dict) -> dict | None:
    """Drops events about test addresses (the projects' filter)."""
    if "exc_info" in hint:
        exc = hint["exc_info"][1]
        msg = str(exc) if exc else ""
        if any(p in msg for p in _TEST_EMAIL_PATTERNS):
            return None
    event_message = event.get("message", "") or ""
    log_entry = event.get("logentry", {}) or {}
    log_message = log_entry.get("message", "") or ""
    if any(p in f"{event_message} {log_message}" for p in _TEST_EMAIL_PATTERNS):
        return None
    event.setdefault("tags", {})["filtered"] = "no"
    return event


def _before_send_transaction(event: dict, hint: dict) -> dict | None:
    url_string = event.get("request", {}).get("url", "")
    try:
        from urllib.parse import urlparse

        path = urlparse(url_string).path
    except Exception:
        path = url_string
    if path in _HEALTH_CHECK_PATHS:
        return None
    return event


def _traces_sampler(sampling_context: dict) -> float:
    asgi_scope = sampling_context.get("asgi_scope", {})
    path = asgi_scope.get("path", "")
    if path in _HEALTH_CHECK_PATHS or path.startswith("/notrace"):
        return 0.0
    return 1.0


def init_sentry(service_name: str) -> None:
    dsn = os.environ.get("SENTRY_DSN", "")
    if not dsn:
        logger.info("Sentry disabled")
        return
    release = os.environ.get("GIT_SHA", "unknown")
    sentry_sdk.init(
        dsn=dsn,
        environment="test",
        traces_sampler=_traces_sampler,
        enable_tracing=True,
        send_default_pii=os.environ.get("SENTRY_PII") == "1",
        attach_stacktrace=True,
        max_value_length=4096,
        release=f"sentryapp@{release[:8]}" if release != "unknown" else None,
        server_name=service_name,
        before_send=_before_send,
        before_send_transaction=_before_send_transaction,
        integrations=[
            FastApiIntegration(),
            SqlalchemyIntegration(),
            LoggingIntegration(level=logging.INFO, event_level=logging.ERROR),
            AsyncioIntegration(),
        ],
    )
    sentry_sdk.set_tag("service", service_name)
    sentry_sdk.capture_message(f"Deployed {service_name}", level="info")


init_sentry("api")
app = FastAPI()


class AppError(Exception):
    """A project error with a status code, answered by its own handler (captured as a 5xx)."""

    def __init__(self, status_code: int, message: str):
        super().__init__(message)
        self.status_code = status_code


@app.exception_handler(AppError)
async def app_error_handler(request: Request, exc: AppError):
    return JSONResponse({"error": str(exc)}, status_code=exc.status_code)


@app.middleware("http")
async def request_id(request: Request, call_next):
    """A request-id middleware: a tag per request, set before the route runs."""
    sentry_sdk.set_tag("request_id", "rid-1")
    if request.url.path == "/mw-boom":
        raise RuntimeError("middleware boom")
    return await call_next(request)


def set_sentry_user(user_id: int | None, email: str | None = None, role: str | None = None) -> None:
    if not user_id:
        return
    sentry_sdk.set_user({"id": str(user_id), "email": email})
    sentry_sdk.set_tag("user_role", role or "user")


@app.get("/health")
async def health():
    return {"ok": True}


@app.get("/msg")
async def msg(text: str = "hello", level: str = "info"):
    event_id = sentry_sdk.capture_message(f"msg {text}", level=level)
    return {"sent": event_id is not None}


@app.get("/msg-default")
def msg_default():
    sentry_sdk.capture_message("plain message")
    return {"ok": True}


@app.get("/exc")
async def exc(n: int = 0):
    try:
        1 / n
    except ZeroDivisionError as e:
        sentry_sdk.capture_exception(e)
    return {"ok": True}


@app.get("/exc-implicit")
async def exc_implicit(key: str = "missing"):
    try:
        {"a": 1}[key]
    except KeyError:
        sentry_sdk.capture_exception()
    return {"ok": True}


@app.get("/user/{user_id}")
async def user(user_id: int, email: str | None = None):
    set_sentry_user(user_id, email=email, role="admin")
    sentry_sdk.set_tag("portal", "owner")
    sentry_sdk.set_context("plan", {"name": "pro", "seats": 3})
    sentry_sdk.set_extra("note", "extra value")
    sentry_sdk.add_breadcrumb(category="audit", message=f"viewed {user_id}", level="info", data={"user": user_id})
    sentry_sdk.add_breadcrumb(message="second crumb")
    sentry_sdk.capture_message(f"user {user_id} looked", level="warning")
    return {"user": user_id}


@app.get("/leak")
async def leak():
    """A tag set in one request must not reach the next one (isolation scope per request)."""
    sentry_sdk.set_tag("leaked", "yes")
    return {"ok": True}


@app.get("/after-leak")
async def after_leak():
    sentry_sdk.capture_message("after leak")
    return {"ok": True}


@app.get("/boom")
async def boom(user_id: int = 0):
    if user_id:
        set_sentry_user(user_id, email="u@atom.fr")
    raise RuntimeError(f"boom {user_id}")


@app.get("/boom-sync")
def boom_sync():
    raise ValueError("sync boom")


@app.get("/http/{code}")
async def http_error(code: int):
    raise HTTPException(status_code=code, detail=f"error {code}")


@app.get("/log")
async def log(what: str = "error"):
    logger.info("info line %s", what)
    logger.warning("warning line")
    if what == "error":
        logger.error("error line %s %d", what, 3)
    elif what == "exception":
        try:
            int("x")
        except ValueError:
            logger.exception("parse failed")
    elif what == "root":
        logging.error("root error")
    elif what == "critical":
        logger.critical("very bad")
    return {"ok": True}


@app.get("/scope")
async def scope_route(source: str | None = None):
    with sentry_sdk.new_scope() as scope:
        scope.set_tag("service", "frontend")
        scope.set_tag("source", source or "browser")
        scope.set_context("frontend", {"url": "/x", "type": None, "stack": "at f"[:10]})
        sentry_sdk.capture_message("[frontend] oops"[:1000], level="error")
    sentry_sdk.capture_message("after scope")
    return {"success": True}


@app.get("/push-scope")
async def push_scope_route():
    with sentry_sdk.push_scope() as scope:
        scope.set_tag("pushed", "1")
        scope.set_user({"id": "p"})
        scope.set_level("fatal")
        sentry_sdk.capture_message("inside push")
    sentry_sdk.capture_message("outside push")
    return {"ok": True}


@app.get("/filtered")
async def filtered():
    sentry_sdk.capture_message("mail to bob@example.com failed")
    try:
        raise ValueError("bad address alice@example.org")
    except ValueError as e:
        sentry_sdk.capture_exception(e)
    sentry_sdk.capture_message("kept")
    return {"ok": True}


async def _job(n: int) -> None:
    sentry_sdk.add_breadcrumb(category="job", message=f"job {n}")
    if n:
        raise LookupError(f"job failed {n}")


@app.get("/task")
async def task(n: int = 1):
    t = asyncio.create_task(_job(n))
    await asyncio.sleep(0.05)
    return {"done": t.done()}


class Item(BaseModel):
    name: str
    price: float


@app.post("/items")
async def items(item: Item):
    if item.price < 0:
        raise ValueError(f"negative price for {item.name}")
    return item


@app.get("/notrace/boom")
async def notrace_boom():
    raise KeyError("untraced")


@app.get("/stats")
async def stats():
    """No Sentry call: only its transaction is sent."""
    return {"n": 1}


@app.get("/bare-capture")
async def bare_capture():
    return {"id": sentry_sdk.capture_exception()}


@app.post("/form")
async def form_route():
    sentry_sdk.capture_message("form posted")
    return {"ok": True}


@app.get("/extras")
async def extras():
    sentry_sdk.set_tags({"t1": "a", "t2": 2})
    sentry_sdk.set_extra("many", {f"k{i}": i for i in range(12)})
    sentry_sdk.set_extra("deep", {"a": {"b": {"c": {"d": {"e": {"f": 1}}}}}})
    sentry_sdk.set_extra("long", "x" * 5000)
    sentry_sdk.set_extra("password", "hunter2")
    sentry_sdk.set_context("when", {"at": datetime(2026, 1, 2, 3, 4, 5, tzinfo=UTC), "pair": (1, 2), "inf": float("inf")})
    sentry_sdk.add_breadcrumb(category="db", message="q", data={"token": "t", "rows": list(range(15))})
    sentry_sdk.capture_message("extras")
    return {"ok": True}


@app.get("/handled-custom")
async def handled_custom():
    raise AppError(503, "service down")


@app.get("/mw-boom")
async def mw_boom():
    return {"never": True}


@app.get("/dedupe")
async def dedupe():
    try:
        raise ValueError("twice")
    except ValueError as e:
        sentry_sdk.capture_exception(e)
        raise


@app.get("/capture-kwargs")
async def capture_kwargs():
    sentry_sdk.capture_message("kw", level="debug", tags={"a": "b"}, extras={"x": 1}, user={"id": "k"}, fingerprint=["f"])
    try:
        raise OSError("disk")
    except OSError as e:
        sentry_sdk.capture_exception(e, tags={"c": "d"})
    sentry_sdk.capture_message("after kw")
    return {"ok": True}
