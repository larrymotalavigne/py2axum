"""A package whose `__init__` rebinds the names of its submodules (python-slugify, tenacity, email-validator do it):
`from .rebind import slug` (fixtures/resolveapp/main.py) is the function the package binds, not the submodule
`slug.py`: CPython reads the package's attribute first. `plain` is not rebound: it stays the submodule."""
from .slug import slug  # noqa: F401 (rebinds the submodule's name, as python-slugify does)
from .starred import *  # noqa: F403 (binds `starred`, the function, over the submodule)
