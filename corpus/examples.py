"""The FastAPI corpus: every `docs_src` module that defines an `app`, the documentation page that shows it,
and the official tests that exercise it.

An example is a module (`docs_src.body.tutorial001_py310`) or, for the multi-file apps, the `main` module of
a package (`docs_src.bigger_applications.app_an_py310.main`). It is translated from a copy of its own
(`<work>/pkgs/<id>/`, without the package's `test_*.py`), so that py2axum sees one application; the
file:line of a refusal is mapped back to the docs_src path.
"""
from __future__ import annotations

import ast
import json
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

SECTIONS = {"tutorial": "Tutorial - User Guide", "advanced": "Advanced User Guide", "how-to": "How To - Recipes"}


@dataclass
class Example:
    id: str                       # Rust-identifier-safe name: body__tutorial001_py310
    module: str                   # docs_src.body.tutorial001_py310
    source: str                   # docs_src/body/tutorial001_py310.py or docs_src/bigger_applications/app_an_py310
    package: bool                 # a multi-file app (a package)
    page: str = ""                # docs/en/docs/tutorial/body.md
    title: str = ""
    section: str = "Other"
    tests: list[str] = field(default_factory=list)  # official test files (tests/test_tutorial/...)
    kind: str = "fastapi"         # fastapi | pydantic
    requests: list = field(default_factory=list)    # pydantic: the generated requests


def _defines_app(path: Path) -> bool:
    try:
        tree = ast.parse(path.read_text())
    except SyntaxError:
        return False
    for node in tree.body:
        targets = node.targets if isinstance(node, ast.Assign) else [node.target] if isinstance(node, ast.AnnAssign) else []
        if any(isinstance(t, ast.Name) and t.id == "app" for t in targets):
            return True
    return False


def ident(module: str) -> str:
    return re.sub(r"[^a-z0-9_]", "_", module.removeprefix("docs_src.").replace(".", "__").lower())


def discover(repo: Path) -> list[Example]:
    out = []
    for path in sorted((repo / "docs_src").rglob("*.py")):
        rel = path.relative_to(repo)
        if path.name.startswith("test_") or path.name == "__init__.py" or not _defines_app(path):
            continue
        module = ".".join(rel.with_suffix("").parts)
        parts = rel.parts  # docs_src/<dir>/<file> or docs_src/<dir>/<pkg>/.../<file>
        package = len(parts) > 3
        source = "/".join(parts[:3]) if package else str(rel)
        out.append(Example(ident(module), module, source, package))
    return out


def _variant_key(name: str) -> str:
    """tutorial001_an_py310 -> tutorial001: the variants of one example share its page."""
    return re.sub(r"(_an)?(_py3\d+)?$", "", Path(name).stem)


def _nav(repo: Path) -> list[str]:
    import yaml

    class Loader(yaml.SafeLoader):
        pass

    Loader.add_multi_constructor("", lambda loader, suffix, node: None)
    data = yaml.load((repo / "docs" / "en" / "mkdocs.yml").read_text(), Loader=Loader)
    pages: list[str] = []

    def walk(n):
        if isinstance(n, str):
            pages.append(n)
        elif isinstance(n, list):
            for x in n:
                walk(x)
        elif isinstance(n, dict):
            for v in n.values():
                walk(v)

    walk(data.get("nav", []))
    return pages


def assign_pages(repo: Path, examples: list[Example]) -> list[str]:
    """Each example's documentation page: the first page (in navigation order) that includes its file, one of
    its variants, or a file of its package; else of its directory. Returns the navigation order."""
    docs = repo / "docs" / "en" / "docs"
    nav = [p for p in _nav(repo) if (docs / p).exists()]
    order = {p: i for i, p in enumerate(nav)}
    by_file: dict[tuple[str, str], str] = {}
    by_dir: dict[str, str] = {}
    titles: dict[str, str] = {}
    for md in sorted(docs.rglob("*.md"), key=lambda p: order.get(str(p.relative_to(docs)), 10**6)):
        page = str(md.relative_to(docs))
        text = md.read_text()
        m = re.search(r"^#\s+(.+)$", text, re.M)
        titles[page] = re.sub(r"\s*\{.*\}\s*$", "", m.group(1)).strip() if m else page
        for ref in re.findall(r"docs_src/([A-Za-z0-9_]+)/([A-Za-z0-9_./]+)", text):
            d, rest = ref
            first = rest.split("/")[0]
            by_file.setdefault((d, _variant_key(first)), page)
            by_dir.setdefault(d, page)
    for ex in examples:
        parts = ex.source.split("/")
        d, first = parts[1], parts[2]
        page = by_file.get((d, _variant_key(first))) or by_dir.get(d, "")
        ex.page = page
        ex.title = titles.get(page, "(no page)")
        ex.section = SECTIONS.get(page.split("/")[0], "Other") if page else "Other"
    return nav


def discover_tests(repo: Path, corpus_dir: Path, out: Path) -> dict[str, set[str]]:
    """module -> official test files that build a client for it (the tests run once, sending nothing)."""
    if out.exists():
        out.unlink()
    env = dict(os.environ, CORPUS_MODE="discover", CORPUS_OUT=str(out), PYTHONPATH=f"{corpus_dir}{os.pathsep}{repo}")
    subprocess.run([sys.executable, "-W", "ignore", "-m", "pytest", "-p", "replay", "-q", "--no-header",
                    "-p", "no:cacheprovider", "tests/test_tutorial"], cwd=repo, env=env, capture_output=True, text=True)
    found: dict[str, set[str]] = {}
    for line in out.read_text().splitlines():
        e = json.loads(line)
        if e["ev"] == "client" and e["module"] and (e["test"] or e.get("file")):
            found.setdefault(e["module"], set()).add(e["test"].split("::")[0] if e["test"] else e["file"])
    return found


def stage(repo: Path, ex: Example, pkgs: Path) -> Path:
    """The example's own package for py2axum (`<pkgs>/<id>/`); rewritten only when its sources changed."""
    dest = pkgs / ex.id
    src = repo / ex.source
    files: dict[str, bytes] = {"__init__.py": b""}
    if ex.package:
        for p in sorted(src.rglob("*.py")):
            if not p.name.startswith("test_"):
                files[str(p.relative_to(src))] = p.read_bytes()
    else:
        files[src.name] = src.read_bytes()
    current = {str(p.relative_to(dest)): p.read_bytes() for p in dest.rglob("*.py")} if dest.exists() else {}
    if current != files:
        if dest.exists():
            shutil.rmtree(dest)
        for rel, data in files.items():
            (dest / rel).parent.mkdir(parents=True, exist_ok=True)
            (dest / rel).write_bytes(data)
    return dest


def unstage(text: str, ex: Example, pkgs: Path) -> str:
    """A message about the staged copy, pointing at the documentation's file instead."""
    base = str((pkgs / ex.id).resolve())
    src_dir = ex.source if ex.package else ex.source.rsplit("/", 1)[0]
    return text.replace(base + "/", src_dir + "/").replace(str(pkgs / ex.id) + "/", src_dir + "/")
