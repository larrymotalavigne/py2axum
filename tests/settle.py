"""Which writes of a scenario need the SETTLE pause, read from the reference application itself.

FastAPI (>= 0.121) runs the end of a `yield` dependency (`get_db`'s commit) once the response is sent, as it runs
BackgroundTasks and as an endpoint's own tasks go on: the next request may read before that work lands. The pause
is only useful after a write that can do work after its response, and it is applied the same way to both servers.
A write needs it when its route
- has a `yield` dependency anywhere in its tree, or a BackgroundTasks parameter;
- is defined in a module that starts work of its own (tasks, threads, timers, a response's `background=`);
- belongs to an application with user middlewares (they may act after the response), or is not found here (a raw
  ASGI route, a mount): kept by prudence.
Any other write has nothing left to run on either side once answered: without the pause, a difference can only
appear (a race), never be hidden.

    python -m tests.settle MODULE:APP < steps.json   # [[method, path], ...] -> [true|false, ...]
"""
from __future__ import annotations

import importlib
import inspect
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
READS = ("GET", "HEAD", "OPTIONS", "SQL", "WS")
SPAWNS = re.compile(r"\b(create_task|ensure_future|TaskGroup|create_task_group|to_thread|run_in_executor|run_in_threadpool"
                    r"|Thread|call_later|call_soon|BackgroundTask)\b")  # BackgroundTask: a response's background=


def _post_response(dep) -> bool:
    if dep.background_tasks_param_name:
        return True
    for d in dep.dependencies:
        c = d.call
        target = c if inspect.isfunction(c) or inspect.ismethod(c) else type(c).__call__  # a callable instance
        if inspect.isgeneratorfunction(target) or inspect.isasyncgenfunction(target):
            return True
        if _post_response(d):
            return True
    return False


def _routes(routes):
    for r in routes:
        if type(r).__name__ == "_IncludedRouter":  # FastAPI >= 0.140: included routers stay nested
            yield from _routes(r.effective_candidates())
        elif hasattr(r, "routes") and not hasattr(r, "dependant"):  # FastAPI < 0.140: an APIRouter's routes
            yield from _routes(r.routes)
        elif getattr(r, "dependant", None) is not None and getattr(r, "path_regex", None) is not None:
            yield r


def _spawns(module: str, cache: dict) -> bool:
    if module not in cache:
        try:
            cache[module] = bool(SPAWNS.search(inspect.getsource(sys.modules[module])))
        except (KeyError, OSError, TypeError):
            cache[module] = True
    return cache[module]


def compute(app_ref: str, steps: list) -> list[bool]:
    mod, _, attr = app_ref.partition(":")
    app = getattr(importlib.import_module(mod), attr or "app")
    every = bool(getattr(app, "user_middleware", None))
    routes = list(_routes(app.router.routes))
    cache: dict = {}
    out = []
    for method, path in steps:
        if method in READS:
            out.append(False)
            continue
        if every or path is None:
            out.append(True)
            continue
        p = path.split("?", 1)[0]
        route = next((r for r in routes if r.path_regex.match(p) and (not r.methods or method in r.methods)), None)
        out.append(route is None or _post_response(route.dependant) or _spawns(route.endpoint.__module__, cache))
    return out


def flags(app_ref: str, steps: list) -> list[bool]:
    """In a separate interpreter: importing the reference app configures its logging and its globals."""
    plain = [[s[0], s[1] if isinstance(s[1], str) else None] for s in steps]
    r = subprocess.run([sys.executable, "-m", "tests.settle", app_ref], input=json.dumps(plain), text=True,
                       capture_output=True, cwd=ROOT, check=True)
    return json.loads(r.stdout.strip().splitlines()[-1])


if __name__ == "__main__":
    # fixtures/extlib: the stand-ins of installed libraries (on the reference servers' PYTHONPATH too)
    sys.path[:0] = [str(ROOT), str(ROOT / "fixtures" / "extlib")]
    print(json.dumps(compute(sys.argv[1], json.load(sys.stdin))))
