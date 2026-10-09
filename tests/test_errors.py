"""Error codes and the format of refusals: every message of the compiler has a stable code, the command line
prints what, where, why and what to do, and the reference page of the site is generated from the table."""
import ast
import json
import re
from pathlib import Path

import pytest

from py2axum import errors
from py2axum.__main__ import main
from py2axum.ir import TranspileError
from tests.test_report import write_project

ROOT = Path(__file__).resolve().parents[1]


def literal_messages():
    """Every message literal passed to TranspileError(...) or an err(...) helper in py2axum/, f-string fields
    replaced by `X` (the helpers of ddl.py and jsonschema.py prefix their messages)."""
    prefix = {"ddl.py": "create_all: ", "jsonschema.py": "JSON schema: "}
    out = []
    for f in sorted((ROOT / "py2axum").glob("*.py")):
        tree = ast.parse(f.read_text())
        for node in ast.walk(tree):
            if not isinstance(node, ast.Call) or not node.args:
                continue
            name = node.func.attr if isinstance(node.func, ast.Attribute) else getattr(node.func, "id", "")
            if name not in {"TranspileError", "err"}:
                continue
            arg = node.args[0]
            if isinstance(arg, ast.Constant) and isinstance(arg.value, str):
                text = arg.value
            elif isinstance(arg, ast.JoinedStr):
                text = "".join(v.value if isinstance(v, ast.Constant) else "X" for v in arg.values)
            else:
                continue
            if not re.sub(r"[X\W]", "", text):  # only placeholders: the message is computed elsewhere
                continue
            out.append((f"{f.name}:{node.lineno}", prefix.get(f.name, "") + text))
    return out


def test_codes_are_well_formed_and_unique():
    codes = [c.code for c in errors.CLASSES]
    assert len(codes) == len(set(codes))
    for c in errors.CLASSES:
        assert re.fullmatch(r"P2A0[1-69]\d\d", c.code), c.code
        assert c.why.endswith(".") and c.fix.endswith((".", ")")), c.code
        assert (ROOT / "docs" / c.see.split("#")[0]).exists(), c.see
    assert errors.CLASSES[-1] is errors.OTHER


def test_every_message_has_a_class():
    """A new refusal gets a code: add a pattern to py2axum/errors.py (or pass code= at the call site)."""
    msgs = literal_messages()
    assert len(msgs) > 300
    unclassified = [f"{where}: {m}" for where, m in msgs if errors.lookup(m)[0] is errors.OTHER]
    assert len(unclassified) <= 3, "\n".join(unclassified)


@pytest.mark.parametrize("msg, code", [
    ("library call `boto3.client()` is not supported (not in the py2axum library map)", "P2A0201"),
    ("method .frobnicate() is not implemented by the runtime", "P2A0203"),
    ("relationship Team.owner: lazy='dynamic' is not supported", "P2A0504"),
    ("column price: unsupported column type `Money`", "P2A0501"),
    ("schema Item.v: unrecognized field validator signature", "P2A0403"),
    ("schema Item: method frob is not supported", "P2A0406"),
    ("`nonlocal` is not supported", "P2A0102"),
    ("middleware SessionMiddleware is not supported (only GZipMiddleware)", "P2A0302"),
    ("create_all: a Sequence (CREATE SEQUENCE) is not supported", "P2A0507"),
    ("unsupported type annotation `Foo`", "P2A0401"),
    ("something nobody wrote yet", "P2A0999"),
])
def test_lookup(msg, code):
    assert TranspileError(msg).code == code


def test_explicit_code_wins():
    e = TranspileError("library call `x.y()` is not supported", code="P2A0901")
    assert e.code == "P2A0901"


def test_explain_format():
    e = TranspileError("library call `boto3.client()` is not supported", ast.parse("x", mode="eval").body,
                       "app/deps.py")
    lines = e.explain().splitlines()
    assert lines[0] == "error[P2A0201]: app/deps.py:1: library call `boto3.client()` is not supported"
    assert lines[1].startswith("  = why: ") and lines[2].startswith("  = help: ")
    assert lines[3] == "  = see: https://larrymotalavigne.github.io/py2axum/reference/errors/#p2a0201"
    assert e.render() == "app/deps.py:1: library call `boto3.client()` is not supported"  # one-line form unchanged


def test_generation_prints_code_why_and_help(tmp_path, capsys):
    pkg = write_project(tmp_path)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert re.search(r"^error\[P2A0\d{3}\]: .*proj/\w+\.py:\d+: ", err, re.M), err
    assert "  = why: " in err and "  = help: " in err and "/reference/errors/#p2a0" in err
    assert "py2axum check" in err  # points to the command that lists every refusal


def test_check_codes_suggestions_and_unblock(tmp_path, capsys):
    pkg = write_project(tmp_path)
    main(["check", str(pkg), "--root", str(tmp_path), "--format", "json"])
    data = json.loads(capsys.readouterr().out)
    info = next(r for r in data["routes"] if r["path"] == "/api/items/me/info")
    assert info["code"] == "P2A0201"
    assert "--python-side '/api/items/me/info'" in info["suggestion"]
    assert [u["new_native"] for u in data["unblock"]] == [2, 2]
    assert data["unblock"][-1]["native_after"] == 6
    assert {u["code"] for u in data["unblock"]} == {"P2A0201", "P2A0401"}
    main(["check", str(pkg), "--root", str(tmp_path)])
    out = capsys.readouterr().out
    assert "help: " in out and "What would make the most routes native" in out
    assert "py2axum check --explain P2A0" in out


def test_check_markdown(tmp_path, capsys):
    pkg = write_project(tmp_path)
    main(["check", str(pkg), "--root", str(tmp_path), "--format", "markdown"])
    out = capsys.readouterr().out
    assert out.startswith("## py2axum check: ")
    assert "| ❌ refused | `GET /api/items/me/info` |" in out
    assert "[P2A0201](https://larrymotalavigne.github.io/py2axum/reference/errors/#p2a0201)" in out
    assert "### What would make the most routes native" in out


def test_check_explain(capsys):
    assert main(["check", "--explain", "p2a0504"]) == 0
    out = capsys.readouterr().out
    assert out.startswith("P2A0504: Relationship\n") and "What to do:" in out
    assert main(["check", "--explain", "P2A9999"]) == 2


def test_reference_page_is_generated():
    """docs/reference/errors.md is `python -m py2axum.errors --markdown`: regenerate it after editing errors.py."""
    page = (ROOT / "docs" / "reference" / "errors.md").read_text()
    assert page == errors.markdown()
    assert "reference/errors.md" in (ROOT / "mkdocs.yml").read_text()
