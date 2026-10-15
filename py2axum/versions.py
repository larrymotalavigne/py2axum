"""Supported library versions: the ranges the conformance suites run on, and the check of the analysed project.

The translation reproduces the behaviour of precise library versions (Pydantic's messages, Starlette's routing
and middlewares, SQLAlchemy's session...). A project that locks or pins a version outside these ranges is
refused rather than translated into a binary that may silently differ (`--allow-untested-versions` overrides).

Sources: a `uv.lock` at --root or above (up to the repository's root; exact versions), else, from the first
directory at --root or above holding one, `requirements*.txt` (pins and specifiers) and the `pyproject.toml`
dependencies (specifiers). A library the
project does not mention is not checked.
"""
from __future__ import annotations

import os
import re
import sys
from dataclasses import dataclass
from pathlib import Path

# name -> (lowest tested, highest tested, first untested). Accepted: lowest <= v < first untested. Mirrored by
# the `conformance` extra of pyproject.toml and by docs/reference/versions.md (tests/test_versions.py); the CI job
# `versions` installs both ends (`python -m py2axum.versions min|max`) and runs pytest + the dynapp conformance.
SUPPORTED: dict[str, tuple[str, str, str]] = {
    "fastapi": ("0.137.0", "0.142.3", "0.143"),
    "starlette": ("1.0.0", "1.7.0", "1.8"),
    "pydantic": ("2.12.0", "2.14.0", "2.15"),
    "pydantic-core": ("2.41.1", "2.50.0", "2.51"),
    "pydantic-settings": ("2.11.0", "2.15.0", "2.16"),
    "sqlalchemy": ("2.0.44", "2.1.4", "2.2"),
    "psycopg": ("3.2.12", "3.3.6", "3.4"),
    "httpx": ("0.28.1", "0.28.1", "0.29"),
    "aiohttp": ("3.13.0", "3.14.4", "3.15"),
    "mcp": ("2.2.0", "2.2.0", "2.3"),  # MCP servers: tested through a real MCP server's lock (internal conformance)
    "asyncpg": ("0.31.0", "0.32.0", "0.33"),
    "icalendar": ("7.0.0", "7.0.0", "7.1"),  # calendar files: tested through a real application's lock (internal conformance)  # the asyncpg driver: transcript-service's production driver (internal conformance)
    "orjson": ("3.13.0", "3.13.0", "3.14"),  # yyjson's messages and the allocation limit of this release
    "unidecode": ("1.4.0", "1.4.0", "1.5"),  # its transliteration tables (tools/gen_unidecode.py)
    "ecs-logging": ("2.3.0", "2.3.0", "2.4"),
    "pyyaml": ("6.0.2", "6.0.3", "6.1"),  # yaml.safe_load / safe_dump / dump: a port of its pure-Python loader and dumper
    "croniter": ("6.2.4", "6.2.4", "6.3"),  # dynrt/croniter.rs ports 6.2.4's croniter.py (6.0 differs: no W, no strict)
}
# outside the CI matrix: their range is the lock of an application conformance-tested internally
_LOCK_ONLY = ("mcp", "asyncpg", "icalendar")
PYTHON: tuple[str, str, str] = ("3.12", "3.15", "3.16")
# libraries checked only when the analysed package imports them (module -> distributions); the framework
# (fastapi, starlette, pydantic, pydantic-core) is always checked
_BY_IMPORT = {"pydantic_settings": ("pydantic-settings",), "sqlalchemy": ("sqlalchemy", "psycopg", "asyncpg"),
              "httpx": ("httpx",), "aiohttp": ("aiohttp",), "mcp": ("mcp",), "icalendar": ("icalendar",),
              "orjson": ("orjson",), "unidecode": ("unidecode",), "ecs_logging": ("ecs-logging",),
              "yaml": ("pyyaml",), "croniter": ("croniter",)}
# constructs translated with the semantics of a newer version than the range's lowest:
# (label, regex on the package's source, distribution, lowest version)
_FEATURES = [("WebSocket routes (Starlette 1.7's WebSocketDisconnected semantics)", r"\.websocket\(", "starlette", "1.7.0")]
# extras installed with the pins (the conformance apps' drivers)
_EXTRAS = {"psycopg": "psycopg[binary]", "sqlalchemy": "sqlalchemy[asyncio]"}


def parse(v: str) -> tuple:
    """A version as a comparable tuple: the release numbers, then -1 for a pre-release (rc, a, b, dev)."""
    m = re.match(r"\s*v?(\d+(?:\.\d+)*)(.*)", v)
    if not m:
        return ()
    rel = tuple(int(x) for x in m.group(1).split("."))
    rel = rel + (0,) * (3 - len(rel)) if len(rel) < 3 else rel
    return rel + ((-1,) if re.match(r"[.-]?(a|b|rc|c|dev|pre|alpha|beta)", m.group(2).lower()) else (0,))


def spec(name: str) -> str:
    lo, _, hi = SUPPORTED.get(name, PYTHON)
    return f">={lo},<{hi}"


def in_range(name: str, version: str) -> bool:
    lo, _, hi = SUPPORTED.get(name, PYTHON)
    return parse(lo) <= parse(version) < parse(hi)


# intermediate versions the CI matrix also runs (`max` otherwise): the minors whose behaviour the runtime
# reproduces apart from both ends (pydantic 2.13's messages, still locked by most projects)
MIDDLES: dict[str, dict[str, str]] = {
    "pydantic2.13": {"pydantic": "2.13.5", "pydantic-core": "2.46.5"},
}


def pins(end: str) -> list[str]:
    """pip requirements of one end of the ranges (`min` or `max`), or of a `MIDDLES` entry, as the CI matrix
    installs them."""
    i = {"min": 0}.get(end, 1)
    over = MIDDLES[end] if end not in ("min", "max") else {}
    return [f"{_EXTRAS.get(n, n)}=={over.get(n, v[i])}" for n, v in SUPPORTED.items() if n not in _LOCK_ONLY]


def _bump(rel: tuple, n: int) -> tuple:
    """The first version after the prefix of length n (`2.13.*` -> 2.14)."""
    head = list(rel[:n]) + [0] * max(0, n - len(rel))
    head[-1] += 1
    return parse(".".join(map(str, head)))


def interval(specifier: str) -> tuple[tuple, tuple] | None:
    """[low, high) allowed by a PEP 440 specifier (`!=` ignored); None if it cannot be read."""
    lo, hi = (0,), (10**9,)
    for clause in filter(None, (c.strip() for c in specifier.split(","))):
        m = re.fullmatch(r"(===|==|~=|>=|<=|!=|>|<)\s*([\w.*+!-]+)", clause)
        if not m:
            return None
        op, v = m.groups()
        if op == "!=":
            continue
        star = v.endswith(".*")
        rel = parse(v.removesuffix(".*"))
        if not rel:
            return None
        n = len(v.removesuffix(".*").split("."))
        if op in ("==", "===") and star:
            lo, hi = max(lo, rel), min(hi, _bump(rel, n))
        elif op in ("==", "==="):
            lo, hi = max(lo, rel), min(hi, rel[:-1] + (1,))
        elif op == "~=":
            lo, hi = max(lo, rel), min(hi, _bump(rel, max(1, n - 1)))
        elif op == ">=":
            lo = max(lo, rel)
        elif op == ">":
            lo = max(lo, rel[:-1] + (1,))
        elif op == "<":
            hi = min(hi, rel)
        elif op == "<=":
            hi = min(hi, rel[:-1] + (1,))
    return lo, hi


@dataclass
class Found:
    name: str
    constraint: str  # "2.13.5" (exact) or ">=2.9" (specifier)
    exact: bool
    file: str
    line: int


def _norm(name: str) -> str:
    return re.sub(r"[-_.]+", "-", name).lower()


def _line_of(text: str, pos: int) -> int:
    return text.count("\n", 0, pos) + 1


_REQ = re.compile(r"^[ \t]*([A-Za-z0-9][A-Za-z0-9._-]*)[ \t]*(\[[^\]\n]*\])?[ \t]*([<>=!~][^;#\n]*)?", re.M)


def _from_requirement(text: str, file: str, base: int = 0, offset: int = 0) -> list[Found]:
    out = []
    for m in _REQ.finditer(text):
        name = _norm(m.group(1))
        if name not in SUPPORTED or not m.group(3):
            continue
        c = m.group(3).strip().replace(" ", "")
        exact = bool(re.fullmatch(r"===?[\w.+!-]+", c)) and not c.endswith("*")
        out.append(Found(name, c.lstrip("=") if exact else c, exact, file, base + _line_of(text, m.start()) - offset))
    return out


def project_constraints(root: Path) -> tuple[list[Found], tuple[str, str, int] | None]:
    """The constraints the project puts on the supported libraries, and its `requires-python`
    (specifier, file, line), from the first directory at --root or above holding a dependency file."""
    d = Path(root).resolve()
    # a uv.lock at --root or above is what `uv sync --frozen` installs: it wins over the requirements and
    # pyproject.toml of a subdirectory (a stale back/requirements.txt next to a locked project)
    up = []
    for cand in (d, *d.parents):
        up.append(cand)
        if (cand / "uv.lock").is_file() or (cand / ".git").exists():
            break
    locked = next((c for c in up if (c / "uv.lock").is_file()), None)
    for cand in ([locked] if locked else (d, *d.parents)):
        lock, pp = cand / "uv.lock", cand / "pyproject.toml"
        reqs = sorted(cand.glob("requirements*.txt"))
        found: list[Found] = []
        py = None
        if lock.is_file():
            text = lock.read_text()
            m = re.search(r'^requires-python = "([^"]+)"', text, re.M)
            if m:
                py = (m.group(1), str(lock), _line_of(text, m.start()))
            for m in re.finditer(r'^\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"', text, re.M):
                name = _norm(m.group(1))
                if name in SUPPORTED:
                    found.append(Found(name, m.group(2), True, str(lock), _line_of(text, m.start()) + 2))
            return found, py
        for r in reqs:
            found += _from_requirement(r.read_text(), str(r))
        if pp.is_file():
            text = pp.read_text()
            m = re.search(r'^requires-python\s*=\s*"([^"]+)"', text, re.M)
            if m:
                py = (m.group(1), str(pp), _line_of(text, m.start()))
            for m in re.finditer(r'"([A-Za-z0-9][^"\n]*)"', text):
                f = _from_requirement(m.group(1), str(pp))
                for x in f:
                    x.line = _line_of(text, m.start())
                found += f
        if found or py or reqs or pp.is_file():
            return found, py
        if (cand / ".git").exists():
            break
    return [], None


def used(package: Path) -> tuple[set[str], list[tuple[str, str, str]]]:
    """The supported distributions the package's code can reach (the framework, plus the libraries it imports:
    a lock also lists what other parts of the repository, or the dependencies, use), and the `_FEATURES` it uses."""
    out = {"fastapi", "starlette", "pydantic", "pydantic-core"}
    feats = set()
    imp = re.compile(r"^[ \t]*(?:from[ \t]+(\w+)|import[ \t]+(\w+))", re.M)
    for f in Path(package).rglob("*.py"):
        try:
            text = f.read_text(errors="replace")
        except OSError:
            continue
        for m in imp.finditer(text):
            out.update(_BY_IMPORT.get(m.group(1) or m.group(2), ()))
        feats.update(i for i, (_, rx, _, _) in enumerate(_FEATURES) if re.search(rx, text))
    return out, [(_FEATURES[i][0], _FEATURES[i][2], _FEATURES[i][3]) for i in sorted(feats)]


def python_version_file(root: Path) -> tuple[tuple[int, int], str] | None:
    """The `.python-version` (uv, pyenv) at --root or above, up to the repository's root: (major, minor), file."""
    d = Path(root).resolve()
    for cand in (d, *d.parents):
        f = cand / ".python-version"
        if f.is_file():
            for line in f.read_text().splitlines():
                m = re.match(r"\s*(?:cpython-|python)?(\d+)\.(\d+)", line)
                if m:
                    return (int(m.group(1)), int(m.group(2))), str(f)
            return None
        if (cand / ".git").exists():
            break
    return None


def add_python_flag(ap) -> None:
    """`--python X.Y` on a command line (the project's Python, see target_python)."""
    ap.add_argument("--python", default=None, metavar="X.Y",
                    help="the Python the project runs on (default: its .python-version, else this Python if the "
                         "project's requires-python allows it); messages and sys.version_info follow it")


def apply_python_flag(args) -> None:
    if getattr(args, "python", None):
        os.environ["PY2AXUM_TARGET_PYTHON"] = args.python


def target_python_patch(root: Path) -> int | None:
    """The patch release of the project's Python (behaviours that changed inside a minor, like `strptime('%e')`
    in 3.13.6): the one `--python` or `.python-version` names, else the transpiler's own when it runs the
    project's minor (a Docker build), else None (unknown: the runtime takes the newest)."""
    tp = target_python(root)
    forced = os.environ.get("PY2AXUM_TARGET_PYTHON")
    pv_file = python_version_file(root)
    text = forced if forced else (Path(pv_file[1]).read_text() if pv_file else None)
    if text is not None:
        m = re.search(r"(\d+)\.(\d+)\.(\d+)", text)
        if m and (int(m.group(1)), int(m.group(2))) == tp:
            return int(m.group(3))
        return None
    return sys.version_info[2] if tuple(sys.version_info[:2]) == tp else None


# behaviours that changed inside a minor (security releases backported by distributions: Debian's 3.13.5 has
# 3.13.13's base64 padding), probed on the transpiler's own Python when it runs the project's minor
PROBES = {
    # binascii's padding rules rewritten in 3.13.13 / 3.14.4: data after the padding is decoded
    "b64_padding_rfc": "import base64\ntry:\n base64.b64decode('aGk=aGk=')\n r=False\nexcept Exception:\n r=True",
    # `%e` accepted by strptime from 3.13.6
    "strptime_e": "import datetime\ntry:\n datetime.datetime.strptime('Oct  9 2026','%b %e %Y')\n r=True\nexcept ValueError:\n r=False",
    # json: a `\\uXXXX` escape ending the text is an unterminated string (3.13.13 / 3.14.4)
    "json_u_escape_unterminated": "import json\ntry:\n json.loads('\"\\\\u0041')\nexcept ValueError as e:\n r=str(e).startswith('Unterminated')",
    # a C timeout or timestamp out of range: one message for float and int from 3.13.15 / 3.14.7
    "pytime_range": "import socket\ntry:\n socket.socket().settimeout(1e20)\n r=False\nexcept OverflowError as e:\n r='C PyTime_t' in str(e)",
}


def probe_features(root: Path) -> dict[str, bool]:
    """PROBES answered by the transpiler's Python, when it is the project's (same minor, and no patch release
    named by --python or .python-version: then the version decides); empty otherwise."""
    if target_python(root) != tuple(sys.version_info[:2]):
        return {}
    forced = os.environ.get("PY2AXUM_TARGET_PYTHON")
    pv_file = python_version_file(root)
    text = forced if forced else (Path(pv_file[1]).read_text() if pv_file else "")
    if re.search(r"\d+\.\d+\.\d+", text):
        return {}
    out = {}
    for name, code in PROBES.items():
        scope: dict = {}
        exec(code, scope)  # noqa: S102 — fixed probes of the standard library
        out[name] = bool(scope["r"])
    return out


def target_python(root: Path) -> tuple[int, int]:
    """The Python the project runs on, whose messages and behaviours the binary reproduces (and against which
    `sys.version_info` is folded): `--python` (PY2AXUM_TARGET_PYTHON), else the project's `.python-version`,
    else the transpiler's own Python when the project's `requires-python` allows it (a Docker build transpiles
    on the image's Python), else the lowest Python `requires-python` allows."""
    forced = os.environ.get("PY2AXUM_TARGET_PYTHON")
    if forced:
        m = re.fullmatch(r"\s*(\d+)\.(\d+)(?:\.\d+)?\s*", forced)
        if not m:
            raise ValueError(f"--python {forced!r}: expected MAJOR.MINOR, e.g. 3.14")
        return int(m.group(1)), int(m.group(2))
    pv = python_version_file(root)
    if pv:
        return pv[0]
    mine = tuple(sys.version_info[:2])
    _, py = project_constraints(root)
    iv = interval(py[0]) if py else None
    if iv is None or iv[0] <= parse(".".join(map(str, sys.version_info[:3]))) < iv[1]:
        return mine
    return iv[0][0], iv[0][1]


def check_project(root: Path, package: Path | None = None) -> list[tuple[str, str, int]]:
    """Errors (message, file, line) for every library or Python constraint of the project outside the
    supported ranges; empty when everything is in range or not constrained. With `package`, only the
    libraries it uses (`used`) are checked."""
    from importlib.metadata import version as _v

    try:
        mine = "py2axum " + _v("py2axum")
    except Exception:
        mine = "py2axum"
    found, py = project_constraints(root)
    features = []
    if package is not None:
        reach, features = used(package)
        found = [f for f in found if f.name in reach]
    errors = []
    for f in found:
        for label, dist, lowest in features:
            iv = (parse(f.constraint), parse(f.constraint)) if f.exact else interval(f.constraint)
            if f.name == dist and iv is not None and (iv[0] < parse(lowest) if f.exact else iv[1] <= parse(lowest)):
                errors.append((f"{dist} {f.constraint}: {label} need {dist}>={lowest}", f.file, f.line))
        lo, hi = parse(SUPPORTED[f.name][0]), parse(SUPPORTED[f.name][2])
        if f.exact:
            if not in_range(f.name, f.constraint):
                errors.append((f"{f.name} {f.constraint} is outside the range {mine} is tested on "
                               f"({f.name}{spec(f.name)}): the binary could behave differently from the Python app. "
                               f"Use a version in range, or --allow-untested-versions to translate anyway",
                               f.file, f.line))
            continue
        iv = interval(f.constraint)
        if iv is not None and (iv[0] >= hi or iv[1] <= lo):
            errors.append((f"{f.name}{f.constraint} excludes every version {mine} is tested on "
                           f"({f.name}{spec(f.name)}). Widen the constraint, or --allow-untested-versions to "
                           f"translate anyway", f.file, f.line))
    if py is not None:
        iv = interval(py[0])
        lo, hi = parse(PYTHON[0]), parse(PYTHON[2])
        if iv is not None and (iv[0] >= hi or iv[1] <= lo):
            errors.append((f"requires-python {py[0]} excludes the Pythons py2axum supports (python{spec('python')})",
                           py[1], py[2]))
        elif iv is not None and iv[0] > parse(".".join(map(str, sys.version_info[:3]))):
            errors.append((f"requires-python {py[0]}: run py2axum on a Python at least as recent as the project's "
                           f"(this one is {sys.version_info[0]}.{sys.version_info[1]}; the parser only knows its own syntax)",
                           py[1], py[2]))
    try:
        tp = target_python(root)
    except ValueError as e:
        return errors + [(str(e), "--python", 0)]
    pv = python_version_file(root)
    where = ("--python", 0) if os.environ.get("PY2AXUM_TARGET_PYTHON") else ((pv[1], 1) if pv else None)
    if where is not None:
        if not parse(PYTHON[0]) <= parse(f"{tp[0]}.{tp[1]}") < parse(PYTHON[2]):
            errors.append((f"Python {tp[0]}.{tp[1]} is outside the Pythons py2axum supports (python{spec('python')})", *where))
        elif tp > tuple(sys.version_info[:2]):
            errors.append((f"the project runs on Python {tp[0]}.{tp[1]}: run py2axum on a Python at least as recent "
                           f"(this one is {sys.version_info[0]}.{sys.version_info[1]}; the parser only knows its own syntax)", *where))
    return errors


if __name__ == "__main__":
    print(" ".join(pins(sys.argv[1])))
