"""errno, sys constants, collections.abc, warnings, sys.version_info chains, str.join(chain(...)), import_module."""
import bisect
import errno
import importlib
import itertools
import sys
import warnings
from collections import Counter, defaultdict, deque
from collections import abc as cabc
from collections.abc import Mapping, MutableMapping, MutableSequence, MutableSet, Sequence, Set
from itertools import chain
from typing import NamedTuple

from fastapi import APIRouter

router = APIRouter(prefix="/s")

# folded at translation against the project's Python: chained comparisons and boolean chains
PY2 = sys.version_info < (3, 0)
OLD = (3, 0) <= sys.version_info < (3, 2)
if sys.version_info < (2, 6) or (3, 0) <= sys.version_info < (3, 2):
    raise RuntimeError("unsupported Python")
MODERN = (3, 8) <= sys.version_info < (4, 0) and True


class Pt(NamedTuple):
    x: int
    y: int


@router.get("/errno")
async def errno_values():
    return {"v": [errno.EPERM, errno.ENOENT, errno.ESRCH, errno.EINTR, errno.EIO, errno.ENXIO, errno.E2BIG, errno.ENOEXEC, errno.EBADF, errno.ECHILD, errno.ENOMEM, errno.EACCES, errno.EFAULT, errno.ENOTBLK, errno.EBUSY, errno.EEXIST, errno.EXDEV, errno.ENODEV, errno.ENOTDIR, errno.EISDIR, errno.EINVAL, errno.ENFILE, errno.EMFILE, errno.ENOTTY, errno.ETXTBSY, errno.EFBIG, errno.ENOSPC, errno.ESPIPE, errno.EROFS, errno.EMLINK, errno.EPIPE, errno.EDOM, errno.ERANGE],
            "matches": [errno.ENOENT == 2, errno.EEXIST == 17]}


@router.get("/sys")
async def sys_values():
    return {"byteorder": sys.byteorder, "maxunicode": sys.maxunicode, "warnoptions": sys.warnoptions,
            "fold": [PY2, OLD, MODERN], "chain": [(3, 0) <= sys.version_info < (4, 0), sys.version_info >= (3, 8)]}


def _value(kind: str):
    if kind == "defaultdict":
        d = defaultdict(list)
        d["a"].append(1)
        return d
    return {
        "dict": {"a": 1}, "list": [1], "tuple": (1, 2), "str": "ab", "bytes": b"ab", "set": {1}, "int": 3, "none": None,
        "deque": deque([1, 2]), "counter": Counter("aab"), "named": Pt(1, 2),
        "float": 1.5, "bool": True, "empty": {}, "frozen": frozenset([1]),
    }[kind]


@router.get("/abc/{kind}")
async def abc_matrix(kind: str):
    v = _value(kind)
    return {"v": [isinstance(v, Mapping), isinstance(v, MutableMapping), isinstance(v, Sequence),
                  isinstance(v, MutableSequence), isinstance(v, Set), isinstance(v, MutableSet),
                  isinstance(v, cabc.Mapping), isinstance(v, (cabc.Sequence, cabc.Set))]}


def _merge(*dicts):
    if len(dicts) == 1 and not isinstance(dicts[0], Mapping):
        dicts = dicts[0]
    out = {}
    for d in dicts:
        out.update(d)
    return out


@router.get("/abc-merge")
async def abc_merge():
    return {"a": _merge({"a": 1}, {"b": 2}), "b": _merge([{"a": 1}, {"c": 3}]), "c": _merge({"z": 0})}


class MyDeprecation(DeprecationWarning):
    pass


def _old(x):
    warnings.warn("old() is deprecated", DeprecationWarning, stacklevel=2)
    return x + 1


@router.get("/warn")
async def warn_values():
    warnings.warn("pending", PendingDeprecationWarning)
    warnings.warn(MyDeprecation("subclass"))
    warnings.warn(category=ImportWarning, message="import")
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        warnings.filterwarnings("ignore", message="x")
        warnings.warn("inside", DeprecationWarning)
    return {"v": _old(1), "classes": [issubclass(MyDeprecation, DeprecationWarning), issubclass(DeprecationWarning, Warning),
                                       issubclass(UserWarning, Exception), issubclass(ResourceWarning, Warning)]}


@router.get("/warn-catch")
async def warn_catch():
    try:
        raise DeprecationWarning("raised")
    except Warning as e:
        return {"err": type(e).__name__, "msg": str(e), "repr": repr(e)}


@router.get("/chain/{n}")
async def chain_join(n: int):
    a = [str(i) for i in range(n)]
    return {"a": "-".join(chain(a, ["x", "y"])), "b": ",".join(itertools.chain(map(str, range(n)), a)),
            "c": "".join(chain.from_iterable([a, a])), "d": ";".join(chain()), "e": b"|".join(chain([b"a"], [b"b"]))}


@router.get("/chain-bad")
async def chain_bad():
    try:
        return {"v": "-".join(chain(["a"], [1]))}
    except TypeError as e:
        return {"err": str(e)}


@router.get("/import/{name}")
async def import_it(name: str):
    return {"v": importlib.import_module("json").dumps({"n": name}), "loads": importlib.import_module("json").loads('{"a": [1, 2]}')}


@router.get("/bisect/{x}")
async def bisect_it(x: int, lo: int = 0, hi: int | None = None):
    a = [1, 3, 3, 5, 8]
    b = [10, 30, 30]
    out = {"l": bisect.bisect_left(a, x, lo, hi), "r": bisect.bisect_right(a, x, lo, hi), "d": bisect.bisect(a, x),
           "k": bisect.bisect_left(a, x * 2, key=lambda v: v * 2), "kw": bisect.bisect_right(a, x, hi=3)}
    bisect.insort(b, x)
    bisect.insort_left(b, x, key=lambda v: -v) if False else bisect.insort_left(b, x)
    bisect.insort_right(a, x, key=lambda v: v)
    out["a"], out["b"] = a, b
    return out


@router.get("/bisect-bad/{kind}")
async def bisect_bad(kind: str):
    try:
        if kind == "type":
            return {"v": bisect.bisect_left([1, 2], "a")}
        if kind == "typer":
            return {"v": bisect.bisect_right([1, 2], "a")}
        if kind == "lo":
            return {"v": bisect.bisect_left([1, 2], 1, -1)}
        if kind == "hi":
            return {"v": bisect.bisect_left([1, 2], 1, 0, 9)}
        if kind == "seq":
            return {"v": bisect.bisect_left(5, 1)}
        return {"v": bisect.bisect_left([1, 2], 1, "a")}
    except Exception as e:
        return {"err": type(e).__name__, "msg": str(e)}
