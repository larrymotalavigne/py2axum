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

import re
import sys
from dataclasses import dataclass
from pathlib import Path

# name -> (lowest tested, highest tested, first untested). Accepted: lowest <= v < first untested. Mirrored by
# the `conformance` extra of pyproject.toml and by docs/supported.md (tests/test_versions.py); the CI job
# `versions` installs both ends (`python -m py2axum.versions min|max`) and runs pytest + the dynapp conformance.
SUPPORTED: dict[str, tuple[str, str, str]] = {
    "fastapi": ("0.137.0", "0.142.3", "0.143"),
    "starlette": ("1.0.0", "1.7.0", "1.8"),
    "pydantic": ("2.12.0", "2.13.5", "2.14"),
    "pydantic-core": ("2.41.1", "2.46.5", "2.47"),
    "pydantic-settings": ("2.11.0", "2.15.0", "2.16"),
    "sqlalchemy": ("2.0.44", "2.1.4", "2.2"),
    "psycopg": ("3.2.12", "3.3.6", "3.4"),
    "httpx": ("0.28.1", "0.28.1", "0.29"),
    "aiohttp": ("3.13.0", "3.14.4", "3.15"),
    "mcp": ("2.2.0", "2.2.0", "2.3"),  # MCP servers: tested through a real MCP server's lock (internal conformance)
}
PYTHON: tuple[str, str, str] = ("3.12", "3.14", "3.15")
# libraries checked only when the analysed package imports them (module -> distributions); the framework
# (fastapi, starlette, pydantic, pydantic-core) is always checked
_BY_IMPORT = {"pydantic_settings": ("pydantic-settings",), "sqlalchemy": ("sqlalchemy", "psycopg"),
              "httpx": ("httpx",), "aiohttp": ("aiohttp",), "mcp": ("mcp",)}
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


def pins(end: str) -> list[str]:
    """pip requirements of one end of the ranges (`min` or `max`), as the CI matrix installs them."""
    i = {"min": 0, "max": 1}[end]
    return [f"{_EXTRAS.get(n, n)}=={v[i]}" for n, v in SUPPORTED.items() if n != "mcp"]


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
            if f.name == dist and iv is not None and (iv[1] <= parse(lowest) or (f.exact and iv[0] < parse(lowest))):
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
    return errors


if __name__ == "__main__":
    print(" ".join(pins(sys.argv[1])))
