"""Static import resolution (py2axum/modules.py) against what CPython binds."""
import textwrap

import pytest

from py2axum.modules import Ext, ModRef, ModuleIndex, Sym

FILES = {
    "proj/__init__.py": "",
    "proj/main.py": "",
    # the package rebinds its submodules' names (python-slugify, tenacity, email-validator)
    "proj/lib/__init__.py": """
        from .slug import slug
        from .starred import *
        from .hidden_all import *
        from os import path as joined
    """,
    "proj/lib/slug.py": "def slug(t):\n    return t\n",
    "proj/lib/starred.py": "def starred():\n    return 1\n",
    "proj/lib/hidden_all.py": "__all__ = ['other']\n\n\ndef hidden_all():\n    pass\n\n\ndef other():\n    pass\n",
    "proj/lib/plain.py": "def helper():\n    return 1\n",
    "proj/lib/joined.py": "",
    # a package whose `__init__` star-imports an external module: whether it exports `sub` is unknown
    "proj/ext/__init__.py": "from json import *\n",
    "proj/ext/sub.py": "",
    # a namespace package (no `__init__.py`)
    "proj/ns/mod.py": "",
}


@pytest.fixture
def index(tmp_path):
    for rel, src in FILES.items():
        p = tmp_path / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(textwrap.dedent(src))
    return ModuleIndex(tmp_path, tmp_path / "proj")


@pytest.mark.parametrize("module, name, expected", [
    ("proj.lib", "slug", Sym("proj.lib.slug", "slug")),  # `from .slug import slug` wins over the submodule
    ("proj.lib", "starred", Sym("proj.lib.starred", "starred")),  # bound by `from .starred import *`
    ("proj.lib", "hidden_all", ModRef("proj.lib.hidden_all")),  # not in its `__all__`: the submodule
    ("proj.lib", "plain", ModRef("proj.lib.plain")),  # not rebound
    ("proj.lib", "joined", Ext("os.path")),  # rebound to an external name
    ("proj.ext", "sub", ModRef("proj.ext.sub")),  # an external star does not shadow a submodule
    ("proj.ns", "mod", ModRef("proj.ns.mod")),
])
def test_from_package_import(index, module, name, expected):
    assert index._spec(("from", module, name), set()) == expected
