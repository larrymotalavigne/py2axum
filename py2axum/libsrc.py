"""Libraries translated from their source (`--translate-libs`): the pure-Python distributions installed in the
project's environment, compiled like project code, only the functions the routes reach.

Precedence, module by module: the project (--root) > the library map (libmap.py: a native equivalent, written by
hand) > the library's source (this module) > refused. A distribution is translated from source only when it is
selected (`--translate-libs auto` or by name), pure Python (no compiled extension), under an admitted licence
(`--allow-license` adds one), and installed at the version the project locks, with files matching their `RECORD`
hashes: the binary carries the code the project runs, never a guess.

The environment is `--venv` (its site-packages), else a `.venv` at --root or above (up to the repository's root),
else the one py2axum runs in (a Docker build installs the project next to it).

Library modules are never part of the transpiled package (no routes are collected there). Their folded trees
(`sys.version_info`, `sys.platform`) are cached in ~/.cache/py2axum/libs/<dist>-<version>-<hash>-<py2axum>-<python>/,
with the verdict of each function compiled (informative: a function is always compiled again for each project).
"""
from __future__ import annotations

import ast
import base64
import hashlib
import json
import os
import pickle
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

# admitted licences (SPDX identifiers, and the trove classifiers that name them): the translation is a derived work
# embedded in the binary
ADMITTED = ("MIT", "BSD-2-Clause", "BSD-3-Clause", "0BSD", "Apache-2.0", "ISC", "PSF-2.0", "Python-2.0", "Unlicense",
            "Zlib", "HPND",
            "BSD-4-Clause", "MPL-2.0")  # BSD-4-Clause and MPL-2.0: the project's choice (17/10/2026), per licence
# distributions admitted whatever licence text was left unrecognised: the project's choice (17/10/2026), per
# distribution. passlib 1.7: BSD-3-Clause, with code derived from Beer-ware, UnixCrypt and jBCrypt sources
ADMITTED_DISTS = {"passlib": "1.7."}
CLASSIFIERS = {
    "License :: OSI Approved :: MIT License": "MIT",
    "License :: OSI Approved :: MIT No Attribution License (MIT-0)": "MIT-0",
    "License :: OSI Approved :: BSD License": "BSD-3-Clause",
    "License :: OSI Approved :: Apache Software License": "Apache-2.0",
    "License :: OSI Approved :: ISC License (ISCL)": "ISC",
    "License :: OSI Approved :: Python Software Foundation License": "PSF-2.0",
    "License :: OSI Approved :: The Unlicense (Unlicense)": "Unlicense",
    "License :: Public Domain": "LicenseRef-Public-Domain",
    "License :: OSI Approved :: GNU General Public License v2 (GPLv2)": "GPL-2.0-only",
    "License :: OSI Approved :: GNU General Public License v2 or later (GPLv2+)": "GPL-2.0-or-later",
    "License :: OSI Approved :: GNU General Public License v3 (GPLv3)": "GPL-3.0-only",
    "License :: OSI Approved :: GNU General Public License v3 or later (GPLv3+)": "GPL-3.0-or-later",
    "License :: OSI Approved :: GNU Lesser General Public License v2 or later (LGPLv2+)": "LGPL-2.0-or-later",
    "License :: OSI Approved :: GNU Lesser General Public License v3 (LGPLv3)": "LGPL-3.0-only",
    "License :: OSI Approved :: GNU Affero General Public License v3": "AGPL-3.0-only",
    "License :: OSI Approved :: Mozilla Public License 2.0 (MPL 2.0)": "MPL-2.0",
    "License :: OSI Approved :: Artistic License": "Artistic-1.0",
}

# packages the translator handles itself (framework, drivers): never translated from source, whatever they import
FRAMEWORK = {"fastapi", "starlette", "pydantic", "pydantic_core", "pydantic_settings", "sqlalchemy", "alembic",
             "typing_extensions", "typing_inspection", "annotated_types", "annotated_doc", "uvicorn", "anyio",
             "sniffio", "httpcore", "psycopg", "psycopg_binary", "psycopg_pool", "asyncpg",
             "greenlet", "mcp", "sse_starlette", "multipart", "python_multipart",
             "_virtualenv", "pip", "setuptools", "pkg_resources", "_distutils_hack", "wheel", "py2axum"}
# packages the library map covers in part: the modules of their source that stay translatable (the rest is native)
MIXED: dict[str, tuple[str, ...]] = {"dateutil": ("dateutil.relativedelta",)}
EXTENSIONS = (".so", ".pyd", ".dylib", ".dll")
# modules of the standard library translated from the source of the project's Python (Lib/), under the library map
# (closed list: no I/O, threads or interpreter state, nothing the library map covers). A module that imports a C
# accelerator (`bisect` and `_bisect`, whose messages differ from Lib/bisect.py: 3.15 says "lo cannot be negative",
# and on wrongly typed arguments in every version) is refused: CPython runs the C code, not the Python one.
STDLIB_SOURCE = {
    "html.entities": None,
    "gettext": None,  # NullTranslations; translation()/find() read .mo files and are refused where reached
    "colorsys": None,
    "keyword": None,
    # not yet: textwrap (statements in the class body), shlex (sys.stdin as a default), graphlib
    # (`__class_getitem__ = classmethod(GenericAlias)`)
}


def c_accelerator(tree: ast.Module) -> str | None:
    """A compiled module of the standard library a module imports (`from _bisect import *`, even under `try`): the
    code CPython runs in its place."""
    import importlib.util

    for n in ast.walk(tree):
        names = [a.name for a in n.names] if isinstance(n, ast.Import) else [n.module] if (
            isinstance(n, ast.ImportFrom) and not n.level and n.module) else []
        for m in names:
            if not m.split(".")[-1].startswith("_"):
                continue
            if m in sys.builtin_module_names:
                return m
            try:
                spec = importlib.util.find_spec(m)
            except (ImportError, ValueError):
                spec = None
            if spec is not None and str(spec.origin or "").endswith(EXTENSIONS):
                return m
    return None


def _norm(name: str) -> str:
    return re.sub(r"[-_.]+", "-", name).lower()


@dataclass
class Config:
    """The `--translate-libs`, `--venv` and `--allow-license` options of the current command."""

    mode: str = "none"  # none | auto | list
    dists: frozenset[str] = frozenset()  # normalised names when mode == "list"
    venv: Path | None = None
    licences: frozenset[str] = frozenset()


CONFIG = Config()


def add_flags(ap) -> None:
    ap.add_argument("--translate-libs", default="none", metavar="none|auto|DIST,...",
                    help="translate pure-Python libraries of the project's environment from their source (only the "
                         "functions the routes reach): none (default), auto (every pure-Python distribution under an "
                         "admitted licence the library map does not cover), or a comma-separated list of distributions")
    ap.add_argument("--venv", type=Path, default=None,
                    help="the project's virtual environment, whose site-packages --translate-libs reads (default: a "
                         ".venv at --root or above, else the environment py2axum runs in)")
    ap.add_argument("--allow-license", action="append", default=[], metavar="SPDX",
                    help="admit a licence for --translate-libs besides " + ", ".join(ADMITTED) + " (repeatable)")


def apply_flags(args) -> None:
    configure(getattr(args, "translate_libs", "none") or "none", getattr(args, "venv", None),
              getattr(args, "allow_license", None) or [])


def configure(mode: str, venv: Path | None = None, licences=()) -> None:
    global CONFIG
    mode = mode.strip()
    if mode in ("none", "auto"):
        CONFIG = Config(mode, frozenset(), venv, frozenset(licences))
    else:
        names = frozenset(_norm(x) for x in mode.split(",") if x.strip())
        if not names:
            raise ValueError("--translate-libs: expected none, auto or a comma-separated list of distributions")
        CONFIG = Config("list", names, venv, frozenset(licences))
    _INDEXES.clear()


def native_tops() -> set[str]:
    """The top-level packages the library map translates natively (libmap names and the frameworks)."""
    from . import libmap

    tops = set(FRAMEWORK)
    for nm in dir(libmap):
        v = getattr(libmap, nm)
        if isinstance(v, (dict, set, frozenset)):
            for k in v:
                if isinstance(k, str) and re.fullmatch(r"[A-Za-z_][\w.]*", k) and "." in k:
                    tops.add(k.split(".")[0])
    return tops


def mapped_names() -> frozenset[str]:
    """Every library name the library map translates natively (`jinja2.Environment`, `aiosmtplib.send`...)."""
    from . import libmap

    names = set()
    for t in (libmap.VALUES, libmap.EXCEPTIONS, libmap.CALLS, libmap.OBJECT_VALUES, libmap.NAMESPACE_CALLS, libmap.LOGURU):
        names.update(k for k in t if isinstance(k, str) and "." in k)
    return frozenset(names)


def stdlib_names() -> set[str]:
    return set(sys.stdlib_module_names)


# ---------------------------------------------------------------------------------------------- distributions


@dataclass
class Dist:
    name: str  # as published (`python-slugify`)
    version: str
    site: Path
    record: str  # the RECORD's text
    files: list[tuple[str, str]]  # (path relative to site-packages, "sha256=..." or "")
    tops: set[str]
    licence: str | None  # SPDX expression, or None when unknown
    licence_from: str
    licence_why: str = ""  # what was found when no licence was recognised
    refusal: str | None = None  # why it is not translated (None: translated)
    code: str = ""  # P2A07xx of the refusal

    @property
    def key(self) -> str:
        h = hashlib.sha256(self.record.encode()).hexdigest()[:16]
        py = ".".join(map(str, sys.version_info[:2]))
        return f"{_norm(self.name)}-{self.version}-{h}-{_p2a_version()}-{py}"


def _p2a_version() -> str:
    """py2axum's version, and a digest of the code that folds the trees (a development checkout changes it)."""
    try:
        from importlib.metadata import version

        v = version("py2axum")
    except Exception:  # noqa: BLE001 — run from a checkout
        v = "src"
    here = Path(__file__).resolve().parent
    h = hashlib.sha256(b"".join((here / f).read_bytes() for f in ("modules.py", "libsrc.py"))).hexdigest()[:8]
    return f"{v}+{h}"


def locked(root: Path, name: str) -> str | None:
    """The version of distribution `name` the project locks: its `uv.lock` (at --root or above, up to the
    repository's root), else an `==` pin of a requirements*.txt there; None when nothing pins it."""
    want = _norm(name)
    d = Path(root).resolve()
    for cand in (d, *d.parents):
        lock = cand / "uv.lock"
        if lock.is_file():
            for m in re.finditer(r'^\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"', lock.read_text(), re.M):
                if _norm(m.group(1)) == want:
                    return m.group(2)
            return None
        for r in sorted(cand.glob("requirements*.txt")):
            from .versions import read_requirements

            for line in read_requirements(r).splitlines():
                m = re.match(r"\s*([A-Za-z0-9][A-Za-z0-9._-]*)(?:\[[^\]]*\])?\s*==\s*([^\s;#,]+)", line)
                if m and _norm(m.group(1)) == want:
                    return m.group(2)
        if (cand / ".git").exists():
            break
    return None


# SPDX identifiers the older `License` field may carry (anything else, `BSD`, `BSD-like`, is free text)
_SPDX_ID = re.compile(
    r"(?:MIT(?:-0)?|0BSD|BSD-[1-4]-Clause(?:-[A-Za-z0-9.]+)*|Apache-[12]\.\d|ISC|PSF-2\.0|Python-2\.0(?:\.1)?|Unlicense|"
    r"Zlib|HPND(?:-[A-Za-z0-9.]+)*|A?L?GPL-[123]\.\d(?:-only|-or-later)?|MPL-[12]\.\d|EPL-[12]\.\d|CC0-1\.0|"
    r"CC-BY(?:-[A-Za-z0-9.]+)*|Artistic-[12]\.\d|BSL-1\.0|OFL-1\.1|LicenseRef-[A-Za-z0-9.-]+)\+?")
# classifiers that name no licence: they say nothing, and one of them hides what the text says
_VAGUE = ("License :: OSI Approved", "License :: Other/Proprietary License", "License :: DFSG approved",
          "License :: Free For Educational Use", "License :: Free For Home Use", "License :: Free for non-commercial use",
          "License :: Freely Distributable", "License :: Freeware", "License :: Repoze Public License")


def _spdx_field(old: str) -> bool:
    """`old` (the `License` field) is a syntactically valid SPDX expression of known identifiers."""
    toks = [t.strip("()") for t in old.split()]
    return bool(toks) and re.fullmatch(r"\(?[A-Za-z0-9.+-]+\)?(?:\s+(?:AND|OR|WITH)\s+\(?[A-Za-z0-9.+-]+\)?)*", old) \
        is not None and all(t in ("AND", "OR", "WITH") or _SPDX_ID.fullmatch(t) for t in toks)


def _classifiers(meta) -> str | None:
    cl = [c for c in (meta.get_all("Classifier") or []) if c.startswith("License ::")]
    if any(c.startswith(_VAGUE) and c != "License :: OSI Approved" for c in cl) and not all(c in CLASSIFIERS for c in cl):
        return None  # a vague classifier among the others: the text decides
    found = [CLASSIFIERS.get(c, f"LicenseRef-{c.rsplit('::', 1)[-1].strip()}") for c in cl]
    found = [x for x in dict.fromkeys(found) if x != "LicenseRef-OSI Approved"]
    if found:
        return " OR ".join(found) if len(found) > 1 else found[0]
    return None


def _licence_files(meta, files, locate) -> list[str]:
    """The text of the licence files a distribution ships in its `.dist-info` (`License-File`, else by name)."""
    out = []
    for rel, _ in files:
        parts = rel.split("/")
        if len(parts) >= 2 and parts[0].endswith(".dist-info") and \
                re.match(r"(?:licen[cs]e|copying|copyright|unlicense)", parts[-1], re.I):
            p = locate(rel)
            try:
                if p.is_file() and p.stat().st_size <= 200_000:
                    out.append(p.read_text(encoding="utf-8", errors="replace"))
            except OSError:
                pass
    return out


def _from_text(texts: list[str]) -> tuple[str | None, str]:
    """(SPDX expression, detail): the licences the files' texts consist of, or None and the unrecognised text."""
    from py2axum.licenses import recognise

    ids: list[str] = []
    for t in texts:
        found, left = recognise(t)
        if left:
            return None, left
        ids += found
    ids = list(dict.fromkeys(ids))
    return (" AND ".join(ids) if ids else None), ""


def _licence_ex(meta, texts=lambda: []) -> tuple[str | None, str, str]:
    """(SPDX expression or None, where it came from, what stopped the search when there is none).

    Order: `License-Expression`; the older `License` field when it is an SPDX expression; then, when that field is
    free text (`BSD`, `BSD-like`: nothing a machine can trust), the licence files compared with the canonical texts,
    else the licence classifiers; with no `License` field, the classifiers then the files."""
    expr = meta.get("License-Expression")
    if expr:
        return expr.strip(), "License-Expression", ""
    old = " ".join((meta.get("License") or "").split())
    if old and _spdx_field(old):
        return old, "License", ""
    detail = f"License field {old!r}" if old else ""
    if old:
        e, d = _from_text(texts())
        if e:
            return e, "licence file", ""
        c = _classifiers(meta)
        if c:
            return c, "classifiers", ""
        return None, "none", (f"{detail}; licence file: unrecognised text {d!r}" if d else
                              detail + "; no recognisable licence file or licence classifier")
    c = _classifiers(meta)
    if c:
        return c, "classifiers", ""
    e, d = _from_text(texts())
    if e:
        return e, "licence file", ""
    return None, "none", (f"licence file: unrecognised text {d!r}" if d else "")


def _licence(meta, texts=lambda: []) -> tuple[str | None, str]:
    e, src, _ = _licence_ex(meta, texts)
    return e, src


def licence_admitted(expr: str | None, extra=frozenset()) -> bool:
    """An SPDX expression whose terms are all admitted (AND), or one alternative of which is (OR)."""
    if not expr:
        return False
    ok = set(ADMITTED) | set(extra)
    e = expr.replace("(", " ( ").replace(")", " ) ")
    toks = e.split()

    def parse_or(i):
        v, i = parse_and(i)
        while i < len(toks) and toks[i].upper() == "OR":
            w, i = parse_and(i + 1)
            v = v or w
        return v, i

    def parse_and(i):
        v, i = atom(i)
        while i < len(toks) and toks[i].upper() == "AND":
            w, i = atom(i + 1)
            v = v and w
        return v, i

    def atom(i):
        if i >= len(toks) or toks[i].upper() in ("AND", "OR", "WITH", ")"):
            raise IndexError(i)  # a malformed expression: never admitted
        if toks[i] == "(":
            v, i = parse_or(i + 1)
            return v, i + 1
        t = toks[i]
        if i + 2 < len(toks) and toks[i + 1].upper() == "WITH":
            t = f"{t} WITH {toks[i + 2]}"
            return t in ok, i + 3
        return t in ok or t.removesuffix("+") in ok, i + 1

    try:
        v, i = parse_or(0)
    except IndexError:
        return False
    return v and i == len(toks)


def site_packages(root: Path) -> list[Path]:
    """The project's site-packages (see the module's docstring)."""
    venv = CONFIG.venv
    if venv is None:
        d = Path(root).resolve()
        for cand in (d, *d.parents):
            if (cand / ".venv").is_dir():
                venv = cand / ".venv"
                break
            if (cand / ".git").exists():
                break
    if venv is not None:
        venv = Path(venv).resolve()
        from .dyn import target_python

        py = "python{}.{}".format(*target_python(root))
        sites = sorted((venv / "lib").glob("python3*/site-packages")) + [venv / "Lib" / "site-packages"]
        sites = [s for s in sites if s.is_dir()]
        exact = [s for s in sites if s.parent.name == py]
        return exact or sites
    import site
    import sysconfig

    out = [Path(sysconfig.get_paths()["purelib"])]
    for s in getattr(site, "getsitepackages", lambda: [])():
        if Path(s) not in out:
            out.append(Path(s))
    return [p for p in out if p.is_dir()]


def _record_hash_ok(site: Path, rel: str, digest: str) -> bool:
    if not digest:
        return rel.endswith((".pyc", "RECORD", "INSTALLER", "REQUESTED")) or "/__pycache__/" in rel or rel.endswith("direct_url.json")
    algo, _, want = digest.partition("=")
    try:
        data = (site / rel).read_bytes()
    except OSError:
        return False
    got = base64.urlsafe_b64encode(hashlib.new(algo, data).digest()).rstrip(b"=").decode()
    return got == want


class LibIndex:
    """The distributions of the project's site-packages and the source modules they provide."""

    def __init__(self, root: Path):
        self.root = Path(root).resolve()
        self.sites = site_packages(self.root)
        self.native = native_tops()
        self.std = stdlib_names()
        self.by_top: dict[str, Dist] = {}
        self.dists: dict[str, Dist] = {}
        self.used: dict[str, set[str]] = {}  # dist -> modules loaded from it
        self.stdlib_dist: Dist | None = None  # the standard library's modules of STDLIB_SOURCE
        self.std_refused: dict[str, str] = {}
        self._mapped: frozenset[str] | None = None
        self._mapped_prefixes: set[str] = set()
        self._scan()

    def _scan(self) -> None:
        from importlib import metadata

        for d in metadata.distributions(path=[str(s) for s in self.sites]):
            name = d.metadata.get("Name")
            if not name or _norm(name) in self.dists:
                continue
            rec = d.read_text("RECORD") or ""
            files = []
            for line in rec.splitlines():
                parts = line.rsplit(",", 2)
                if len(parts) == 3:
                    files.append((parts[0], parts[1]))
            site = Path(str(d.locate_file("")))
            tops = set()
            tl = d.read_text("top_level.txt")
            if tl:
                tops = {x.strip() for x in tl.splitlines() if x.strip()}
            else:
                for p, _ in files:
                    head = p.split("/")[0]
                    if head.endswith(".dist-info") or head.endswith(".data") or head == "..":
                        continue
                    tops.add(head.removesuffix(".py"))
            lic, src, why = _licence_ex(d.metadata, lambda d=d, files=files: _licence_files(
                d.metadata, files, lambda rel, d=d: Path(str(d.locate_file(rel)))))
            dist = Dist(name, d.version, site, rec, files, tops, lic, src, why)
            self.dists[_norm(name)] = dist
            for t in tops:
                self.by_top.setdefault(t, dist)

    def selected(self, dist: Dist) -> bool:
        if CONFIG.mode == "auto":
            return True
        return CONFIG.mode == "list" and _norm(dist.name) in CONFIG.dists

    def native_module(self, name: str) -> bool:
        """`name` stays with the library map (or the frameworks): never read from source."""
        top = name.split(".")[0]
        if top in self.std:
            return name not in STDLIB_SOURCE
        if top in FRAMEWORK:
            return True
        if top in MIXED:
            return any(name == m or name.startswith(m + ".") for m in MIXED[top]) or name == top
        return top in self.native

    def check(self, dist: Dist) -> None:
        """Decide once whether a selected distribution may be translated (sets `refusal`)."""
        if dist.refusal is not None or dist.code == "ok":
            return
        ext = [p for p, _ in dist.files if p.endswith(EXTENSIONS)]
        if ext:
            dist.refusal, dist.code = (f"distribution {dist.name} {dist.version} is not pure Python (compiled "
                                       f"extension {ext[0]}): only pure-Python libraries are translated from source"), "P2A0702"
            return
        forced = dist.licence is None and (
            _norm(dist.name) in {_norm(x) for x in CONFIG.licences}
            or dist.version.startswith(ADMITTED_DISTS.get(_norm(dist.name), "\0")))
        if not forced and not licence_admitted(dist.licence, CONFIG.licences):
            what = dist.licence or "unknown (" + (dist.licence_why or "no License-Expression, licence file nor "
                                                  "licence classifier") + ")"
            dist.refusal, dist.code = (f"distribution {dist.name} {dist.version} has licence {what}, not admitted for "
                                       f"translation from source (admitted: {', '.join(ADMITTED)}; "
                                       f"--allow-license {'<SPDX>' if dist.licence else dist.name} admits "
                                       f"{'another' if dist.licence else 'it once you have read its licence'})"), "P2A0701"
            return
        pin = locked(self.root, dist.name)
        if pin is not None and pin != dist.version:
            dist.refusal, dist.code = (f"distribution {dist.name}: the project locks {pin} but {dist.version} is "
                                       f"installed in {dist.site}: install the locked version (uv sync --frozen)"), "P2A0703"
            return
        for rel, digest in dist.files:
            if rel.endswith(".py") and not _record_hash_ok(dist.site, rel, digest):
                dist.refusal, dist.code = (f"distribution {dist.name} {dist.version}: {rel} does not match its RECORD "
                                           f"hash (modified after installation): reinstall it"), "P2A0703"
                return
        dist.code = "ok"

    def partial(self, top: str) -> Dist | None:
        """The distribution of `top` when the library map covers only some of its names (`jinja2.Environment`, not
        `jinja2.Template`) and its source may be translated: the names the map does not cover are read from it."""
        if top in self.std or top in FRAMEWORK or top in MIXED or top not in self.native:
            return None
        dist = self.by_top.get(top)
        if dist is None or not self.selected(dist):
            return None
        self.check(dist)
        return None if dist.refusal else dist

    def mapped(self, dotted: str) -> bool:
        """`dotted` is a name of the library map, or a class whose attributes it maps (it stays native)."""
        if self._mapped is None:
            self._mapped = mapped_names()
            self._mapped_prefixes = {k.rpartition(".")[0] for k in self._mapped}
        return dotted in self._mapped or dotted in self._mapped_prefixes

    def mapped_of(self, top: str) -> list[str]:
        self.mapped(top)
        return sorted(k for k in self._mapped if k.split(".")[0] == top)

    def locate(self, name: str, partial: bool = False) -> tuple[Path, Path, Dist] | None:
        """The source file of library module `name` and its site-packages, when it is translated from source
        (`partial`: a module of a library the map covers in part, read for the names the map does not cover)."""
        if self.native_module(name) and not (partial and self.partial(name.split(".")[0]) is not None):
            return None
        top = name.split(".")[0]
        if top in self.std:
            return self.locate_stdlib(name)
        dist = self.by_top.get(top)
        if dist is None or not self.selected(dist):
            return None
        base = dist.site.joinpath(*name.split("."))
        path = None
        for cand in (base.with_suffix(".py"), base / "__init__.py"):
            if cand.is_file() and _exact_case(dist.site, cand):
                path = cand
                break
        if path is None:
            return None
        self.check(dist)
        if dist.refusal:
            return None
        self.used.setdefault(dist.name, set()).add(name)
        return path, dist.site, dist

    def locate_stdlib(self, name: str) -> tuple[Path, Path, Dist] | None:
        """A module of STDLIB_SOURCE, from the Lib/ of the Python py2axum runs on, when it is the project's minor
        (else refused where it is used: the library map has no equivalent)."""
        import sysconfig

        from .dyn import target_python

        if tuple(sys.version_info[:2]) != tuple(target_python(self.root)):
            self.std_refused[name] = (("the standard library module {} is translated from the source of the project's "
                                       "Python {}.{}: run py2axum on that Python").format(name, *target_python(self.root)),
                                      "P2A0704")
            return None
        lib = Path(sysconfig.get_paths()["stdlib"])
        base = lib.joinpath(*name.split("."))
        path = next((c for c in (base.with_suffix(".py"), base / "__init__.py") if c.is_file()), None)
        if path is None:
            return None
        accel = c_accelerator(ast.parse(path.read_bytes()))
        if accel is not None:
            self.std_refused[name] = (f"the standard library module {name} is not pure Python (compiled extension "
                                      f"{accel}, which CPython runs instead of its Lib/ code): not translated from source",
                                      "P2A0702")
            return None
        if self.stdlib_dist is None:
            ver = ".".join(map(str, sys.version_info[:3]))
            self.stdlib_dist = Dist("python-stdlib", ver, lib, f"{sys.version}\n{lib}", [], set(), "PSF-2.0", "Python")
            self.stdlib_dist.code = "ok"
        self.used.setdefault(self.stdlib_dist.name, set()).add(name)
        return path, lib, self.stdlib_dist

    def why_not(self, dotted: str) -> tuple[str, str] | None:
        """Why a library name stayed external although its distribution is installed: (message, code)."""
        top = dotted.split(".")[0]
        mod = next((m for m in self.std_refused if dotted == m or dotted.startswith(m + ".")), None)
        if mod is not None:
            return self.std_refused[mod]
        dist = self.by_top.get(top)
        in_part = (top in self.native and top not in self.std and top not in FRAMEWORK and top not in MIXED
                   and not self.mapped(dotted))
        if dist is None or (self.native_module(dotted.rpartition(".")[0] or dotted) and not in_part):
            return None
        if not self.selected(dist):
            return (f"{dist.name} {dist.version} is installed: `--translate-libs auto` (or `--translate-libs "
                    f"{_norm(dist.name)}`) translates it from its source"), ""
        self.check(dist)
        if dist.refusal:
            return dist.refusal, dist.code
        return None


def _exact_case(base: Path, path: Path) -> bool:
    cur = base
    for part in path.relative_to(base).parts:
        if part not in os.listdir(cur):
            return False
        cur = cur / part
    return True


_INDEXES: dict[str, LibIndex] = {}


def index(root: Path) -> LibIndex | None:
    """The library index of a project (None when --translate-libs is none)."""
    if CONFIG.mode == "none":
        return None
    k = str(Path(root).resolve())
    if k not in _INDEXES:
        _INDEXES[k] = LibIndex(Path(root))
    return _INDEXES[k]


# ---------------------------------------------------------------------------------------------- folding, cache


class _FoldPlatform(ast.NodeTransformer):
    """`sys.platform == "..."`, `sys.platform.startswith("...")` and `os.name == "..."` read against the platform
    py2axum runs on (the binary is built where it runs), and the `if` they decide keeps only its branch."""

    def __init__(self, tree):
        self.sys, self.os = set(), set()
        for n in ast.walk(tree):
            if isinstance(n, ast.Import):
                for a in n.names:
                    if a.name == "sys":
                        self.sys.add(a.asname or "sys")
                    if a.name == "os":
                        self.os.add(a.asname or "os")

    def _attr(self, e, mods, attr):
        return isinstance(e, ast.Attribute) and e.attr == attr and isinstance(e.value, ast.Name) and e.value.id in mods

    def _value(self, e):
        if self._attr(e, self.sys, "platform"):
            return sys.platform
        if self._attr(e, self.os, "name"):
            return os.name
        return None

    def visit_Compare(self, node):
        self.generic_visit(node)
        if len(node.ops) == 1 and isinstance(node.ops[0], (ast.Eq, ast.NotEq)):
            v, c = self._value(node.left), node.comparators[0]
            if v is not None and isinstance(c, ast.Constant) and isinstance(c.value, str):
                return _folded(node, (v == c.value) == isinstance(node.ops[0], ast.Eq))
        return node

    def visit_Call(self, node):
        self.generic_visit(node)
        f = node.func
        if (isinstance(f, ast.Attribute) and f.attr == "startswith" and self._value(f.value) is not None
                and len(node.args) == 1 and not node.keywords and isinstance(node.args[0], ast.Constant)
                and isinstance(node.args[0].value, (str, tuple))):
            return _folded(node, self._value(f.value).startswith(node.args[0].value))
        return node

    def visit_UnaryOp(self, node):
        self.generic_visit(node)
        if isinstance(node.op, ast.Not) and getattr(node.operand, "_py2axum_folded", False):
            return _folded(node, not node.operand.value)
        return node

    def visit_If(self, node):
        self.generic_visit(node)
        if getattr(node.test, "_py2axum_folded", False):
            kept = node.body if node.test.value else node.orelse
            return kept or [ast.copy_location(ast.Pass(), node)]
        return node

    def visit_IfExp(self, node):
        self.generic_visit(node)
        if getattr(node.test, "_py2axum_folded", False):
            return node.body if node.test.value else node.orelse
        return node


def _folded(like, value: bool):
    c = ast.copy_location(ast.Constant(value), like)
    c._py2axum_folded = True
    return c


def fold_platform(tree: ast.Module) -> ast.Module:
    if not any(isinstance(n, ast.Attribute) and n.attr in ("platform", "name") for n in ast.walk(tree)):
        return tree
    return ast.fix_missing_locations(_FoldPlatform(tree).visit(tree))


def cache_dir() -> Path:
    base = os.environ.get("XDG_CACHE_HOME")
    return (Path(base) if base else Path.home() / ".cache") / "py2axum" / "libs"


def parse_cached(dist: Dist, path: Path, python: tuple[int, int]):
    """The folded tree of a library file and the statements its version tests dropped, from the cache when the
    distribution, py2axum, Python and target Python are the same."""
    from .modules import fold_version_info

    rel = str(path.relative_to(dist.site))
    d = cache_dir() / dist.key / f"py{python[0]}.{python[1]}"
    f = d / (hashlib.sha256(rel.encode()).hexdigest()[:24] + ".pickle")
    try:
        with open(f, "rb") as fh:
            got = pickle.load(fh)  # noqa: S301 — our own cache, keyed by the RECORD's hash
        if got[0] == rel:
            return got[1], got[2]
    except (OSError, EOFError, pickle.UnpicklingError, IndexError, AttributeError, TypeError):
        pass
    tree = ast.parse(path.read_bytes(), filename=str(path))
    tree, dropped = fold_version_info(tree, python)
    tree = fold_platform(tree)
    from .modules import fold_flags

    tree = fold_flags(tree)
    try:
        d.mkdir(parents=True, exist_ok=True)
        tmp = f.with_suffix(f".{os.getpid()}.tmp")
        with open(tmp, "wb") as fh:
            pickle.dump((rel, tree, dropped), fh)
        tmp.replace(f)
    except OSError:
        pass
    return tree, dropped


def record_verdicts(ix: LibIndex, verdicts: dict[str, dict[str, str]]) -> None:
    """The verdict of each library function compiled ("ok" or its refusal code), next to the cached trees."""
    for name, fns in verdicts.items():
        dist = ix.dists.get(_norm(name))
        if dist is None:
            continue
        d = cache_dir() / dist.key
        try:
            d.mkdir(parents=True, exist_ok=True)
            f = d / "verdicts.json"
            old = json.loads(f.read_text()) if f.is_file() else {}
            old.update(fns)
            f.write_text(json.dumps(old, indent=1, sort_keys=True))
        except (OSError, ValueError):
            pass


@dataclass
class LibStats:
    """`py2axum check`: what a translated library costs."""

    name: str
    version: str
    licence: str
    ok: list[str] = field(default_factory=list)
    refused: list[tuple[str, str, str]] = field(default_factory=list)  # (function, code, message)
    lines: int = 0  # lines of the functions translated

    @property
    def gen_mb(self) -> float:
        # measured: ~0.5 KB of gen.rs per Python line reached (the traceback table included)
        return self.lines * 0.5 / 1024

    @property
    def peak_gib(self) -> float:
        # rustc's peak (thin LTO): ~0.53 GiB per MB of gen.rs
        return self.gen_mb * 0.53


def stats(proj) -> list[LibStats]:
    """What each library translated from source costs, from a compiled project: its functions reached (translated
    or refused, with the code), the lines translated, the estimated gen.rs and rustc peak. Records the verdicts in
    the cache."""
    ix = proj.ix.libs
    if ix is None:
        return []
    out: dict[str, LibStats] = {}
    verdicts: dict[str, dict[str, str]] = {}
    for key in list(proj.fns):
        sym = key[0]
        m = proj.ix.modules.get(getattr(sym, "module", None))
        if m is None or m.lib is None:
            continue
        dist = ix.stdlib_dist if m.lib == "python-stdlib" else ix.dists[_norm(m.lib)]
        st = out.setdefault(dist.name, LibStats(dist.name, dist.version, dist.licence or "?"))
        qual = f"{sym.module}.{sym.name}"
        e = proj.fn_errors.get(key)
        if e is not None:
            if qual not in {r[0] for r in st.refused}:
                st.refused.append((qual, e.code, e.render()))
            verdicts.setdefault(dist.name, {})[qual] = e.code
            continue
        if qual in st.ok:
            continue
        st.ok.append(qual)
        verdicts.setdefault(dist.name, {})[qual] = "ok"
        try:
            node = proj.fn_node(sym)
        except Exception:  # noqa: BLE001 — a synthetic function: no lines of its own
            node = None
        if node is not None and hasattr(node, "end_lineno"):
            st.lines += node.end_lineno - node.lineno + 1
    record_verdicts(ix, verdicts)
    for st in out.values():
        st.ok.sort()
        st.refused.sort()
    return sorted(out.values(), key=lambda s: s.name.lower())
