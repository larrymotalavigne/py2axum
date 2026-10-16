"""Module flags folded like `sys.version_info` (six's `PY3`), `@overload` signatures, sentinels and type-only
builtins, as libraries write them."""
import sys
from typing import overload

PY2 = sys.version_info[0] == 2
PY3 = sys.version_info[0] == 3
MODERN = sys.version_info >= (3, 12)

if PY3:
    string_types = (str,)
    text_type = str
else:
    string_types = (basestring,)  # noqa: F821 — never translated: PY3 is folded
    text_type = unicode  # noqa: F821

MISSING = object()


@overload
def norm(x: int) -> int: ...


@overload
def norm(x: str) -> str: ...


def norm(x):
    return x * 2


class Conv:
    @overload
    def to(self, x: int) -> str: ...

    @overload
    def to(self, x: bytes) -> str: ...

    def to(self, x):
        if isinstance(x, (bytes, bytearray)):
            return x.decode()
        return text_type(x)


def get(d, key, default=MISSING):
    v = d.get(key, MISSING)
    if v is MISSING:
        if default is MISSING:
            raise KeyError(key)
        return default
    return v


def info(s):
    return {"py3": PY3, "py2": PY2, "modern": MODERN, "str": isinstance(s, string_types), "ascii": s.isascii(),
            "big": sys.maxsize, "norm": norm(s), "conv": Conv().to(s.encode()), "bytearray": isinstance(s, bytearray),
            "obj": isinstance(MISSING, object), "same": MISSING is MISSING, "other": MISSING is object(),
            "get": get({"a": 1}, "a"), "dflt": get({}, "b", 7), "complex": isinstance(1, complex)}
