"""`py2axum check`: per-route status without generating, exit status for CI, refusal of untested versions."""
import json

from py2axum.__main__ import main
from tests.test_report import write_project

BLOCKED = {"/api/items/stamp/now", "/api/items/me/info", "/api/items/me/again", "/api/items/me/stamp"}
NATIVE = {"/api/items/{item_id}", "/users/count"}


def run_json(capsys, *args):
    rc = main(["check", *args, "--json"])
    return rc, json.loads(capsys.readouterr().out)


def test_text_output(tmp_path, capsys):
    pkg = write_project(tmp_path)
    assert main(["check", str(pkg), "--root", str(tmp_path)]) == 1  # refused routes: generation would fail
    out = capsys.readouterr().out
    assert "native       GET /users/count" in out
    assert "refused      GET /api/items/me/info" in out
    # the reason, at file:line of the construct (not of the route)
    assert "proj/deps.py:5: library call `jwt.decode()` is not supported" in out
    assert "library jwt" in out and "type SecretStr" in out  # blockers summary
    assert "2/6 routes native (33.3 %), 0 python-side, 4 refused — generation would fail" in out
    assert "hint: --python-side auto" in out
    assert "\x1b[" not in out  # no colour when not a terminal


def test_json_and_auto(tmp_path, capsys):
    pkg = write_project(tmp_path)
    rc, data = run_json(capsys, str(pkg), "--root", str(tmp_path))
    assert rc == 1
    status = {r["path"]: r["status"] for r in data["routes"]}
    assert {p for p, s in status.items() if s == "refused"} == BLOCKED
    assert {p for p, s in status.items() if s == "native"} == NATIVE
    info = next(r for r in data["routes"] if r["path"] == "/api/items/me/info")
    assert info["blockers"] == {"library jwt": [f"{pkg}/deps.py:5"]}
    assert info["where"] == f"{pkg}/views/items.py:{info['where'].rsplit(':', 1)[1]}"
    assert {b["construction"]: (b["routes"], b["only_blocker"]) for b in data["blockers"]} == {
        "library jwt": (2, 2), "type SecretStr": (2, 2)}
    assert data["summary"] == {"total": 6, "native": 2, "python-side": 0, "refused": 4, "native_pct": 33.3,
                               "generates": False}
    # --python-side auto: the same routes stay in Python, generation succeeds
    rc, data = run_json(capsys, str(pkg), "--root", str(tmp_path), "--python-side", "auto")
    assert rc == 0
    assert {r["path"] for r in data["routes"] if r["status"] == "python-side"} == BLOCKED
    assert data["summary"]["generates"] is True


def test_fail_under(tmp_path, capsys):
    pkg = write_project(tmp_path)
    args = [str(pkg), "--root", str(tmp_path), "--python-side", "auto"]
    assert main(["check", *args, "--fail-under", "30"]) == 0
    assert main(["check", *args, "--fail-under", "50"]) == 1
    assert "native routes 33.3 % < --fail-under 50 %" in capsys.readouterr().out


def test_declared_python_side(tmp_path, capsys):
    pkg = write_project(tmp_path)
    rc, data = run_json(capsys, str(pkg), "--root", str(tmp_path), "--backend", "dyn",
                        "--python-side", "/api/items/me/info")
    routes = {r["path"]: r for r in data["routes"]}
    assert routes["/api/items/me/info"]["status"] == "python-side"
    assert routes["/api/items/me/info"]["reason"] == "declared --python-side"
    assert routes["/api/items/me/again"]["status"] == "refused"
    assert rc == 1


def test_global_error(tmp_path, capsys):
    """An error about the whole application refuses generation, even with --python-side auto."""
    pkg = write_project(tmp_path)
    main_py = pkg / "main.py"
    main_py.write_text(main_py.read_text().replace("app = FastAPI()", "app = FastAPI(dependencies=[])"))
    rc, data = run_json(capsys, str(pkg), "--root", str(tmp_path), "--python-side", "auto")
    assert rc == 1
    assert any("main.py" in g["error"] and "FastAPI(dependencies=...)" in g["error"] for g in data["global"])
    assert not any(r["status"] == "python-side" for r in data["routes"])


def test_typed_backend_covers_everything(tmp_path, capsys):
    pkg = write_project(tmp_path)
    for name in ("deps.py", "views/items.py", "schemas.py"):
        (pkg / name).unlink()
    main_py = pkg / "main.py"
    main_py.write_text(main_py.read_text().replace("from proj.views import items\n", "")
                       .replace('    app.include_router(items.router, prefix="/api")\n', ""))
    rc, data = run_json(capsys, str(pkg), "--root", str(tmp_path))
    assert rc == 0 and data["backend"] == "typed"
    assert [(r["path"], r["status"]) for r in data["routes"]] == [("/users/count", "native")]


def test_untested_version_refused(tmp_path, capsys):
    """A locked version outside the tested ranges stops check, generation and is listed by --report."""
    pkg = write_project(tmp_path)
    (tmp_path / "uv.lock").write_text('version = 1\nrequires-python = ">=3.12"\n\n'
                                      '[[package]]\nname = "fastapi"\nversion = "0.99.1"\n')
    rc, data = run_json(capsys, str(pkg), "--root", str(tmp_path), "--python-side", "auto")
    assert rc == 1
    [g] = data["global"]
    assert g["where"] == f"{tmp_path}/uv.lock:6"
    assert "fastapi 0.99.1 is outside the range" in g["error"] and "--allow-untested-versions" in g["error"]
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    assert f"error: {tmp_path}/uv.lock:6: fastapi 0.99.1" in capsys.readouterr().err
    assert not (tmp_path / "out").exists()
    rc, data = run_json(capsys, str(pkg), "--root", str(tmp_path), "--python-side", "auto",
                        "--allow-untested-versions")
    assert rc == 0 and data["global"] == []
    assert main([str(pkg), "--root", str(tmp_path), "--report", str(tmp_path / "r.md")]) == 0
    report = json.loads((tmp_path / "r.json").read_text())
    assert any("fastapi 0.99.1" in g["erreur"] for g in report["bloquants_globaux"])


def test_unmapped_library_setting(tmp_path, capsys):
    """`stripe.api_key = ...` at module level: the library is unknown to the binary, so every read or call of
    it is refused already; the setting itself does not block the application (it stays Python's)."""
    pkg = write_project(tmp_path)
    (pkg / "pay.py").write_text("import os\n\nimport stripe\n\nstripe.api_key = os.getenv('STRIPE_KEY')\n\n\n"
                                "def customers():\n    return stripe.Customer.list(limit=1)\n")
    main_py = pkg / "main.py"
    main_py.write_text(main_py.read_text().replace(
        "app = create_app()\n",
        "app = create_app()\n\n\n@app.get('/pay')\ndef pay():\n    from proj.pay import customers\n    return customers()\n"))
    rc, data = run_json(capsys, str(pkg), "--root", str(tmp_path), "--backend", "dyn", "--python-side", "auto")
    assert rc == 0 and not data["global"]
    pay = next(r for r in data["routes"] if r["path"] == "/pay")
    assert pay["status"] == "python-side" and "stripe.Customer.list()" in pay["reason"]
    # a library the map does know (json) is still refused
    (pkg / "pay.py").write_text("import json\n\njson.encoder = None\n\n\ndef customers():\n    return 1\n")
    rc, data = run_json(capsys, str(pkg), "--root", str(tmp_path), "--backend", "dyn", "--python-side", "auto")
    assert rc == 1 and any("json.encoder" in g["error"] for g in data["global"])
