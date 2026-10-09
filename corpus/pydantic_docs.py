"""The Pydantic corpus: the documentation's code blocks (`docs/**/*.md`) turned into FastAPI apps.

Each runnable block (or group of blocks sharing their globals, `{group="..."}`) is first run in Python with
`BaseModel` instrumented: every validation of a model the block defines (`Model(**data)`,
`Model.model_validate(obj)`, `Model.model_validate_json(text)`) and every serialisation call
(`model_dump(...)`, `model_dump_json(...)` with literal options) is recorded. The block's definitions
(imports, classes, functions, plain assignments; not the statements that run the examples) then become a module
with one route per model:

    POST /v/<Model>        the body validated as the model and returned (validation, then serialisation)
    POST /d/<Model>/<n>    the body validated, then the n-th recorded `model_dump(mode="json", ...)` /
                           `model_dump_json(...)` call returned

and the recorded inputs that a JSON body can carry become its requests. Blocks marked `test="skip"` or
`xfail`, or without a model validated from JSON-representable data, are not in the corpus.
"""
from __future__ import annotations

import ast
import json
import re
import subprocess
import sys
import textwrap
from pathlib import Path

import examples as exs

FENCE = re.compile(r"^(?P<indent>[ \t]*)```(?:py|python)(?:[ \t]+\{(?P<settings>[^}]*)\})?[ \t]*\n(?P<body>.*?)^(?P=indent)```",
                   re.M | re.S)
SETTING = re.compile(r'(\w+)="([^"]*)"')
MAX_INPUTS = 12

RECORDER = r'''
import json, math, sys
from pydantic import BaseModel
calls, depth = [], [0]


def _ours(cls):
    return cls.__module__ == "__main__" and "<locals>" not in cls.__qualname__ and "." not in cls.__qualname__


def _wrap(name, kind):
    orig = getattr(BaseModel, name)
    fn = orig.__func__ if isinstance(orig, classmethod) or hasattr(orig, "__func__") else orig

    def inner(first, *args, **kw):
        cls = first if isinstance(first, type) else type(first)
        if depth[0] == 0 and _ours(cls):
            calls.append([kind, cls.__qualname__, [repr(a) for a in args], {k: repr(v) for k, v in kw.items()},
                          _plain(args[0]) if args and kind in ("validate", "json") else None,
                          _plain(kw) if kind == "init" else None])
        depth[0] += 1
        try:
            return fn(first, *args, **kw)
        finally:
            depth[0] -= 1

    setattr(BaseModel, name, classmethod(inner) if kind in ("validate", "json") else inner)


def _plain(x):
    def ok(v):
        if v is None or isinstance(v, (bool, str)) or type(v) is int:
            return True
        if type(v) is float:
            return math.isfinite(v)
        if type(v) is list:
            return all(ok(i) for i in v)
        if type(v) is dict:
            return all(type(k) is str and ok(i) for k, i in v.items())
        return False
    if isinstance(x, (str, bytes)) and not isinstance(x, bool):
        return {"raw": x.decode() if isinstance(x, bytes) else x}
    return {"json": x} if ok(x) else None


_wrap("__init__", "init")
_wrap("model_validate", "validate")
_wrap("model_validate_json", "json")
_wrap("model_dump", "dump")
_wrap("model_dump_json", "dumpjson")
src = open(sys.argv[1]).read()
import io, contextlib
try:
    with contextlib.redirect_stdout(io.StringIO()):
        exec(compile(src, "<docs>", "exec"), {"__name__": "__main__"})
except BaseException as e:  # noqa: BLE001
    calls.append(["exception", type(e).__name__, [], {}, None, None])
json.dump(calls, open(sys.argv[2], "w"))
'''


def blocks(repo: Path):
    """(page, line, settings, code) of every python block of the documentation, in page order."""
    docs = repo / "docs"
    for md in sorted(docs.rglob("*.md")):
        text = md.read_text()
        for m in FENCE.finditer(text):
            settings = dict(SETTING.findall(m.group("settings") or ""))
            body = textwrap.dedent("\n".join(line[len(m.group("indent")):] if line.startswith(m.group("indent")) else line
                                             for line in m.group("body").splitlines()))
            line = text.count("\n", 0, m.start()) + 2
            yield str(md.relative_to(repo)), line, settings, body


def units(repo: Path):
    """Blocks grouped as pytest-examples runs them: a `group` shares its globals with its earlier blocks."""
    groups: dict[tuple[str, str], dict] = {}
    out = []
    for page, line, settings, code in blocks(repo):
        test = settings.get("test", "")
        if test.startswith(("skip", "xfail")) or settings.get("requires", "3.0") > f"{sys.version_info[0]}.{sys.version_info[1]}":
            continue
        code = re.sub(r"\s*#\s*\(\d+\)!", "", code)  # mkdocs annotations
        g = settings.get("group")
        if g:
            key = (page, g)
            if key not in groups:
                groups[key] = {"page": page, "line": line, "parts": []}
                out.append(groups[key])
            groups[key]["parts"].append((line, code))
        else:
            out.append({"page": page, "line": line, "parts": [(line, code)]})
    return out


def _sanitise(parts, models: set[str]) -> tuple[str, list[int]]:
    """The definitions of the blocks (what a module of an application would hold), and for each line of the
    result the documentation line it comes from."""
    lines, origin = [], []
    for start, code in parts:
        try:
            tree = ast.parse(code)
        except SyntaxError:
            continue
        src = code.splitlines()
        for node in tree.body:
            keep = isinstance(node, (ast.Import, ast.ImportFrom, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef,
                                     ast.TypeAlias))
            if isinstance(node, (ast.Assign, ast.AnnAssign)) and node.value is not None:
                keep = not any(isinstance(c, ast.Call) and (
                    (isinstance(c.func, ast.Name) and (c.func.id in models or c.func.id in ("print", "input", "open")))
                    or (isinstance(c.func, ast.Attribute) and (c.func.attr.startswith(("model_", "validate_", "dump_")))))
                    for c in ast.walk(node.value))
            if not keep:
                continue
            first = node.decorator_list[0].lineno if getattr(node, "decorator_list", None) else node.lineno
            for i in range(first, node.end_lineno + 1):
                lines.append(src[i - 1])
                origin.append(start + i - 1)
            lines.append("")
            origin.append(0)
    return "\n".join(lines), origin


def _key(*parts: str) -> str:
    import hashlib

    return hashlib.sha256("\0".join(parts).encode()).hexdigest()[:16]


def _record(work: Path, ident: str, code: str) -> list:
    """The calls the block makes (cached by the block's code and the installed pydantic)."""
    import pydantic

    d = work / "record"
    d.mkdir(parents=True, exist_ok=True)
    (d / "recorder.py").write_text(RECORDER)
    out = d / f"{ident}.{_key(code, RECORDER, pydantic.VERSION)}.json"
    if out.exists():
        return json.loads(out.read_text())
    for old in d.glob(f"{ident}.*.json"):
        old.unlink()
    (d / f"{ident}.py").write_text(code)
    try:
        subprocess.run([sys.executable, str(d / "recorder.py"), str(d / f"{ident}.py"), str(out)], cwd=d,
                       capture_output=True, timeout=60)
    except subprocess.TimeoutExpired:
        return []
    return json.loads(out.read_text()) if out.exists() else []


def _literal_kwargs(kw: dict) -> str | None:
    parts = []
    for k, v in kw.items():
        try:
            ast.literal_eval(v)
        except (ValueError, SyntaxError):
            return None
        parts.append(f"{k}={v}")
    return ", ".join(parts)


def _app(defs: str, routes: list[tuple[str, str, str]]) -> str:
    out = [defs, "", "# --- corpus: one route per model of the block (corpus/pydantic_docs.py) ---",
           "from fastapi import FastAPI as _FastAPI", "from fastapi.responses import Response as _Response", "",
           "app = _FastAPI()", ""]
    for i, (cls, kind, kw) in enumerate(routes):
        if kind == "v":
            out += [f'@app.post("/v/{cls}")', f"def _v_{i}(item: {cls}) -> {cls}:", "    return item", ""]
        elif kind == "dump":
            out += [f'@app.post("/d/{cls}/{i}")', f"def _d_{i}(item: {cls}):",
                    f"    return item.model_dump(mode=\"json\"{', ' + kw if kw else ''})", ""]
        else:
            out += [f'@app.post("/d/{cls}/{i}")', f"def _d_{i}(item: {cls}):",
                    f"    return _Response(item.model_dump_json({kw}), media_type=\"application/json\")", ""]
    return "\n".join(out)


def _imports(path: Path, pkgdir: Path) -> str | None:
    """Whether the generated module imports (cached by its content)."""
    mark = pkgdir / ".imports-ok"
    key = _key(path.read_text())
    if mark.exists() and mark.read_text() == key:
        return None
    r = subprocess.run([sys.executable, "-c", "import importlib, sys; sys.path.insert(0, sys.argv[1]); "
                        "importlib.import_module('pkgs.' + sys.argv[2] + '.main')", str(pkgdir.parent.parent), pkgdir.name],
                       capture_output=True, text=True, timeout=60)
    if r.returncode == 0:
        mark.write_text(key)
        return None
    return (r.stderr.strip().splitlines() or ["?"])[-1]


def _prepare(u: dict, pk: Path, work: Path, repo: Path) -> exs.Example | str:
    """The corpus example of one unit of blocks, or why it is not in the corpus."""
    ident = "pyd__" + re.sub(r"[^a-z0-9]+", "_", u["page"].removeprefix("docs/").removesuffix(".md").lower()) + f"__l{u['line']}"
    raw = "\n\n".join(code for _, code in u["parts"])
    calls = _record(work, ident, raw)
    inputs: dict[str, list] = {}
    dumps: list[tuple[str, str, str]] = []
    for kind, cls, args, kw, plain_arg, plain_kw in calls:
        if kind == "init" and plain_kw is not None and not args:
            inputs.setdefault(cls, []).append(plain_kw)
        elif kind in ("validate", "json") and plain_arg is not None and not kw and len(args) == 1:
            if kind == "json" or "json" in plain_arg:
                inputs.setdefault(cls, []).append(plain_arg)
        elif kind in ("dump", "dumpjson") and not args:
            lit = _literal_kwargs({k: v for k, v in kw.items() if k != "mode"}) if kind == "dump" else _literal_kwargs(kw)
            if lit is not None and (cls, kind, lit) not in dumps:
                dumps.append((cls, kind, lit))
    inputs = {c: [x for i, x in enumerate(v) if x not in v[:i]][:MAX_INPUTS] for c, v in inputs.items()}
    if not inputs:
        return "no model validated from JSON data"
    routes = [(c, "v", "") for c in inputs] + [(c, k, kw) for c, k, kw in dumps if c in inputs]
    defs, origin = _sanitise(u["parts"], set(inputs) | {c for c, _, _ in dumps})
    d = pk / ident
    d.mkdir(parents=True, exist_ok=True)
    (d / "__init__.py").write_text("")
    main = _app(defs, routes)
    if not (d / "main.py").exists() or (d / "main.py").read_text() != main:
        (d / "main.py").write_text(main)
    if _imports(d / "main.py", d):
        return "definitions do not import"
    requests = []
    for i, (cls, kind, _) in enumerate(routes):
        path = f"/v/{cls}" if kind == "v" else f"/d/{cls}/{i}"
        for x in inputs[cls]:
            requests.append(["POST", path, x])
    (d / "lines.json").write_text(json.dumps({"page": u["page"], "origin": origin}))
    return exs.Example(ident, f"pkgs.{ident}.main", f"{u['page']}:{u['line']}", False, page=u["page"],
                       title=_title(repo / u["page"]), section="Pydantic", kind="pydantic", requests=requests)


def examples(repo: Path, work: Path, log, jobs: int = 8) -> list[exs.Example]:
    """The corpus examples of the Pydantic documentation (each with its requests), staged in <work>/pkgs."""
    from concurrent.futures import ThreadPoolExecutor

    pk = work / "pkgs"
    pk.mkdir(parents=True, exist_ok=True)
    (pk / "__init__.py").write_text("")
    us = units(repo)
    with ThreadPoolExecutor(jobs) as pool:
        got = list(pool.map(lambda u: _prepare(u, pk, work, repo), us))
    out = [g for g in got if isinstance(g, exs.Example)]
    skipped = {}
    for g in got:
        if isinstance(g, str):
            skipped[g] = skipped.get(g, 0) + 1
    log(f"pydantic: {len(us)} runnable blocks or groups, {len(out)} in the corpus; not in it: {skipped}")
    return out


def _title(md: Path) -> str:
    m = re.search(r"^#\s+(.+)$", md.read_text(), re.M)
    return m.group(1).strip() if m else md.stem


def unstage(text: str, ex: exs.Example, pkgs: Path) -> str:
    """file:line in the generated module -> the documentation's page:line."""
    meta = json.loads((pkgs / ex.id / "lines.json").read_text())
    base = re.escape(str((pkgs / ex.id).resolve()) + "/main.py") + "|" + re.escape(str(pkgs / ex.id) + "/main.py")

    def sub(m):
        n = int(m.group(1))
        origin = meta["origin"][n - 1] if 0 < n <= len(meta["origin"]) else 0
        return f"{meta['page']}:{origin}" if origin else f"{meta['page']} (generated route, line {n})"

    return re.sub(rf"(?:{base})(?!:\d)", meta["page"], re.sub(rf"(?:{base}):(\d+)", sub, text))


def replay(work: Path, slot: int, ex: exs.Example, binaries: dict, servers_cls):
    import httpx

    from replay import _conf

    servers = servers_cls(work, None, slot, [ex], binaries)
    try:
        if ex.module not in servers.map:
            return [], servers.startup
        urls = servers.map[ex.module]
        events = []
        with httpx.Client(timeout=20) as ref, httpx.Client(timeout=20) as cand:
            for method, path, payload in ex.requests:
                if "raw" in payload:
                    kw = {"content": payload["raw"].encode(), "headers": {"content-type": "application/json"}}
                else:
                    kw = {"json": payload["json"]}
                obs = {}
                for which, c in (("ref", ref), ("cand", cand)):
                    try:
                        r = c.request(method, urls[which] + path, **kw)
                        obs[which] = _conf.observe(r, urls[which], None, method, path, set())
                    except httpx.TransportError as e:
                        obs[which] = {"req": f"{method} {path}", "error": type(e).__name__}
                events.append({"ev": "exchange", "module": ex.module, "test": f"{path} {json.dumps(payload)[:200]}",
                               "kind": "http", "same": json.dumps(obs["ref"]) == json.dumps(obs["cand"]),
                               "ref": obs["ref"], "cand": obs["cand"]})
        return events, servers.startup
    finally:
        servers.stop()

