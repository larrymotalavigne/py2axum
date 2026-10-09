"""Supported version ranges: one source (py2axum/versions.py), mirrored by pyproject.toml and docs."""
import re
import tomllib
from pathlib import Path

import pytest

from py2axum import versions
from py2axum.versions import check_project, interval, parse

ROOT = Path(__file__).resolve().parent.parent


def test_pyproject_mirrors_supported():
    pp = tomllib.loads((ROOT / "pyproject.toml").read_text())
    extra = {}
    for req in pp["project"]["optional-dependencies"]["conformance"]:
        m = re.match(r"([\w-]+)(?:\[[^\]]*\])?(.*)", req)
        extra[m.group(1).lower()] = m.group(2)
    for name in versions.SUPPORTED:
        assert extra.get(name) == versions.spec(name), name
    assert pp["project"]["requires-python"] == ">=3.12"
    classifiers = {c.rsplit(" ", 1)[1] for c in pp["project"]["classifiers"] if c.startswith("Programming Language :: Python :: 3.")}
    assert classifiers == {"3.12", "3.13", "3.14"}


def test_docs_list_every_range():
    doc = (ROOT / "docs" / "reference" / "versions.md").read_text()
    for name in versions.SUPPORTED:
        lo, hi, _ = versions.SUPPORTED[name]
        assert f"| {name} | {lo} | {hi} | `{versions.spec(name)}` |" in doc, name


def test_ci_matrix_runs_both_ends():
    if not (ROOT / ".gitlab-ci.yml").exists():
        pytest.skip("the version matrix runs in the internal CI, not exported")
    ci = (ROOT / ".gitlab-ci.yml").read_text()
    assert "python -m py2axum.versions $END" in ci
    assert 'PY: "3.12"' in ci and 'PY: "3.14"' in ci
    assert versions.pins("min")[0] == "fastapi==0.137.0" and "psycopg[binary]==3.3.6" in versions.pins("max")
    for end in versions.MIDDLES:
        assert "END: " + end + " }" in ci  # each intermediate version has its job
        assert {f"{n}=={v}" for n, v in versions.MIDDLES[end].items()} <= set(versions.pins(end))


def test_interval():
    assert interval(">=2.9,<3") == (parse("2.9"), parse("3"))
    assert interval("==2.13.*") == (parse("2.13"), parse("2.14"))
    assert interval("~=2.12") == (parse("2.12"), parse("3"))
    assert interval("~=2.12.1") == (parse("2.12.1"), parse("2.13"))
    lo, hi = interval("==0.115.0")
    assert lo <= parse("0.115.0") < hi and parse("0.115.1") >= hi
    assert parse("2.0.0rc1") < parse("2.0.0")


def write(tmp_path, name, text):
    (tmp_path / name).write_text(text)
    return tmp_path


def test_requirements_pin(tmp_path):
    write(tmp_path, "requirements.txt", "# api\nfastapi==0.142.2\npydantic[email]==2.8.2  # old\nrequests==1.0\n")
    [(msg, file, line)] = check_project(tmp_path)
    assert file.endswith("requirements.txt") and line == 3 and msg.startswith("pydantic 2.8.2 is outside")


def test_pyproject_specifiers(tmp_path):
    write(tmp_path, "pyproject.toml", '[project]\nname = "x"\nrequires-python = ">=3.12"\n'
                                      'dependencies = [\n  "fastapi>=0.100",\n  "sqlalchemy<2",\n]\n')
    [(msg, file, line)] = check_project(tmp_path)  # fastapi>=0.100 overlaps the range: accepted
    assert line == 6 and msg.startswith("sqlalchemy<2 excludes every version")


def test_requires_python(tmp_path):
    write(tmp_path, "pyproject.toml", '[project]\nname = "x"\nrequires-python = "==3.11.*"\n')
    [(msg, _, line)] = check_project(tmp_path)
    assert line == 3 and "excludes the Pythons" in msg
    write(tmp_path, "pyproject.toml", '[project]\nname = "x"\nrequires-python = ">=3.99"\n')
    [(msg, _, _)] = check_project(tmp_path)
    assert "excludes the Pythons" in msg


def test_lock_wins_and_in_range(tmp_path):
    write(tmp_path, "pyproject.toml", '[project]\nname = "x"\ndependencies = ["sqlalchemy<2"]\n')
    write(tmp_path, "uv.lock", 'requires-python = ">=3.12"\n\n[[package]]\nname = "sqlalchemy"\nversion = "2.0.54"\n')
    assert check_project(tmp_path) == []


def test_lock_above_wins_over_subdirectory_requirements(tmp_path):
    """`uv sync --frozen` installs the lock of the repository: a stale back/requirements.txt does not count."""
    write(tmp_path, "uv.lock", '[[package]]\nname = "fastapi"\nversion = "0.142.2"\n')
    (tmp_path / "back").mkdir()
    write(tmp_path / "back", "requirements.txt", "fastapi==0.136.1\n")
    assert check_project(tmp_path / "back") == []
    (tmp_path / "uv.lock").unlink()
    [(msg, file, _)] = check_project(tmp_path / "back")
    assert file.endswith("requirements.txt") and msg.startswith("fastapi 0.136.1 is outside")


def test_only_used_libraries(tmp_path):
    """A lock lists libraries the analysed package never imports (other services, dependencies): not checked."""
    write(tmp_path, "uv.lock", '[[package]]\nname = "mcp"\nversion = "1.26.0"\n\n'
                               '[[package]]\nname = "aiohttp"\nversion = "3.9.0"\n')
    pkg = tmp_path / "api"
    pkg.mkdir()
    (pkg / "main.py").write_text("from fastapi import FastAPI\nimport aiohttp\n")
    [(msg, _, line)] = check_project(tmp_path, pkg)
    assert line == 7 and msg.startswith("aiohttp 3.9.0")
    assert len(check_project(tmp_path)) == 2


def test_feature_needs_newer_version(tmp_path):
    """WebSocket routes reproduce Starlette 1.7: an older locked Starlette is refused for them only."""
    write(tmp_path, "uv.lock", '[[package]]\nname = "starlette"\nversion = "1.6.0"\n')
    pkg = tmp_path / "api"
    pkg.mkdir()
    (pkg / "main.py").write_text("from fastapi import FastAPI\napp = FastAPI()\n")
    assert check_project(tmp_path, pkg) == []
    (pkg / "ws.py").write_text("from .main import app\n\n@app.websocket('/ws')\nasync def ws(websocket):\n    pass\n")
    [(msg, _, line)] = check_project(tmp_path, pkg)
    assert line == 3 and msg.startswith("starlette 1.6.0: WebSocket routes") and "starlette>=1.7.0" in msg
    # the lowest version itself is enough, pinned (an exact pin is a one-version interval) or as a lower bound
    for pin in ("==1.7.0", ">=1.7.0"):
        (tmp_path / "uv.lock").unlink(missing_ok=True)
        write(tmp_path, "requirements.txt", f"starlette{pin}\n")
        assert check_project(tmp_path, pkg) == []


def test_locked_version_reads_pins_and_specifiers(tmp_path):
    """The binary reproduces the pydantic minor of the project: its lock, else its `==` pin, else the installed
    one when the project's specifier allows it (else the highest tested version the specifier allows)."""
    from importlib.metadata import version as installed

    from py2axum.dyn import locked_version
    (tmp_path / ".git").mkdir()
    write(tmp_path, "requirements.txt", "pydantic[email]==2.13.5\n")
    assert locked_version(tmp_path, "pydantic") == "2.13.5"
    write(tmp_path, "requirements.txt", "pydantic>=2.12,<2.14\n")
    mine = installed("pydantic")
    assert locked_version(tmp_path, "pydantic") == (mine if parse(mine) < parse("2.14") else "2.13.5")
    write(tmp_path, "requirements.txt", "pydantic>=2.14\n")
    assert locked_version(tmp_path, "pydantic") == (mine if parse(mine) >= parse("2.14") else "2.14.0")
    write(tmp_path, "uv.lock", '[[package]]\nname = "pydantic"\nversion = "2.12.5"\n')
    assert locked_version(tmp_path, "pydantic") == "2.12.5"
