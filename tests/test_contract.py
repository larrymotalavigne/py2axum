"""The public contract frozen at 0.8 (docs/reference/versioning.md): the command lines, the binary's options and
environment variables, the error codes and the JSON of `check`. Any change makes a test fail: a removal or a
rename is a breaking change (a major version from 1.0, after a deprecation), an addition a minor one. Once the
change is intended, regenerate the snapshot with `UPDATE_CONTRACT=1 python -m pytest tests/test_contract.py`
and say it in CHANGELOG.md; never reuse a retired error code."""
import json
import os
import re
from pathlib import Path

import pytest

from py2axum import errors
from py2axum.__main__ import main
from tests.test_report import write_project

ROOT = Path(__file__).resolve().parents[1]
SNAPSHOT = ROOT / "tests" / "contract" / "public.json"
SCHEMA = ROOT / "docs" / "reference" / "check-schema.json"
# read by the runtime but not configuration of py2axum: the stand-in of socket.getfqdn() in Message-ID
NOT_CONFIG = {"HOSTNAME"}


def _arguments(ap) -> list[dict]:
    out = []
    for a in ap._actions:
        if type(a).__name__ == "_HelpAction":
            continue
        default = a.default
        if default is not None and not isinstance(default, (bool, int, float, str, list)):
            default = str(default)
        out.append({"flags": list(a.option_strings) or [a.dest], "action": type(a).__name__.strip("_"),
                    "nargs": a.nargs, "choices": list(a.choices) if a.choices else None, "default": default,
                    "metavar": a.metavar, "required": a.required})
    return out


def binary_options() -> list[str]:
    """The command-line options of the generated binary (uvicorn's, parsed by dynrt/uvicorn.rs)."""
    src = (ROOT / "py2axum/runtime/dynrt/uvicorn.rs").read_text()
    body = src[src.index("match name.as_str() {"):src.index("s if s.starts_with('-')")]
    return sorted(set(re.findall(r'"(--[a-z-]+)"\s*(?:=>|\|)', body)))


def binary_commands() -> list[str]:
    """The generated binary's subcommands, as their usage line states them (dynrt/migrate.rs)."""
    src = (ROOT / "py2axum/runtime/dynrt/migrate.rs").read_text()
    return re.search(r'"usage: (.+?)"', src).group(1).split(" | ")


def environment() -> list[str]:
    """Every environment variable py2axum reads: PY2AXUM_* anywhere, and the unprefixed ones the binary reads."""
    names = set()
    for f in (ROOT / "py2axum").rglob("*"):
        if f.suffix in {".py", ".rs"}:
            text = f.read_text()
            names |= set(re.findall(r"\bPY2AXUM_[A-Z0-9_]*[A-Z0-9]\b", text))
            if f.suffix == ".rs" or f.name == "dyn.py":
                names |= set(re.findall(r'\b(?:env::var(?:_os)?|env_or|env|flag_env)\("([A-Z][A-Z0-9_]+)"', text))
    return sorted(names - NOT_CONFIG)


def current() -> dict:
    from py2axum.__main__ import parser as generate
    from py2axum.check import parser as check
    from py2axum.watch import parser as watch

    return {
        "cli": {"py2axum": _arguments(generate()), "py2axum check": _arguments(check()),
                "py2axum watch": _arguments(watch())},
        "binary_options": binary_options(),
        "binary_commands": binary_commands(),
        "environment": environment(),
        "error_codes": sorted(c.code for c in errors.CLASSES),
    }


def snapshot() -> dict:
    return json.loads(SNAPSHOT.read_text())


def test_update_snapshot_when_asked():
    if not os.environ.get("UPDATE_CONTRACT"):
        pytest.skip("UPDATE_CONTRACT=1 rewrites tests/contract/public.json")
    old = snapshot() if SNAPSHOT.exists() else {"retired_error_codes": []}
    new = current()
    gone = sorted(set(old.get("error_codes", [])) - set(new["error_codes"]))
    new["retired_error_codes"] = sorted(set(old.get("retired_error_codes", [])) | set(gone))
    SNAPSHOT.parent.mkdir(exist_ok=True)
    SNAPSHOT.write_text(json.dumps(new, indent=1, ensure_ascii=False) + "\n")


MSG = ("public contract changed (docs/reference/versioning.md): if intended, UPDATE_CONTRACT=1 python -m pytest "
       "tests/test_contract.py, and a CHANGELOG entry; a removal or a rename needs a deprecation first")


@pytest.mark.parametrize("command", ["py2axum", "py2axum check", "py2axum watch"])
def test_command_lines_are_frozen(command):
    assert current()["cli"][command] == snapshot()["cli"][command], MSG


def test_binary_options_are_frozen():
    assert binary_options() == snapshot()["binary_options"], MSG


def test_binary_commands_are_frozen_and_documented():
    assert binary_commands() == snapshot()["binary_commands"], MSG
    docs = (ROOT / "docs/reference/cli.md").read_text()
    assert all(f"`{c}`" in docs for c in binary_commands()), "undocumented binary command (docs/reference/cli.md)"


def test_environment_is_frozen_and_documented():
    names = environment()
    assert names == snapshot()["environment"], MSG
    docs = (ROOT / "docs/reference/environment.md").read_text() + (ROOT / "docs/reference/cli.md").read_text()
    missing = [n for n in names if f"`{n}`" not in docs and not (n.startswith("UVICORN_") and "`UVICORN_*`" in docs)]
    assert not missing, f"undocumented environment variables (docs/reference/environment.md): {missing}"


def test_error_codes_are_frozen_never_reused():
    snap = snapshot()
    codes = sorted(c.code for c in errors.CLASSES)
    assert not set(codes) & set(snap["retired_error_codes"]), "a retired error code is never reused"
    assert codes == snap["error_codes"], MSG


# ---------------------------------------------------------------- the JSON of `check`, against its schema

def validate(value, schema, where="$"):
    """The subset of JSON Schema the published schema uses (type, const, enum, pattern, properties, required,
    additionalProperties, items): enough to fail on a key added, removed or retyped."""
    types = schema.get("type")
    if types is not None:
        types = [types] if isinstance(types, str) else types
        py = {"object": dict, "array": list, "string": str, "boolean": bool, "null": type(None),
              "integer": int, "number": (int, float)}
        ok = any(isinstance(value, py[t]) and not (t in {"integer", "number"} and isinstance(value, bool))
                 for t in types)
        assert ok, f"{where}: {value!r} is not {types}"
    if "const" in schema:
        assert value == schema["const"], f"{where}: {value!r} != {schema['const']!r}"
    if "enum" in schema:
        assert value in schema["enum"], f"{where}: {value!r} not in {schema['enum']}"
    if "pattern" in schema and isinstance(value, str):
        assert re.search(schema["pattern"], value), f"{where}: {value!r} !~ {schema['pattern']}"
    if isinstance(value, dict):
        props = schema.get("properties", {})
        for k in schema.get("required", []):
            assert k in value, f"{where}: missing key {k!r}"
        for k, v in value.items():
            if k in props:
                validate(v, props[k], f"{where}.{k}")
            else:
                extra = schema.get("additionalProperties", True)
                assert extra is not False, f"{where}: key {k!r} not in the published schema"
                if isinstance(extra, dict):
                    validate(v, extra, f"{where}.{k}")
    if isinstance(value, list) and "items" in schema:
        for i, v in enumerate(value):
            validate(v, schema["items"], f"{where}[{i}]")


def check_json(capsys, *args) -> dict:
    main(["check", *args, "--format", "json"])
    return json.loads(capsys.readouterr().out)


def test_check_json_matches_published_schema(tmp_path, capsys):
    schema = json.loads(SCHEMA.read_text())
    pkg = write_project(tmp_path)
    outputs = [check_json(capsys, str(pkg), "--root", str(tmp_path)),
               check_json(capsys, str(pkg), "--root", str(tmp_path), "--python-side", "auto")]
    for data in outputs:
        validate(data, schema)
    seen = {r["status"] for d in outputs for r in d["routes"]}
    assert seen == {"native", "python-side", "refused"}  # every kind of route went through the schema
    assert outputs[0]["blockers"] and outputs[0]["unblock"]


def test_check_json_global_error_matches_schema(tmp_path, capsys):
    pkg = write_project(tmp_path)
    (tmp_path / "uv.lock").write_text('version = 1\nrequires-python = ">=3.12"\n\n'
                                      '[[package]]\nname = "fastapi"\nversion = "0.99.1"\n')
    data = check_json(capsys, str(pkg), "--root", str(tmp_path))
    assert data["global"]
    validate(data, json.loads(SCHEMA.read_text()))


def test_schema_lists_exactly_the_keys_emitted():
    """The schema is strict (additionalProperties false everywhere it describes an object of py2axum)."""
    schema = json.loads(SCHEMA.read_text())

    def walk(s, where="$"):
        if s.get("type") == "object" and "properties" in s:
            assert s.get("additionalProperties") is False, f"{where}: the schema must be strict"
            for k, v in s["properties"].items():
                walk(v, f"{where}.{k}")
        if "items" in s:
            walk(s["items"], where + "[]")
    walk(schema)
