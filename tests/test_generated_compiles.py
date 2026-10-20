"""What generates compiles: `cargo check` of a crate whose generators the routes call in every way, and whose
functions that do not translate (stubs that raise, never reached by a route) are called the way a translated
function would be. The 18/10 measure of --translate-libs: et_xmlfile generated, then failed to compile (E0061:
the stub of a `@contextmanager` function lacked the yielder its caller passed; a generator method called through
its method wrapper lacked it too).

Skipped without cargo. The target directory is shared between runs (PY2AXUM_CHECK_TARGET, default
~/.cache/py2axum/check-target): the first run checks the runtime's dependencies (a few minutes), later runs only
the crate."""
import os
import shutil
import subprocess
import textwrap
from pathlib import Path

import pytest
from test_libsrc import make_dist

from py2axum import libsrc
from py2axum.__main__ import main

APP = '''
import contextlib
from contextlib import contextmanager

import xmlish
from fastapi import FastAPI
from fastapi.responses import StreamingResponse

app = FastAPI()


@contextmanager
def opened(path):
    with contextlib.ExitStack() as st:  # not translated: the function becomes a stub that raises
        yield st


def chunks(n):
    yield contextlib.ExitStack()


class Holder:
    def use(self):
        with opened("x") as f:
            return f

    def stream(self):
        return StreamingResponse(chunks(2))


class Bag:
    def __init__(self, items):
        self.items = items

    async def upper(self):
        for s in self.items:
            yield s.upper()

    @classmethod
    async def named(cls, n):
        for i in range(n):
            yield cls.__name__ + str(i)

    async def pairs(self):
        async for s in self.upper():
            yield s + s


@app.get("/t")
async def t():
    bag = Bag(["a", "b"])
    return {"h": Holder.__name__, "x": xmlish.XmlFile.__name__, "p": [s async for s in bag.pairs()],
            "n": [s async for s in Bag.named(2)]}
'''

# et_xmlfile's shape: a module-level @contextmanager function that does not translate, entered by a method (refused
# on main as a library value, translated by the method decorators' series), and an async generator that does not
# translate iterated by a generator method
XMLISH = {
    "xmlish/__init__.py": "from .writer import XmlFile\n",
    "xmlish/writer.py": textwrap.dedent('''
        import contextlib
        from contextlib import contextmanager


        @contextmanager
        def _get_writer(file, encoding=None):
            with contextlib.ExitStack() as stack:
                yield stack


        async def _chunks(file):
            with contextlib.ExitStack() as stack:
                yield stack


        class XmlFile:
            def __init__(self, f, encoding=None):
                self.f = f
                self.encoding = encoding

            def __enter__(self):
                self.cm = _get_writer(self.f, self.encoding)
                return self.cm.__enter__()

            def __exit__(self, *exc):
                return self.cm.__exit__(*exc)

            async def write(self):
                async for c in _chunks(self.f):
                    yield c
    '''),
}


@pytest.fixture(autouse=True)
def _reset():
    yield
    libsrc.configure("none")


@pytest.mark.skipif(shutil.which("cargo") is None, reason="cargo is not installed")
def test_generators_and_stubs_compile(tmp_path):
    make_dist(tmp_path / ".venv", "xmlish", "1.0", XMLISH)
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(APP)
    (tmp_path / ".git").mkdir()  # the project's root: no lock or .venv above it
    out = tmp_path / "out"
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(out), "--name", "gencheck_axum",
                 "--venv", str(tmp_path / ".venv"), "--translate-libs", "auto"]) == 0
    gen = (out / "src" / "gen.rs").read_text()
    # the shapes this test is about are in the crate: stubs of generator functions, generator methods' wrappers
    assert gen.count("_: &dynrt::web::Yielder) -> R {\n    Err(Exc::runtime(") >= 2
    assert "fn mw_f_proj_main__Bag_named" in gen and "fn mw_f_xmlish_writer__XmlFile_write" in gen
    target = os.environ.get("PY2AXUM_CHECK_TARGET") or str(Path.home() / ".cache" / "py2axum" / "check-target")
    r = subprocess.run(["cargo", "check", "--quiet", "--message-format=short"], cwd=out, capture_output=True, text=True,
                       env={**os.environ, "CARGO_TARGET_DIR": target, "RUSTFLAGS": "-D warnings"}, timeout=1800)
    assert r.returncode == 0, r.stderr[-4000:]
