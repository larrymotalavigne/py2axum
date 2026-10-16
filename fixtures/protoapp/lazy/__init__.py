"""Names imported lazily by the package's `__getattr__` (PEP 562), as stripe does."""
from importlib import import_module

_import_map = {
    "Greeter": ("fixtures.protoapp.lazy._greet", False),
    "tools": ("fixtures.protoapp.lazy.tools", True),
}


def __getattr__(name):
    try:
        target, is_submodule = _import_map[name]
        module = import_module(target)
        if is_submodule:
            return module
        return getattr(module, name)
    except KeyError:
        raise AttributeError(name) from None
