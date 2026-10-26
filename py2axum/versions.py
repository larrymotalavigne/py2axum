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

from .ir import TranspileError

# name -> (lowest tested, highest tested, first untested). Accepted: lowest <= v < first untested. Mirrored by
# the `conformance` extra of pyproject.toml and by docs/reference/versions.md (tests/test_versions.py); the CI job
# `versions` installs both ends (`python -m py2axum.versions min|max`) and runs pytest + the dynapp conformance.
SUPPORTED: dict[str, tuple[str, str, str]] = {
    "fastapi": ("0.137.0", "0.156.8", "0.157"),
    "starlette": ("1.0.0", "1.9.0", "1.10"),
    "pydantic": ("2.12.0", "2.14.0", "2.15"),
    "pydantic-core": ("2.41.1", "2.50.0", "2.51"),
    "pydantic-settings": ("2.11.0", "2.16.0", "2.17"),
    "sqlalchemy": ("2.0.44", "2.1.5", "2.2"),
    "psycopg": ("3.2.12", "3.3.6", "3.4"),
    "httpx": ("0.28.1", "0.28.1", "0.29"),
    "aiohttp": ("3.13.0", "3.14.4", "3.15"),
    "mcp": ("2.2.0", "2.2.0", "2.3"),  # MCP servers: tested through a real MCP server's lock (internal conformance)
    "asyncpg": ("0.31.0", "0.32.0", "0.33"),
    "icalendar": ("7.0.0", "7.0.0", "7.1"),  # calendar files: tested through a real application's lock (internal conformance)  # the asyncpg driver: transcript-service's production driver (internal conformance)
    # yyjson's messages and the allocation limit: 3.12.0 answers fixtures/libapp/orjsonx.py's 738 cases byte for byte
    # like 3.13.0 (measured 18/10, Python 3.12)
    "orjson": ("3.12.0", "3.13.0", "3.14"),
    "unidecode": ("1.4.0", "1.4.0", "1.5"),  # its transliteration tables (tools/gen_unidecode.py)
    "ecs-logging": ("2.3.0", "2.3.0", "2.4"),
    "pyyaml": ("6.0.2", "6.0.3", "6.1"),  # yaml.safe_load / safe_dump / dump: a port of its pure-Python loader and dumper
    "croniter": ("6.2.4", "6.2.4", "6.3"),
    "dnspython": ("2.8.0", "2.8.0", "2.9"),  # dynrt/dnsres.rs: the stub resolver (Resolver, resolve), its retries and messages
    "itsdangerous": ("2.2.0", "2.2.0", "2.3"),  # dynrt/itsd.rs: URLSafe(Timed)Serializer, tokens byte for byte  # dynrt/croniter.rs ports 6.2.4's croniter.py (6.0 differs: no W, no strict)
    "pytz": ("2026.5", "2026.5", "2026.6"),  # dynrt/pytzrt.rs: its zones (tools/gen_pytz.py), one tz database release per version
    # dynrt/duparse.rs, dutz.rs (parser, tz; relativedelta is checked only with them): 2.8.2 has the same code but an
    # older zoneinfo tarball
    "python-dateutil": ("2.9.0", "2.9.0.post0", "2.10"),
    "cachetools": ("7.2.2", "7.2.2", "7.3"),  # dynrt/ctools.rs (7.2.1 reads the TTL timer later, 7.0 had no size checks)
    # dynrt/stripe.rs: stripe-python's requests, objects and errors (15.0 rewrote StripeObject: no longer a dict)
    "stripe": ("14.0.0", "14.4.1", "14.5"),
    # dynrt/xlsx.rs: openpyxl's writer without lxml (ElementTree through et_xmlfile), its parts byte for byte
    "openpyxl": ("3.1.5", "3.1.5", "3.2"),
    "et-xmlfile": ("2.0.0", "2.0.0", "2.1"),
    # dynrt/pgpy.rs, pgpycore.rs: PGPKey.from_blob() (armor, packets, pgpy's order of the user ids); pyasn1 decodes
    # the curve OIDs of the EC keys (its messages are in pgpy's errors)
    "pgpy": ("0.6.0", "0.6.0", "0.7"),
    "pyasn1": ("0.6.4", "0.6.4", "0.7"),
    # what pgpy reaches of cryptography while parsing: the deprecation warnings of TripleDES and Camellia (a protected
    # secret key), whose removal from `algorithms` would change pgpy's behaviour; checked only with pgpy
    "cryptography": ("50.0.2", "50.0.2", "51"),
    # dynrt/rl/: reportlab's pure-Python path (no rl_accel), its PDF byte for byte but the instants (4.4.6 changed the
    # header comment of every PDF)
    "reportlab": ("4.4.5", "4.4.5", "4.4.6"),
    # dynrt/qr.rs: qrcode's encoding, and the PNG of Pillow's wheels (zlib-ng 2.3.3 in 12.3.0's: its deflate output)
    "qrcode": ("8.2", "8.2", "8.3"),
    "pillow": ("12.3.0", "12.3.0", "12.4"),
    # dynrt/loguru.rs: the default handler, its format and colours, the level methods, add() on a standard stream
    "loguru": ("0.7.2", "0.7.3", "0.8"),
    # dynrt/auth.rs: passlib's CryptContext of bcrypt over pyca/bcrypt. bcrypt 4.0 to 4.2 differ in their errors
    # (a short salt panics); 4.3 hashes the first 72 bytes of a longer password, 5.0 refuses it, and with 5.0
    # passlib 1.7.4 can no longer load its bcrypt backend (every hash and verify raises: reproduced)
    "bcrypt": ("4.3.0", "5.0.0", "5.1"),
    "passlib": ("1.7.4", "1.7.4", "1.8"),
    # dynrt/dotenv.rs: load_dotenv / dotenv_values, a port of 1.2's parser.py, variables.py and main.py
    "python-dotenv": ("1.2.0", "1.2.4", "1.3"),
}
# outside the CI matrix: their range is the lock of an application conformance-tested internally
_LOCK_ONLY = ("mcp", "asyncpg", "icalendar")
PYTHON: tuple[str, str, str] = ("3.12", "3.15", "3.16")
# libraries checked only when the analysed package imports them (module -> distributions); the framework
# (fastapi, starlette, pydantic, pydantic-core) is always checked
_BY_IMPORT = {"pydantic_settings": ("pydantic-settings",), "sqlalchemy": ("sqlalchemy", "psycopg", "asyncpg"),
              "httpx": ("httpx",), "aiohttp": ("aiohttp",), "mcp": ("mcp",), "icalendar": ("icalendar",),
              "orjson": ("orjson",), "unidecode": ("unidecode",), "ecs_logging": ("ecs-logging",),
              "yaml": ("pyyaml",), "croniter": ("croniter",),
              "itsdangerous": ("itsdangerous",), "dns": ("dnspython",), "pytz": ("pytz",), "cachetools": ("cachetools",),
              "openpyxl": ("openpyxl", "et-xmlfile"), "reportlab": ("reportlab",), "qrcode": ("qrcode", "pillow"),
              "loguru": ("loguru",),
              "dateutil.parser": ("python-dateutil",), "dateutil.tz": ("python-dateutil",),
              "stripe": ("stripe",), "pgpy": ("pgpy", "pyasn1", "cryptography"),
              "bcrypt": ("bcrypt",), "passlib": ("passlib", "bcrypt"), "dotenv": ("python-dotenv",)}
# constructs translated with the semantics of a newer version than the range's lowest:
# (label, regex on the package's source, distribution, lowest version)
_FEATURES = [("WebSocket routes (Starlette 1.7's WebSocketDisconnected semantics)", r"\.websocket\(", "starlette", "1.7.0")]
# what is known to differ between versions of a library, for the message of a version refusal: (label, regex on the
# package's source, distribution). The runtime reproduces the project's version of these (docs/reference/versions.md);
# a version outside the tested ranges is where they are not proven. Not a proof of absence: other behaviours may differ.
_SENSITIVE = [
    ("an `EmailStr` field (a CR or LF in the address changed in Pydantic 2.14)", r"\bEmailStr\b", "pydantic"),
    ("a `UUID` field (the error message changed in Pydantic 2.14)", r"\bUUID[1-8]?\b", "pydantic"),
    ("`max_digits` / `decimal_places` on a `Decimal` (rounding changed in 2.14)", r"\bmax_digits\b|\bdecimal_places\b", "pydantic"),
    ("an Enum `_missing_` (an exception other than ValueError changed in 2.14)", r"def\s+_missing_\b", "pydantic"),
    ("a `model_validate_json` or JSON-mode validation (messages follow the minor)", r"model_validate_json|TypeAdapter", "pydantic"),
    ("WebSocket routes (the disconnect semantics of Starlette 1.7)", r"\.websocket\(", "starlette"),
    ("`CORSMiddleware` (its preflight and header handling changed between 1.x minors)", r"\bCORSMiddleware\b", "starlette"),
    ("uploads and forms (the multipart limits and messages follow Starlette's version)", r"\bUploadFile\b|\bForm\(", "starlette"),
    ("a JSON-lines or server-sent-events endpoint (the status was ignored before 0.141)", r"EventSourceResponse|ServerSentEvent|JSONLResponse", "fastapi"),
    ("an `include_router` with response models (the element type was lost before 0.141)", r"\binclude_router\(", "fastapi"),
    ("a lazy-loaded relationship in async code (MissingGreenlet on 2.0, StatementError on 2.1)", r"\blazy=|selectinload|joinedload|AsyncSession", "sqlalchemy"),
    ("`orjson` (parse messages and size limits follow its yyjson version)", r"\borjson\b", "orjson"),
    ("`cachetools` caches (the timer and size checks changed between 7.x releases)", r"\bcachetools\b", "cachetools"),
    ("`pytz` zones (one tz database release per version)", r"\bpytz\b", "pytz"),
    ("`stripe` objects (StripeObject was rewritten in 15.0)", r"\bstripe\b", "stripe"),
]


def sensitive(package: Path, dist: str) -> list[str]:
    """The `_SENSITIVE` constructs of `dist` found in the package, as `label (file:line)`."""
    rules = [(lab, re.compile(rx)) for lab, rx, d in _SENSITIVE if d == dist]
    found: dict[str, str] = {}
    if not rules:
        return []
    for f in sorted(Path(package).rglob("*.py")):
        try:
            text = f.read_text(errors="replace")
        except OSError:
            continue
        for lab, rx in rules:
            if lab not in found and (m := rx.search(text)):
                found[lab] = f"{f.relative_to(package)}:{_line_of(text, m.start())}"
    return [f"{lab} ({where})" for lab, where in found.items()]


def _known_differences(package: Path | None, dist: str) -> str:
    """The sentence a version refusal adds: what the project uses that is known to differ between versions."""
    if package is None:
        return ""
    hits = sensitive(package, dist)
    if not hits:
        return (f" No construct known to differ between {dist} versions was found in the project (that is not a "
                f"proof: --allow-untested-versions translates it, then compare the binary with the Python app).")
    return f" Constructs of this project known to differ between {dist} versions: " + "; ".join(hits) + "."


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
    # the previous highest: FastAPI changed its request handling from 0.144 to 0.156 (dynrt `fastapi_at_least`)
    "fastapi0.142": {"fastapi": "0.142.3", "starlette": "1.7.0", "sqlalchemy": "2.1.4"},
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


# requirements files the last project_constraints() could not decode: reported by check_project()
UNREADABLE: list[tuple[str, str, int]] = []


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
            try:
                found += _from_requirement(read_requirements(r), str(r))
            except TranspileError as e:
                UNREADABLE.append((e.msg, str(r), 1))
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
    # dateutil: checked when its parser or tz is imported (relativedelta alone reads no version-dependent data)
    du = re.compile(r"^[ \t]*(?:from[ \t]+dateutil(?:\.(?:parser|tz)\b|[ \t]+import[ \t]+[^\n]*\b(?:parser|tz)\b)|"
                    r"import[ \t]+dateutil\.(?:parser|tz)\b)", re.M)
    for f in Path(package).rglob("*.py"):
        try:
            text = f.read_text(errors="replace")
        except OSError:
            continue
        for m in imp.finditer(text):
            out.update(_BY_IMPORT.get(m.group(1) or m.group(2), ()))
        if du.search(text):
            out.update(_BY_IMPORT["dateutil.tz"])
        feats.update(i for i, (_, rx, _, _) in enumerate(_FEATURES) if re.search(rx, text))
    return out, [(_FEATURES[i][0], _FEATURES[i][2], _FEATURES[i][3]) for i in sorted(feats)]


def read_requirements(path: Path) -> str:
    """A requirements file decoded as pip does (pip._internal.utils.encoding.auto_decode): a BOM (UTF-8, UTF-16,
    UTF-32: `pip freeze > requirements.txt` under PowerShell writes UTF-16), else a `# -*- coding: x -*-` line, else
    UTF-8. Undecodable: refused with the file's name, never a Python error."""
    import codecs

    from .ir import TranspileError

    data = path.read_bytes()
    boms = [(codecs.BOM_UTF8, "utf-8"), (codecs.BOM_UTF16, "utf-16"), (codecs.BOM_UTF16_BE, "utf-16-be"),
            (codecs.BOM_UTF16_LE, "utf-16-le"), (codecs.BOM_UTF32, "utf-32"), (codecs.BOM_UTF32_BE, "utf-32-be"),
            (codecs.BOM_UTF32_LE, "utf-32-le")]
    enc, start = "utf-8", 0
    for bom, name in boms:
        if data.startswith(bom):
            enc, start = name, len(bom)
            break
    else:
        m = re.search(rb"coding[:=]\s*([-\w.]+)", b"\n".join(data.split(b"\n")[:2]))
        if m:
            enc = m.group(1).decode("ascii")
    try:
        return data[start:].decode(enc)
    except (UnicodeDecodeError, LookupError) as e:
        why = e.reason if isinstance(e, UnicodeDecodeError) else str(e)
        raise TranspileError(f"{path}: cannot be read as text ({enc}: {why}); save it as UTF-8 (pip reads a BOM or a "
                             "`# -*- coding: ... -*-` line)", None, str(path))


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
    # urlsplit accepts an IPvFuture host written `[V...]` (only `[v...]` before; mimetypes.guess_type too)
    "ipvfuture_upper": "import urllib.parse\ntry:\n urllib.parse.urlsplit('http://[V1.x]/')\n r=True\nexcept ValueError:\n r=False",
    # urlunsplit: an empty netloc no longer written `//` before a relative path (3.12.4)
    "urlunsplit_new": "from urllib.parse import urlunsplit\nr=urlunsplit(('http', '', 'x', '', ''))=='http:x'",
    "pytime_range": "import socket\ntry:\n socket.socket().settimeout(1e20)\n r=False\nexcept OverflowError as e:\n r='C PyTime_t' in str(e)",
    # the email header folder (dynrt/emailhdr.rs), changed in security releases (3.12.13, 3.13.12-14, 3.14.3-6)
    "email_ws_fix": "from email import _header_value_parser as p\nr=not p.WhiteSpaceTerminal('', 'fws').startswith_fws()",
    "email_pop_empty": "from email import _header_value_parser as p\nl=[' ']\np._steal_trailing_WSP_if_exists(l)\nr=l==[]",
    "email_last_word_ew": "import inspect\nfrom email import _header_value_parser as p\n"
                          "r='last_word_is_ew' in inspect.signature(p._fold_as_ew).parameters",
    "email_addr_fix": "from email.headerregistry import HeaderRegistry\nr=str(HeaderRegistry()('To', 'a@b.com junk'))=='a@b.com, <>'",
    "email_ew_phrase": "from email import _header_value_parser as p\nr=str(p.get_phrase('=?utf-8?q?a?= =?utf-8?q?b?=')[0])=='ab'",
    "email_verify_bytes": "import email.generator as g\nr=hasattr(g, 'NEWLINE_WITHOUT_FWSP_BYTES')",
    # html.parser, under reportlab's paragraph markup (dynrt/rl/paraparser.rs): the HTML5 rewrite (3.12.12, 3.13.8),
    # then the RAWTEXT elements and `&#` (3.12.13 / 3.13.10 / 3.14.1), the abrupt comment close (3.13.15 / 3.14.7)
    "htmlparser_html5": "import html.parser as h\nr=hasattr(h.HTMLParser, '_set_support_cdata')",
    "htmlparser_rawtext_more": "import html.parser as h\nr='xmp' in h.HTMLParser.CDATA_CONTENT_ELEMENTS",
    "htmlparser_charref_data": "import html.parser as h\nr=hasattr(h, 'incomplete_charref')",
    "htmlparser_abrupt_comment": "import html.parser as h\nclass P(h.HTMLParser):\n def handle_data(s, d): s.d.append(d)\n"
                                 "p=P(convert_charrefs=False)\np.d=[]\np.feed('<!-->x-->')\nr=p.d==['x-->']",
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
    UNREADABLE.clear()
    found, py = project_constraints(root)
    unreadable = list(UNREADABLE)
    features = []
    if package is not None:
        reach, features = used(package)
        found = [f for f in found if f.name in reach]
    errors = unreadable
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
                               f"Use a version in range, or --allow-untested-versions to translate anyway."
                               + _known_differences(package, f.name), f.file, f.line))
            continue
        iv = interval(f.constraint)
        if iv is not None and (iv[0] >= hi or iv[1] <= lo):
            errors.append((f"{f.name}{f.constraint} excludes every version {mine} is tested on "
                           f"({f.name}{spec(f.name)}). Widen the constraint, or --allow-untested-versions to "
                           f"translate anyway." + _known_differences(package, f.name), f.file, f.line))
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
