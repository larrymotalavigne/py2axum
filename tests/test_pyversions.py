"""The project's Python (docs/python-versions.md): how py2axum finds it, `sys.version_info` folded against it,
and the constructs of newer Pythons refused with file:line."""
import ast
import sys
import textwrap

import pytest

from py2axum.__main__ import main
from py2axum.modules import fold_version_info
from py2axum.versions import PROBES, probe_features, target_python, target_python_patch


def fold(src: str, py: tuple[int, int]) -> str:
    tree, _ = fold_version_info(ast.parse(textwrap.dedent(src)), py)
    return ast.unparse(tree)


@pytest.mark.parametrize("expr, py, out", [
    ("sys.version_info >= (3, 14)", (3, 14), "True"),
    ("sys.version_info >= (3, 14)", (3, 13), "False"),
    ("sys.version_info < (3, 13)", (3, 12), "True"),
    ("sys.version_info > (3, 14)", (3, 14), "True"),   # (3, 14, x, ...) > (3, 14)
    ("sys.version_info == (3, 14)", (3, 14), "False"),
    ("sys.version_info >= (3,)", (3, 12), "True"),
    ("sys.version_info >= (3, 13, 2)", (3, 14), "True"),  # decided by the minor
    ("(3, 15) <= sys.version_info", (3, 15), "True"),
    ("sys.version_info.minor", (3, 15), "15"),
    ("sys.version_info[0]", (3, 15), "3"),
    ("sys.version_info[:2]", (3, 13), "(3, 13)"),
    ("not sys.version_info >= (3, 14)", (3, 14), "False"),
    # depends on the patch level: left as written (refused where it is used)
    ("sys.version_info >= (3, 14, 2)", (3, 14), "sys.version_info >= (3, 14, 2)"),
    ("sys.version_info.micro", (3, 14), "sys.version_info.micro"),
    ("sys.version_info", (3, 14), "sys.version_info"),
])
def test_fold_expressions(expr, py, out):
    assert fold(f"import sys\nx = {expr}", py).splitlines()[-1] == f"x = {out}"


def test_fold_keeps_the_branch_taken_and_aliases():
    src = """
    import sys as s
    from sys import version_info as vi
    if s.version_info >= (3, 15):
        from . import new
    elif vi < (3, 13):
        import old
    else:
        import mid
    y = 1 if vi >= (3, 14) else 2
    def f():
        if s.version_info < (3, 14):
            return 1
    """
    assert [l for l in fold(src, (3, 14)).splitlines()[2:] if l] == ["import mid", "y = 1", "def f():", "    pass"]
    assert "from . import new" in fold(src, (3, 15))
    assert "import old" in fold(src, (3, 12))


def test_literal_if_true_is_left_alone():
    assert "if True:" in fold("import sys\nif True:\n    x = sys.version_info.minor", (3, 14))


def test_target_python(tmp_path, monkeypatch):
    monkeypatch.delenv("PY2AXUM_TARGET_PYTHON", raising=False)
    (tmp_path / ".git").mkdir()
    proj = tmp_path / "app"
    proj.mkdir()
    mine = tuple(sys.version_info[:2])
    assert target_python(proj) == mine  # nothing declared: the transpiler's own
    (tmp_path / "pyproject.toml").write_text('[project]\nname = "x"\nrequires-python = ">=3.12"\n')
    assert target_python(proj) == mine  # allowed by requires-python: a Docker build transpiles on the image's Python
    (tmp_path / "pyproject.toml").write_text('[project]\nname = "x"\nrequires-python = ">=3.99"\n')
    assert target_python(proj) == (3, 99)  # not allowed: its lowest
    (tmp_path / ".python-version").write_text("3.13.7\n")
    assert target_python(proj) == (3, 13)
    monkeypatch.setenv("PY2AXUM_TARGET_PYTHON", "3.12")
    assert target_python(proj) == (3, 12)


def test_python_flag_outside_the_supported_range(tmp_path, capsys, monkeypatch):
    monkeypatch.setenv("PY2AXUM_TARGET_PYTHON", "3.12")  # restored after the test (--python sets it)
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "main.py").write_text("from fastapi import FastAPI\napp = FastAPI()\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out"), "--python", "3.11"]) == 1
    assert "Python 3.11 is outside the Pythons py2axum supports" in capsys.readouterr().err


def test_gated_module_of_a_newer_syntax_is_never_parsed(tmp_path, capsys, monkeypatch):
    """A module imported under `sys.version_info >= (3, 99)`: no syntax error, no route, for an older project."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(textwrap.dedent("""
        import sys
        from fastapi import FastAPI

        app = FastAPI()

        if sys.version_info >= (3, 99):
            from . import future

            app.include_router(future.router)


        @app.get("/v")
        async def v():
            return {"minor": sys.version_info.minor, "new": sys.version_info >= (3, 14)}
    """))
    (pkg / "future.py").write_text("this is not python 3.99 syntax either ::\n")
    monkeypatch.delenv("PY2AXUM_TARGET_PYTHON", raising=False)
    assert main(["check", str(pkg), "--root", str(tmp_path)]) == 0
    out = capsys.readouterr().out
    assert "syntax error" not in out and "1/1 routes native" in out, out


def test_target_python_patch(tmp_path, monkeypatch):
    """The patch release (behaviours that changed inside a minor, `strptime('%e')` from 3.13.6)."""
    monkeypatch.delenv("PY2AXUM_TARGET_PYTHON", raising=False)
    (tmp_path / ".git").mkdir()
    assert target_python_patch(tmp_path) == sys.version_info[2]  # the transpiler's own
    (tmp_path / ".python-version").write_text("3.13.5\n")
    assert target_python_patch(tmp_path) == 5
    (tmp_path / ".python-version").write_text("3.13\n")
    assert target_python_patch(tmp_path) is None  # unknown: the newest
    monkeypatch.setenv("PY2AXUM_TARGET_PYTHON", "3.12.4")
    assert (target_python(tmp_path), target_python_patch(tmp_path)) == ((3, 12), 4)


def test_probed_features(tmp_path, monkeypatch):
    """Behaviours that changed inside a minor are probed on the transpiler's Python when it is the project's
    (a distribution backports security fixes: Debian's 3.13.5 has 3.13.13's base64 padding)."""
    monkeypatch.delenv("PY2AXUM_TARGET_PYTHON", raising=False)
    (tmp_path / ".git").mkdir()
    got = probe_features(tmp_path)
    assert set(got) == set(PROBES) and all(isinstance(v, bool) for v in got.values())
    if sys.version_info >= (3, 15):
        assert all(got.values())
    (tmp_path / ".python-version").write_text(f"{sys.version_info[0]}.{sys.version_info[1]}.0\n")
    assert probe_features(tmp_path) == {}  # a patch release named: the version decides
    monkeypatch.setenv("PY2AXUM_TARGET_PYTHON", "3.12" if sys.version_info[:2] != (3, 12) else "3.13")
    assert probe_features(tmp_path) == {}  # another minor: nothing to probe

