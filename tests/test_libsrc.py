"""--translate-libs: libraries translated from their source (py2axum/libsrc.py), against fake environments."""
import ast
import base64
import hashlib
import sys

import pytest

from py2axum import libsrc
from py2axum.__main__ import main

PY = "python{}.{}".format(*sys.version_info[:2])


def make_dist(venv, name, version, files, licence="MIT", classifiers=(), top=None, record_extra=()):
    """A distribution installed in `venv` (dist-info with METADATA, RECORD and its hashes)."""
    site = venv / "lib" / PY / "site-packages"
    site.mkdir(parents=True, exist_ok=True)
    rows = []
    for rel, text in files.items():
        p = site / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)
        h = base64.urlsafe_b64encode(hashlib.sha256(text.encode()).digest()).rstrip(b"=").decode()
        rows.append(f"{rel},sha256={h},{len(text)}")
    info = site / f"{name.replace('-', '_')}-{version}.dist-info"
    info.mkdir()
    meta = ["Metadata-Version: 2.4", f"Name: {name}", f"Version: {version}"]
    if licence:
        meta.append(f"License-Expression: {licence}")
    meta += [f"Classifier: {c}" for c in classifiers]
    (info / "METADATA").write_text("\n".join(meta) + "\n")
    if top:
        (info / "top_level.txt").write_text(top + "\n")
    rows += list(record_extra) + [f"{info.name}/METADATA,,", f"{info.name}/RECORD,,"]
    (info / "RECORD").write_text("\n".join(rows) + "\n")
    return site


APP = """
from fastapi import FastAPI
from {imp}

app = FastAPI()


@app.get("/x/{{n}}")
async def x(n: int):
    return {{"v": {call}}}
"""


def project(tmp_path, imp="tinylib import double", call="double(n)"):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(APP.format(imp=imp, call=call))
    (tmp_path / ".git").mkdir()  # the project's root: no lock or .venv above it
    return pkg


TINY = {"tinylib/__init__.py": "from .core import double\n", "tinylib/core.py": "def double(x):\n    return x * 2\n"}


def gen(tmp_path, pkg, *flags):
    return main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out"), "--venv", str(tmp_path / ".venv"), *flags])


@pytest.fixture(autouse=True)
def _reset():
    yield
    libsrc.configure("none")


def test_translated_from_source(tmp_path):
    make_dist(tmp_path / ".venv", "tinylib", "1.0", TINY)
    pkg = project(tmp_path)
    assert gen(tmp_path, pkg, "--translate-libs", "auto") == 0
    rs = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "f_tinylib_core__double" in rs and "tinylib/core.py" in rs


def test_not_selected_says_how(tmp_path, capsys):
    make_dist(tmp_path / ".venv", "tinylib", "1.0", TINY)
    make_dist(tmp_path / ".venv", "other", "1.0", {"other.py": "X = 1\n"})
    pkg = project(tmp_path)
    assert gen(tmp_path, pkg, "--translate-libs", "other") == 1
    err = capsys.readouterr().err
    assert "tinylib 1.0 is installed: `--translate-libs auto` (or `--translate-libs tinylib`)" in err, err


@pytest.mark.parametrize("licence, classifiers, allow, code", [
    ("GPL-3.0-only", (), None, "P2A0701"),
    (None, (), None, "P2A0701"),
    (None, ("License :: OSI Approved :: GNU General Public License v2 or later (GPLv2+)",), None, "P2A0701"),
    ("GPL-3.0-only", (), "GPL-3.0-only", None),
    ("MIT OR GPL-3.0-only", (), None, None),
    ("MIT AND GPL-3.0-only", (), None, "P2A0701"),
    (None, ("License :: OSI Approved :: BSD License",), None, None),
])
def test_licence_gate(tmp_path, capsys, licence, classifiers, allow, code):
    make_dist(tmp_path / ".venv", "tinylib", "1.0", TINY, licence=licence, classifiers=classifiers)
    pkg = project(tmp_path)
    flags = ["--translate-libs", "auto"] + (["--allow-license", allow] if allow else [])
    rc = gen(tmp_path, pkg, *flags)
    err = capsys.readouterr().err
    if code is None:
        assert rc == 0, err
    else:
        assert rc == 1 and f"error[{code}]" in err and "not admitted for translation from source" in err, err
        assert "--allow-license" in err and "main.py:" in err


def test_compiled_extension_refused(tmp_path, capsys):
    make_dist(tmp_path / ".venv", "tinylib", "1.0", TINY, record_extra=["tinylib/_speedups.cpython-313-darwin.so,,"])
    pkg = project(tmp_path)
    assert gen(tmp_path, pkg, "--translate-libs", "auto") == 1
    err = capsys.readouterr().err
    assert "error[P2A0702]" in err and "is not pure Python (compiled extension tinylib/_speedups" in err, err


def test_modified_file_refused(tmp_path, capsys):
    site = make_dist(tmp_path / ".venv", "tinylib", "1.0", TINY)
    (site / "tinylib" / "core.py").write_text("def double(x):\n    return x * 3\n")
    pkg = project(tmp_path)
    assert gen(tmp_path, pkg, "--translate-libs", "auto") == 1
    err = capsys.readouterr().err
    assert "error[P2A0703]" in err and "tinylib/core.py does not match its RECORD hash" in err, err


def test_locked_version_must_be_installed(tmp_path, capsys):
    make_dist(tmp_path / ".venv", "tinylib", "1.0", TINY)
    pkg = project(tmp_path)
    (tmp_path / "uv.lock").write_text('version = 1\n\n[[package]]\nname = "tinylib"\nversion = "1.1"\n')
    assert gen(tmp_path, pkg, "--translate-libs", "auto") == 1
    err = capsys.readouterr().err
    assert "error[P2A0703]" in err and "the project locks 1.1 but 1.0 is installed" in err, err


def test_project_then_library_map_then_source(tmp_path, capsys):
    """dateutil.relativedelta stays native (library map) while dateutil.parser comes from the source; a module of
    the project wins over a library of the same name"""
    make_dist(tmp_path / ".venv", "python-dateutil", "2.9.0", {
        "dateutil/__init__.py": "",
        "dateutil/relativedelta.py": "def relativedelta(**kw):\n    raise RuntimeError('source read')\n",
        "dateutil/parser.py": "def twice(s):\n    return s + s\n"}, top="dateutil")
    make_dist(tmp_path / ".venv", "tinylib", "1.0", TINY)
    pkg = project(tmp_path, imp="dateutil.relativedelta import relativedelta\nfrom dateutil.parser import twice\n"
                               "from tinylib import double",
                  call="[str(relativedelta(days=n)), twice(str(n)), double(n)]")
    (tmp_path / "tinylib.py").write_text("def double(x):\n    return x - 1\n")
    assert gen(tmp_path, pkg, "--translate-libs", "auto") == 0, capsys.readouterr().err
    rs = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "f_dateutil_parser__twice" in rs and "source read" not in rs and "reldelta" in rs
    assert "f_tinylib__double" in rs and "f_tinylib_core__double" not in rs


def test_framework_never_from_source(tmp_path):
    ix = libsrc.LibIndex.__new__(libsrc.LibIndex)
    ix.native, ix.std = libsrc.native_tops(), libsrc.stdlib_names()
    for name in ("fastapi.routing", "starlette.responses", "pydantic.fields", "sqlalchemy.orm", "json", "yaml",
                 "dateutil.relativedelta", "dateutil"):
        assert ix.native_module(name), name
    for name in ("humanize", "humanize.number", "slugify.slugify", "dateutil.parser"):
        assert not ix.native_module(name), name


@pytest.mark.parametrize("expr, ok", [("MIT", True), ("Apache-2.0", True), ("(MIT OR Apache-2.0)", True),
                                      ("BSD-3-Clause AND MIT", True), ("GPL-2.0-or-later", False), ("", False),
                                      ("MIT AND (GPL-3.0-only OR ISC)", True), ("Apache-2.0 WITH LLVM-exception", False),
                                      ("LicenseRef-Proprietary", False), ("MIT OR", False)])
def test_licence_expressions(expr, ok):
    assert libsrc.licence_admitted(expr) is ok


def test_platform_folded():
    src = ("import sys, os\nif sys.platform == 'win32':\n    A = 1\nelse:\n    A = 2\n"
           "B = 3 if sys.platform.startswith('linux') or os.name == 'nt' else 4\n")
    tree = libsrc.fold_platform(ast.parse(src))
    out = ast.unparse(tree)
    assert "win32" not in out and "A = 2" in out
    assert ("B = 3" in out) == sys.platform.startswith("linux") or "or" in out


def test_cache_reused(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path / "cache"))
    site = make_dist(tmp_path / ".venv", "tinylib", "1.0", TINY)
    libsrc.configure("auto", tmp_path / ".venv")
    ix = libsrc.LibIndex(tmp_path)
    dist = ix.dists["tinylib"]
    t1, _ = libsrc.parse_cached(dist, site / "tinylib" / "core.py", (3, 13))
    files = list((tmp_path / "cache" / "py2axum" / "libs").rglob("*.pickle"))
    assert len(files) == 1 and dist.key in str(files[0])
    t2, _ = libsrc.parse_cached(dist, site / "tinylib" / "core.py", (3, 13))
    assert ast.dump(t1) == ast.dump(t2)


GUARDED = {
    "guarded/__init__.py": (
        "try:\n    import _guarded_accel_missing as accel\nexcept ImportError:\n    accel = None\n\n"
        "try:\n    import json as present\nexcept ImportError:\n    present = None\n\n\n"
        "def mode():\n    return 'fallback' if accel is None else 'accel'\n\n\n"
        "def has_json():\n    return present.dumps([1])\n"),
}


def test_import_guard_decided_by_the_environment(tmp_path):
    """`try: import x / except ImportError:` of a library: the handler when x is not installed, as at import"""
    make_dist(tmp_path / ".venv", "guarded", "1.0", GUARDED)
    pkg = project(tmp_path, imp="guarded import mode, has_json", call="[mode(), has_json(), n]")
    assert gen(tmp_path, pkg, "--translate-libs", "auto") == 0
    rs = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "_guarded_accel_missing" not in rs


LAZY = {
    "lazypkg/__init__.py": (
        "from importlib import import_module\n\n_import_map = {'Thing': ('lazypkg._thing', False), "
        "'sub': ('lazypkg.sub', True)}\n\n\ndef __getattr__(name):\n    try:\n        target, is_sub = _import_map[name]\n"
        "        module = import_module(target)\n        return module if is_sub else getattr(module, name)\n"
        "    except KeyError:\n        raise AttributeError(name) from None\n"),
    "lazypkg/_thing.py": "def Thing(x):\n    return x + 1\n",
    "lazypkg/sub.py": "def f(x):\n    return x * 3\n",
}


def test_module_getattr_lazy_map(tmp_path):
    make_dist(tmp_path / ".venv", "lazypkg", "1.0", LAZY)
    pkg = project(tmp_path, imp="lazypkg import Thing, sub", call="[Thing(n), sub.f(n)]")
    assert gen(tmp_path, pkg, "--translate-libs", "auto") == 0
    rs = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "f_lazypkg__thing__Thing" in rs and "f_lazypkg_sub__f" in rs


def test_licence_field_spdx_fallback(tmp_path):
    """the older `License` field is read when it holds an SPDX expression (pyasn1's `BSD-2-Clause`), never free
    text (`BSD 3-Clause OR Apache-2.0` is refused, uritemplate)"""
    from email.message import Message

    m = Message()
    m["License"] = "BSD-2-Clause"
    assert libsrc._licence(m) == ("BSD-2-Clause", "License")
    m = Message()
    m["License"] = "BSD 3-Clause OR Apache-2.0"
    assert libsrc._licence(m)[0] is None


def test_flags_folded():
    from py2axum.modules import fold_flags, fold_version_info

    src = ("import sys\nPY3 = sys.version_info[0] == 3\nif PY3:\n    a = 1\nelse:\n    a = undefined\n"
           "def f():\n    return 1 if not PY3 else 2\ndef g(PY3):\n    return 3 if PY3 else 4\n")
    tree, _ = fold_version_info(ast.parse(src), (3, 13))
    out = ast.unparse(fold_flags(tree))
    assert "undefined" not in out and "return 2" in out and "3 if PY3 else 4" in out


def test_stdlib_c_accelerator_never_from_source():
    """CPython runs `_bisect`, not Lib/bisect.py (their messages differ: "lo cannot be negative" in 3.15): a
    module of the standard library that imports a compiled one is refused, never translated from its Lib/ code"""
    import bisect
    import gettext
    import html.entities
    import keyword

    assert libsrc.c_accelerator(ast.parse(open(bisect.__file__).read())) == "_bisect"
    for m in (gettext, html.entities, keyword):
        assert libsrc.c_accelerator(ast.parse(open(m.__file__).read())) is None, m
    assert "bisect" not in libsrc.STDLIB_SOURCE


def test_bisect_stays_refused(tmp_path, capsys):
    pkg = project(tmp_path, imp="bisect import bisect_right", call="bisect_right([1, 2, 3], n)")
    assert gen(tmp_path, pkg, "--translate-libs", "auto") == 1
    assert "bisect_right" in capsys.readouterr().err
