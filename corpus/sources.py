"""The corpus sources, fetched at run time and never vendored: the FastAPI repository at the highest
supported FastAPI version (its `docs_src/` example apps, `tests/test_tutorial/` official tests and
`docs/en/docs/` pages) and the Pydantic repository at the highest supported Pydantic version (its `docs/`
pages and their code blocks), and the SQLAlchemy repository at the highest supported SQLAlchemy version (its
`doc/build/` pages and the docstrings of `lib/sqlalchemy/ext/`). All three are MIT-licensed (corpus/README.md).
"""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from py2axum.versions import SUPPORTED  # noqa: E402

CACHE = Path(__file__).resolve().parent / ".cache"
REPOS = {"fastapi": ("https://github.com/fastapi/fastapi.git", "{v}"),
         "pydantic": ("https://github.com/pydantic/pydantic.git", "v{v}"),
         "sqlalchemy": ("https://github.com/sqlalchemy/sqlalchemy.git", "rel_{v_}")}


def version(name: str) -> str:
    """The highest tested version of `name` (py2axum/versions.py)."""
    return SUPPORTED[name][1]


def fetch(name: str, dest: Path | None = None) -> Path:
    """A shallow clone of `name` at its highest supported version (reused when already there)."""
    url, tag = REPOS[name]
    tag = tag.format(v=version(name), v_=version(name).replace(".", "_"))
    dest = dest or CACHE / f"{name}-{tag}"
    if not (dest / ".git").exists():
        dest.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "-c", "advice.detachedHead=false", "clone", "-q", "--depth", "1", "--branch", tag,
                        url, str(dest)], check=True)
    return dest


if __name__ == "__main__":
    for n in sys.argv[1:] or REPOS:
        print(fetch(n))
