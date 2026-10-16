"""tests/settle.py: the SETTLE pause is kept after every write that can do work once answered."""
import sys
import textwrap

import pytest

sys.path.insert(0, str(__import__("pathlib").Path(__file__).resolve().parent))
import settle  # noqa: E402

APP = """
from fastapi import APIRouter, BackgroundTasks, Depends, FastAPI

app = FastAPI()
router = APIRouter(prefix="/r")


async def session():
    yield 1


def plain_dep():
    return 1


@app.post("/plain")
async def plain(x: int = Depends(plain_dep)):
    return {}


@app.post("/yield")
async def with_yield(s=Depends(session)):
    return {}


@app.post("/bg")
async def with_bg(tasks: BackgroundTasks):
    return {}


@router.post("/nested")
async def nested(s=Depends(session)):
    return {}


@router.post("/flat")
async def flat():
    return {}


app.include_router(router)
"""

SPAWNING = """
import asyncio
from fastapi import FastAPI

app = FastAPI()


@app.post("/later")
async def later():
    asyncio.get_running_loop().create_task(asyncio.sleep(0))
    return {}
"""


@pytest.fixture
def pkg(tmp_path, monkeypatch):
    def make(name, source, middleware=False):
        d = tmp_path / name
        d.mkdir()
        (d / "__init__.py").write_text("")
        extra = "\nfrom starlette.middleware.gzip import GZipMiddleware\napp.add_middleware(GZipMiddleware)\n" if middleware else ""
        (d / "main.py").write_text(textwrap.dedent(source) + extra)
        monkeypatch.syspath_prepend(str(tmp_path))
        return f"{name}.main:app"
    return make


def test_writes_with_post_response_work(pkg):
    ref = pkg("settleapp", APP)
    steps = [("GET", "/plain"), ("POST", "/plain"), ("POST", "/yield"), ("POST", "/bg"), ("POST", "/r/nested"),
             ("POST", "/r/flat?x=1"), ("POST", "/nowhere"), ("SQL", "SELECT 1"), ("POST", None)]
    assert settle.compute(ref, steps) == [False, False, True, True, True, False, True, False, True]


def test_module_that_starts_tasks(pkg):
    assert settle.compute(pkg("spawnapp", SPAWNING), [("POST", "/later")]) == [True]


def test_user_middlewares_keep_every_pause(pkg):
    assert settle.compute(pkg("mwapp", APP, middleware=True), [("POST", "/plain"), ("GET", "/plain")]) == [True, False]
