"""Behaviours and messages that differ between the Pythons py2axum supports (3.12 to 3.15): the binary follows
the project's Python (docs/python-versions.md). Valid syntax on every one of them; the 3.14 and 3.15 syntax is
in py314.py and py315.py, imported only under a `sys.version_info` test."""
import asyncio
import base64
import contextvars
import threading
from collections import Counter, defaultdict
from decimal import Decimal
import json
import re
import secrets
import sys
import traceback
from datetime import UTC, date, datetime
from pathlib import Path

from fastapi import APIRouter
from fastapi.responses import FileResponse

router = APIRouter()

CV = contextvars.ContextVar("pyver_cv", default=0)


def unpack_list():
    a, _ = [1, 2, 3]
    return a


def unpack_tuple():
    a, _ = (1, 2, 3)
    return a


def unpack_str():
    a, _ = "abc"
    return a


def dict_update():
    d = {}
    d.update([1])
    return d


def token_with():
    # Python 3.14: a ContextVar token is a context manager (TypeError before)
    with CV.set(7):
        inner = CV.get()
    return [inner, CV.get()]


def notes():
    try:
        raise ValueError("boom")
    except ValueError as e:
        e.add_note("first")
        e.add_note("second")
        return [e.__notes__, traceback.format_exception_only(e)]


def path_copy():
    # Python 3.14: Path.copy() and Path.move() (AttributeError before)
    src = Path("/tmp/py2axum_pyver_src.txt")
    src.write_text("hello")
    dst = src.copy(Path("/tmp/py2axum_pyver_dst.txt"))
    moved = dst.move(Path("/tmp/py2axum_pyver_moved.txt"))
    return [dst.name, moved.read_text(), dst.exists()]


CASES = {
    # ZeroDivisionError: "division by zero" for every operator from 3.14 (float modulo reworded in 3.13 first)
    "int_truediv": lambda n, x: n / 0,
    "int_floordiv": lambda n, x: n // 0,
    "int_mod": lambda n, x: n % 0,
    "int_divmod": lambda n, x: divmod(n, 0),
    "float_truediv": lambda n, x: x / 0,
    "float_floordiv": lambda n, x: x // 0,
    "float_mod": lambda n, x: x % 0,
    "float_divmod": lambda n, x: divmod(x, 0),
    "zero_pow": lambda n, x: 0.0 ** -1,
    # unpacking: 3.14 counts the items of a list or tuple
    "unpack_list": lambda n, x: unpack_list(),
    "unpack_tuple": lambda n, x: unpack_tuple(),
    "unpack_str": lambda n, x: unpack_str(),
    # dict(seq): 3.14 raises the element's TypeError, with a note
    "dict_seq": lambda n, x: dict([n]),
    "dict_update": lambda n, x: dict_update(),
    "dict_len": lambda n, x: dict([(1, 2, 3)]),
    # format specs: 3.14 groups the fractional part (`.6_f`), an error before
    "fmt_frac": lambda n, x: f"{x:.6_f}",
    "fmt_frac_comma": lambda n, x: format(x, ".4,f"),
    "fmt_group": lambda n, x: f"{1234567:_}|{1234567:,}|{255:_x}|{255:#_b}|{x:_.2f}",
    "fmt_comma_x": lambda n, x: format(255, ",x"),
    "fmt_code": lambda n, x: format("a", "d"),
    "fmt_list": lambda n, x: format([1], "5"),
    # argument checks of str/bytes methods (3.13: argument clinic), str.replace(count=) (3.13), bytes (3.15)
    "str_count0": lambda n, x: "a".count(),
    "str_find0": lambda n, x: "a".find(),
    "str_replace1": lambda n, x: "a".replace("a"),
    "str_replace_kw": lambda n, x: "aaa".replace("a", "b", count=2),
    "bytes_replace": lambda n, x: b"aaa".replace(b"a", b"b", 2).decode(),
    "bytes_replace_empty": lambda n, x: b"abc".replace(b"", b"-").decode(),
    "bytes_replace_kw": lambda n, x: b"aaa".replace(b"a", b"b", count=2).decode(),
    "bytes_count0": lambda n, x: b"a".count(),
    "list_count0": lambda n, x: [1].count(),
    # exception notes: add_note, and json's notes (3.14)
    "notes": lambda n, x: notes(),
    "json_notes": lambda n, x: json.dumps({"a": [1, {2}]}),
    "json_notes_tuple": lambda n, x: json.dumps([[1, (2, {3})]]),
    # 3.14 additions
    "token_with": lambda n, x: token_with(),
    "path_copy": lambda n, x: path_copy(),
    # re: `\z` (3.14, a bad escape before), `\B` on an empty string (matches from 3.14)
    "re_z": lambda n, x: bool(re.search(r"b\z", "ab")),
    "re_B_empty": lambda n, x: re.findall(r"\B", ""),
    "re_B": lambda n, x: re.findall(r"\B.", "ab cd"),
    # base64: urlsafe_b64decode without padding (3.15), base32's messages (3.15's C decoder)
    "b64_unpadded": lambda n, x: base64.urlsafe_b64decode("YWJjYQ").decode(),
    "b64_unpadded2": lambda n, x: base64.urlsafe_b64decode("YQ=a").hex(),
    "b64_one_more": lambda n, x: base64.urlsafe_b64decode("YWJjY"),
    "b64_padded_std": lambda n, x: base64.b64decode("YWJjYQ"),
    # padding rewritten in the security releases 3.13.13 and 3.14.4: data after it is decoded, not ignored
    "b64_after_padding": lambda n, x: base64.b64decode("aGk=aGk=").hex(),
    "b64_after_padding2": lambda n, x: base64.b64decode("aGk=a").hex(),
    "b64_excess_padding": lambda n, x: base64.b64decode("aGk===").hex(),
    "b64_strict_excess": lambda n, x: base64.b64decode("aGk=aGk=", validate=True).hex(),
    "b32_short": lambda n, x: base64.b32decode("JBSWY3D"),
    "b32_lower": lambda n, x: base64.b32decode("jbswy3dp"),
    "b32_excess": lambda n, x: base64.b32decode("JBSWY3DP="),
    "b32_leading": lambda n, x: base64.b32decode("========"),
    "b32_partial": lambda n, x: base64.b32decode("JBSW====").decode(),
    "b32_inner": lambda n, x: base64.b32decode("JBS=Y3DP"),
    "b32_map01": lambda n, x: base64.b32decode("JBSWY3D1", map01="L").hex(),
    "b32_casefold": lambda n, x: base64.b32decode("jbswy3dp", casefold=True).decode(),
    "b32_two": lambda n, x: base64.b32decode("JB"),
    # datetime.strptime: `%d` without a year (an error from 3.15), `%e` (3.13)
    "strptime_d": lambda n, x: datetime.strptime("09", "%d").isoformat(),
    "strptime_dm": lambda n, x: datetime.strptime("09 10", "%d %m").isoformat(),
    "strptime_dy": lambda n, x: datetime.strptime("09 2026", "%d %Y").isoformat(),
    "strptime_e": lambda n, x: datetime.strptime("Oct  9 2026", "%b %e %Y").isoformat(),
    # range messages of the date constructors (3.14 adds the value)
    "date_month": lambda n, x: date(2026, 13, 1).isoformat(),
    "date_day": lambda n, x: date(2025, 2, 29).isoformat(),
    # os.urandom (behind secrets): reworded in 3.15
    "token_negative": lambda n, x: secrets.token_hex(-1),
    # Counter ^ Counter (3.15), and the type named by an operator error
    "counter_xor": lambda n, x: dict(Counter(a=1, b=2) ^ Counter(a=3, c=1)),
    "counter_add_int": lambda n, x: Counter(a=1) + 1,
    "defaultdict_add_int": lambda n, x: defaultdict(int) + 1,
    # datetime.fromtimestamp: 3.14 rewords the error, 3.15 accepts any real number
    "fromtimestamp_decimal": lambda n, x: datetime.fromtimestamp(Decimal("1.5"), UTC).isoformat(),
    "fromtimestamp_str": lambda n, x: datetime.fromtimestamp("1"),
    # %-formatting: 3.15 names the argument and the position in its errors
    "pct_ok": lambda n, x: "%x|%X|%o|%c|%c|%5.1f|%-4d|%i" % (255, 255, 8, 65, "z", x, n, True),
    "pct_few": lambda n, x: "%s %s" % ("a",),  # noqa: F501, F502, F507, F509 — the errors are the cases
    "pct_many": lambda n, x: "%s" % ("a", "b"),  # noqa: F501, F502, F507, F509 — the errors are the cases
    "pct_d": lambda n, x: "%s %d" % ("a", "b"),
    "pct_d_single": lambda n, x: "%d" % "x",
    "pct_i": lambda n, x: "%i" % (None,),
    "pct_x": lambda n, x: "%x" % ("x",),
    "pct_o_float": lambda n, x: "%o" % (1.5,),
    "pct_c": lambda n, x: "%c" % ("ab",),
    "pct_f": lambda n, x: "%f" % ("x",),
    "pct_mapping": lambda n, x: "%(a)s" % ("x",),  # noqa: F501, F502, F507, F509 — the errors are the cases
    "pct_key": lambda n, x: "%(a)d" % {"a": "s"},
    "pct_unsupported": lambda n, x: "x%y" % 1,  # noqa: F501, F502, F507, F509 — the errors are the cases
    "pct_stray": lambda n, x: "abc%" % (1,),  # noqa: F501, F502, F507, F509 — the errors are the cases
    "pct_key_incomplete": lambda n, x: "%(a" % {"a": 1},  # noqa: F501, F502, F507, F509 — the errors are the cases
    # sys.version_info: read against the project's Python at translation
    "version": lambda n, x: [sys.version_info.major, sys.version_info.minor, list(sys.version_info[:2]),
                             sys.version_info >= (3, 14), sys.version_info < (3, 13), sys.version_info[0] == 3],
}


@router.get("/pyver/{name}")
async def pyver(name: str, n: int = 5, x: float = 1234.56789):
    try:
        return {"r": CASES[name](n, x)}
    except Exception as e:
        # re.error is named PatternError from 3.13: the class name is left out
        kind = "re.error" if isinstance(e, re.error) else type(e).__name__
        return {"t": kind, "e": str(e), "notes": getattr(e, "__notes__", None)}


@router.get("/pyver/mime/{name}")
async def mime(name: str):
    # Starlette guesses the type with mimetypes: CPython's table changed in 3.13, 3.14 and 3.15, and the host's
    # mime.types files (/etc/mime.types...) are read over it
    p = Path("/tmp/py2axum_pyver_mime.bin")
    if not p.exists():  # written once: the ETag and Last-Modified both servers send come from its mtime
        p.write_bytes(b"x")
    return FileResponse(p, filename=name)


LOCK = threading.Lock()


@router.get("/pyver/cm/{kind}")
async def context_managers(kind: str):
    # the TypeError of a value without the protocol used: 3.14 names the missing method, and the other protocol
    try:
        if kind == "async-lock":
            async with LOCK:
                return {"r": "entered"}
        if kind == "sync-semaphore":
            with asyncio.Semaphore(1):
                return {"r": "entered"}
        async with 1:
            return {"r": "entered"}
    except TypeError as e:
        return {"t": type(e).__name__, "e": str(e)}

