"""sys.settrace: a trace function counting the calls of one module's functions, their outcome and, for
exceptions, the line and the depth where they came from (extract_tb)."""
import sys
import threading
from traceback import extract_tb

from fastapi import APIRouter
from prometheus_client import CollectorRegistry, Counter, generate_latest

from . import traced

router = APIRouter(prefix="/traced")
REG = CollectorRegistry()
CALLS = Counter("traced_calls", "Calls per function", ["path"], registry=REG)
OUTCOMES = Counter("traced_outcomes", "Outcome per function", ["path", "event", "class_name"], registry=REG)
ERRORS = Counter("traced_errors", "Errors per function", ["path", "line", "class_name", "frame_level"], registry=REG)
EVENTS: list = []
WATCHED = "fixtures.dynapp.traced"


def on_event(frame, event, arg):
    if event != "call" or frame.f_globals.get("__name__") != WATCHED:
        return on_event
    path = f"{WATCHED}.{frame.f_code.co_qualname}"
    CALLS.labels(path=path).inc()
    failed = []

    def on_exit(exit_frame, exit_event, exit_arg):
        if failed or exit_event not in ("return", "exception"):
            return on_exit
        name = (exit_arg[0] if exit_event == "exception" else type(exit_arg)).__name__
        OUTCOMES.labels(path=path, event=exit_event, class_name=name).inc()
        EVENTS.append([exit_frame.f_code.co_name, exit_event, name, exit_frame.f_lineno == exit_frame.f_code.co_firstlineno])
        if exit_event == "exception":
            stack = extract_tb(exit_arg[2])
            ERRORS.labels(path=path, line=exit_arg[2].tb_lineno, class_name=name, frame_level=len(stack) - 1).inc()
            failed.append(True)
        return on_exit

    return on_exit


@router.get("/run")
async def run():
    EVENTS.clear()
    sys.settrace(on_event)
    threading.settrace_all_threads(on_event)
    out = {"installed": sys.gettrace() is on_event}
    try:
        out["first"] = traced.Shelf([3, 4]).first()
        out["tolerant"] = traced.tolerant({}, "x")
        out["compute"] = await traced.compute(5)
        out["lambda"] = traced.twice(4)
        try:
            await traced.failing()
        except KeyError as e:
            out["failing"] = repr(e)
    finally:
        sys.settrace(None)
        threading.settrace_all_threads(None)
    out["after"] = traced.double(1)
    out["stops"] = [StopAsyncIteration.__name__, issubclass(StopAsyncIteration, Exception), repr(StopAsyncIteration("x"))]
    out["events"] = EVENTS
    out["metrics"] = generate_latest(REG).decode()
    return out
