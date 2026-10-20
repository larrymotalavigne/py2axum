"""--python-side auto (py2axum/fallback.py): a refusal outside the routes leaves the lifespan, or the whole
application, to Python instead of stopping the generation; without auto it stays an error with file:line."""
import json
import re
import textwrap

import pytest

from py2axum.__main__ import main

HEAD = """
from contextlib import asynccontextmanager
import logging

from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse

import ujson  # not in the library map: any call to it is refused

CACHE = {}
"""

OK = """

@app.get("/ok")
async def ok():
    return {"ok": 1}
"""


def project(tmp_path, body: str):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(HEAD + textwrap.dedent(body) + OK)
    (tmp_path / ".git").mkdir()
    return pkg


def gen(tmp_path, pkg, *flags):
    return main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out"), *flags])


def flat(path) -> str:
    """A generated file with its whitespace collapsed (rustfmt formats it only where it is installed)."""
    return " ".join(path.read_text().replace("[", "[ ").replace("]", " ]").split())


NO_ROUTES = "pub static ROUTES: &[ dynrt::web::RouteDef ] = &[ ];"


def relayed(tmp_path) -> tuple[list[str], list[str]]:
    """The paths and mount prefixes main.rs relays to Python."""
    main_rs = " ".join((tmp_path / "out" / "src" / "main.rs").read_text().split())
    m = re.search(r"set_python_side\(\s*&\[(.*?)\],\s*&\[(.*?)\],?\s*\)", main_rs)
    return re.findall(r'"([^"]*)"', m.group(1)), re.findall(r'"([^"]*)"', m.group(2))


LIFESPAN = """
@asynccontextmanager
async def lifespan(app):
    {line}
    yield

app = FastAPI(lifespan=lifespan)
"""


def test_lifespan_alone(tmp_path, capsys):
    """Nothing translated reads what the lifespan sets: it alone runs in Python, the routes stay native."""
    pkg = project(tmp_path, LIFESPAN.format(line='app.state.boot = ujson.dumps({"a": 1})'))
    assert gen(tmp_path, pkg) == 1
    assert "main.py:14: library call `ujson.dumps()`" in capsys.readouterr().err
    assert gen(tmp_path, pkg, "--python-side", "auto") == 0
    err = capsys.readouterr().err
    assert "python-side (auto): lifespan — " in err and "ujson.dumps()" in err
    assert relayed(tmp_path) == ([], [])
    gen_rs = flat(tmp_path / "out" / "src" / "gen.rs")
    assert "pub async fn lifespan(_cx: &Cx) -> R<Option<V>> { Ok(None)" in gen_rs
    assert '"/ok"' in gen_rs


@pytest.mark.parametrize("line, reader, hit", [
    ('app.state.boot = ujson.dumps({"a": 1})', "request.app.state.boot", "state.boot"),
    ('CACHE["k"] = ujson.dumps({"a": 1})', "len(CACHE)", "proj.main.CACHE"),
    ('global CACHE\n    CACHE = ujson.loads("{}")', "len(CACHE)", "proj.main.CACHE"),
])
def test_lifespan_readers_move(tmp_path, capsys, line, reader, hit):
    """A translated route reading what the Python lifespan sets in the process goes to Python with it."""
    body = LIFESPAN.format(line=line) + f"""

@app.get("/r")
async def r(request: Request):
    return {{"v": {reader}}}
"""
    pkg = project(tmp_path, body)
    assert gen(tmp_path, pkg, "--python-side", "auto") == 0
    err = capsys.readouterr().err
    assert "python-side (auto): /r (GET r) — " in err and f"reads `{hit}`, which the lifespan (left to Python) sets" in err
    assert "[P2A0316]" in err
    assert relayed(tmp_path) == (["/r"], [])


@pytest.mark.parametrize("line, why", [
    ("logging.basicConfig(level=logging.DEBUG)\n    ujson.dumps({})", "logging.basicConfig"),
    ("fill(CACHE)\n    ujson.dumps({})", "an object it was given"),
])
def test_lifespan_unbounded_moves_the_app(tmp_path, capsys, line, why):
    """A change of the lifespan py2axum cannot bound: the whole application stays in Python."""
    pkg = project(tmp_path, "\ndef fill(d):\n    d['k'] = 1\n\n" + LIFESPAN.format(line=line))
    assert gen(tmp_path, pkg, "--python-side", "auto") == 0
    err = capsys.readouterr().err
    assert "python-side (auto): the whole application — " in err and why in err
    assert relayed(tmp_path) == ([], [""])
    assert NO_ROUTES in flat(tmp_path / "out" / "src" / "gen.rs")


@pytest.mark.parametrize("line, moved", [
    ("logger.add('api.log', rotation='00:00')", True),  # a sink the binary's logger would not have
    ("logger.remove()\n    ujson.dumps({})", True),
    ("logger.success('started')\n    ujson.dumps({})", False),  # a record only: the Python process prints it
])
def test_lifespan_loguru(tmp_path, capsys, line, moved):
    """loguru is native (dynrt/loguru.rs): a lifespan left to Python that adds or removes a sink leaves the whole
    application to Python"""
    pkg = project(tmp_path, "\nfrom loguru import logger\n" + LIFESPAN.format(line=line))
    assert gen(tmp_path, pkg, "--python-side", "auto") == 0
    err = capsys.readouterr().err
    assert ("python-side (auto): the whole application — " in err) == moved
    if moved:
        assert "the lifespan calls `loguru.logger." in err
        assert relayed(tmp_path) == ([], [""])
    else:
        assert "python-side (auto): lifespan — " in err and relayed(tmp_path) == ([], [])


def test_on_event(tmp_path, capsys):
    pkg = project(tmp_path, """
app = FastAPI()


@app.on_event("startup")
async def start():
    ujson.dumps({})
""")
    assert gen(tmp_path, pkg) == 1
    capsys.readouterr()
    assert gen(tmp_path, pkg, "--python-side", "auto") == 0
    assert "python-side (auto): lifespan — " in capsys.readouterr().err
    assert relayed(tmp_path) == ([], [])


@pytest.mark.parametrize("body", [
    """
app = FastAPI()


@app.middleware("http")
async def mw(request: Request, call_next):
    ujson.dumps({})
    return await call_next(request)
""",
    """
app = FastAPI()


@app.exception_handler(ValueError)
async def handler(request: Request, exc: ValueError):
    return JSONResponse({"e": ujson.dumps({})}, status_code=400)
""",
    """
app = FastAPI(telemetry=True)
""",
])
def test_whole_application(tmp_path, capsys, body):
    """A middleware, an exception handler, an app option: every request is relayed to Python."""
    pkg = project(tmp_path, body)
    assert gen(tmp_path, pkg) == 1
    capsys.readouterr()
    assert gen(tmp_path, pkg, "--python-side", "auto") == 0
    err = capsys.readouterr().err
    assert "python-side (auto): the whole application — " in err and "main.py:" in err
    assert relayed(tmp_path) == ([], [""])
    gen_rs = flat(tmp_path / "out" / "src" / "gen.rs")
    assert NO_ROUTES in gen_rs
    assert "fn f_" not in gen_rs  # no project code in the binary


def test_whole_application_with_websockets_is_relayed(tmp_path, capsys):
    """The whole application left to Python, WebSocket routes included: the binary relays their connections too"""
    pkg = project(tmp_path, """
app = FastAPI()


@app.middleware("http")
async def mw(request: Request, call_next):
    ujson.dumps({})
    return await call_next(request)


@app.websocket("/ws")
async def ws(websocket):
    await websocket.accept()
""")
    assert gen(tmp_path, pkg, "--python-side", "auto") == 0
    err = capsys.readouterr().err
    assert re.search(r"the whole application — .*main\.py:\d+: library call `ujson\.dumps\(\)`", err), err
    assert NO_ROUTES in flat(tmp_path / "out" / "src" / "gen.rs")
    assert relayed(tmp_path) == ([], [""])


def test_check_tells_the_same(tmp_path, capsys):
    """`py2axum check --python-side auto`: each route's status and reason, the lifespan or whole app in the notes."""
    body = LIFESPAN.format(line='app.state.boot = ujson.dumps({"a": 1})') + """

@app.get("/r")
async def r(request: Request):
    return {"v": request.app.state.boot}
"""
    pkg = project(tmp_path, body)
    rc = main(["check", str(pkg), "--root", str(tmp_path), "--python-side", "auto", "--json"])
    data = json.loads(capsys.readouterr().out)
    assert rc == 0 and data["global"] == [] and data["summary"]["generates"]
    st = {r["path"]: (r["status"], r["code"]) for r in data["routes"]}
    assert st == {"/ok": ("native", None), "/r": ("python-side", "P2A0316")}
    assert any(n.startswith("python-side (auto): lifespan — ") for n in data["notes"])
    # without auto: the lifespan's error is global, the generation would fail
    rc = main(["check", str(pkg), "--root", str(tmp_path), "--json"])
    data = json.loads(capsys.readouterr().out)
    assert rc == 1 and any("ujson.dumps()" in g["error"] for g in data["global"])


def test_check_whole_application(tmp_path, capsys):
    pkg = project(tmp_path, """
app = FastAPI()


@app.middleware("http")
async def mw(request: Request, call_next):
    ujson.dumps({})
    return await call_next(request)
""")
    rc = main(["check", str(pkg), "--root", str(tmp_path), "--python-side", "auto", "--json"])
    data = json.loads(capsys.readouterr().out)
    assert rc == 0 and data["global"] == []
    assert [(r["path"], r["status"]) for r in data["routes"]] == [("/ok", "python-side")]
    assert "ujson.dumps()" in data["routes"][0]["reason"]
    assert any(n.startswith("python-side (auto): the whole application — ") for n in data["notes"])


ROUTE_ONLY = """
app = FastAPI()


@app.get("/bad")
async def bad():
    return {"v": ujson.dumps({})}
"""

LIFESPAN_READER = LIFESPAN.format(line='app.state.boot = ujson.dumps({"a": 1})') + """

@app.get("/r")
async def r(request: Request):
    return {"v": request.app.state.boot}
"""

WHOLE_APP = """
app = FastAPI()


@app.middleware("http")
async def mw(request: Request, call_next):
    ujson.dumps({})
    return await call_next(request)
"""


@pytest.mark.parametrize("body", [ROUTE_ONLY, LIFESPAN_READER, WHOLE_APP], ids=["route", "lifespan", "whole"])
def test_check_and_generation_agree(tmp_path, capsys, body):
    """One source of truth: the routes `check` calls python-side are exactly those the generated binary relays
    (and native + python-side covers every route)."""
    pkg = project(tmp_path, body)
    assert main(["check", str(pkg), "--root", str(tmp_path), "--python-side", "auto", "--json"]) == 0
    data = json.loads(capsys.readouterr().out)
    assert data["summary"]["generates"] and data["summary"]["refused"] == 0
    assert data["summary"]["native"] + data["summary"]["python-side"] == data["summary"]["total"]
    assert gen(tmp_path, pkg, "--python-side", "auto") == 0
    paths, mounts = relayed(tmp_path)
    side = {r["path"] for r in data["routes"] if r["status"] == "python-side"}
    if mounts == [""]:  # the whole application is relayed
        assert side == {r["path"] for r in data["routes"]}
    else:
        assert side == set(paths)


def test_check_explicit_lifespan(tmp_path, capsys):
    """`--python-side lifespan` without auto: the lifespan runs in Python, `check` and the generation both accept it."""
    pkg = project(tmp_path, LIFESPAN.format(line='app.state.boot = ujson.dumps({"a": 1})'))
    assert main(["check", str(pkg), "--root", str(tmp_path), "--python-side", "lifespan", "--json"]) == 0
    data = json.loads(capsys.readouterr().out)
    assert data["global"] == [] and data["summary"]["generates"]
    assert gen(tmp_path, pkg, "--python-side", "lifespan") == 0


SESSION_GET = """
app = FastAPI()


@app.get("/s")
async def s(request: Request):
    return {"v": request.scope.get("session")}
"""


def test_scope_get_session_without_the_middleware(tmp_path, capsys):
    """`request.scope.get("session")` is None where no SessionMiddleware sets it (a rate limiter reading it): the route stays
    native; with the middleware in the project the read is refused (the binary's scope would not hold the session)."""
    pkg = project(tmp_path, SESSION_GET)
    assert main(["check", str(pkg), "--root", str(tmp_path), "--json"]) == 0
    assert {r["path"]: r["status"] for r in json.loads(capsys.readouterr().out)["routes"]}["/s"] == "native"
    pkg2 = tmp_path / "p2"
    pkg2.mkdir()
    pkg = project(pkg2, SESSION_GET.replace("app = FastAPI()", "app = FastAPI()\nfrom starlette.middleware.sessions import SessionMiddleware  # noqa\n_ = SessionMiddleware"))
    assert main(["check", str(pkg), "--root", str(pkg2), "--json"]) == 1
    data = json.loads(capsys.readouterr().out)
    assert {r["path"]: r["status"] for r in data["routes"]}["/s"] == "refused"


FACTORY_LIFESPAN = """
TESTING = False


def create_app():
    label = "boot"
    if not TESTING:
        @asynccontextmanager
        async def lifespan(app):
            {line}
            yield

        app = FastAPI(lifespan=lifespan)
    else:
        app = FastAPI()

    @app.get("/ok")
    async def ok():
        return {{"ok": 1}}

    @app.get("/r")
    async def r(request: Request):
        return {{"v": {reader}}}

    return app


app = create_app()
"""


def factory_project(tmp_path, line: str, reader: str = "1"):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(HEAD + FACTORY_LIFESPAN.format(line=line, reader=reader))
    (tmp_path / ".git").mkdir()
    return pkg


def test_factory_lifespan_native(tmp_path, capsys):
    """A lifespan the factory defines (under an `if`): native, with the factory's locals, guarded by the `if`."""
    pkg = factory_project(tmp_path, "app.state.boot = label", "request.app.state.boot")
    assert gen(tmp_path, pkg) == 0, capsys.readouterr().err
    gen_rs = flat(tmp_path / "out" / "src" / "gen.rs")
    assert "fw_f_proj_main__create_app__locals__lifespan" in gen_rs
    assert "fl_proj_main__create_app__label" in gen_rs
    assert "return Ok(None);" in gen_rs  # the `if not TESTING:` guard


def test_factory_lifespan_left_to_python(tmp_path, capsys):
    """Not translatable: it alone runs in Python (it was read as not being a function of the project)."""
    pkg = factory_project(tmp_path, 'ujson.dumps({"a": 1})')
    assert gen(tmp_path, pkg, "--python-side", "auto") == 0
    err = capsys.readouterr().err
    assert "python-side (auto): lifespan — " in err and "ujson.dumps()" in err
    assert relayed(tmp_path) == ([], [])


@pytest.mark.parametrize("reader, moved", [
    # the lifespan only initialises it to the default the reader gives getattr: same value in the binary
    ('getattr(request.app.state, "count", 0)', False),
    ('getattr(request.app.state, "count", 1)', True),
    ("request.app.state.count", True),
])
def test_lifespan_initialised_state(tmp_path, capsys, reader, moved):
    pkg = factory_project(tmp_path, 'app.state.count = 0\n            ujson.dumps({"a": 1})', reader)
    assert gen(tmp_path, pkg, "--python-side", "auto") == 0
    err = capsys.readouterr().err
    assert relayed(tmp_path) == ((["/r"], []) if moved else ([], [])), err
