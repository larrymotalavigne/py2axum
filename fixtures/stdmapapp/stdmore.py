"""More standard library the translated libraries reach: urllib.parse.parse_qs/parse_qsl, array.array,
unicodedata.category/bidirectional/decomposition/name, ipaddress.IPv4Address/IPv6Address and AddressValueError,
functions defined in a module-level `if` (olefile), names unpacked from a bytes literal (idna), and
dict.fromkeys(chain(...))."""
import array
import bisect
import ipaddress
import unicodedata
from itertools import chain
from urllib.parse import parse_qs, parse_qsl

from fastapi import APIRouter

router = APIRouter(prefix="/m")

ST_A, ST_B, ST_C = b"VMD"
LETTERS_A, LETTERS_B = "xy"

if bytes is str:
    def first(c):
        return ord(c)
else:
    def first(c):
        return c if c.__class__ is int else c[0]


def _err(e: Exception) -> dict:
    return {"error": type(e).__name__, "msg": str(e)}


QS = {
    "plain": ("a=1&b=2&a=3", False, False, None, "&"),
    "blank": ("a=&b=2&&c", False, False, None, "&"),
    "keep": ("a=&b=2&&c", True, False, None, "&"),
    "strict": ("a=1&&b=2", False, True, None, "&"),
    "strict-noeq": ("a=1&b", False, True, None, "&"),
    "plus": ("q=caf%C3%A9+au+lait&x=%zz&y=%E9", False, False, None, "&"),
    "semi": ("a=1;b=2&c=3", False, False, None, ";"),
    "max": ("a=1&b=2&c=3", False, False, 2, "&"),
    "max-ok": ("a=1&b=2", False, False, 2, "&"),
    "badsep": ("a=1", False, False, None, ""),
    "empty": ("", False, False, None, "&"),
}


@router.get("/qs/{case}")
async def qs(case: str):
    text, keep, strict, maxf, sep = QS[case]
    try:
        return {"qs": parse_qs(text, keep_blank_values=keep, strict_parsing=strict, max_num_fields=maxf, separator=sep),
                "qsl": [list(p) for p in parse_qsl(text, keep, strict, max_num_fields=maxf, separator=sep)]}
    except ValueError as e:
        return _err(e)


@router.get("/qs-none")
async def qs_none():
    return {"qs": parse_qs(None), "qsl": parse_qsl(None)}


@router.get("/array/{tc}")
async def arr(tc: str, v: int = 0):
    out = {}
    try:
        a = array.array(tc, [1, 2, v])
        out["ok"] = {"repr": repr(a), "list": a.tolist(), "itemsize": a.itemsize, "typecode": a.typecode,
                     "len": len(a), "last": a[-1], "in": v in a, "bytes": a.tobytes().hex()}
    except (OverflowError, TypeError, ValueError) as e:
        out["error"] = _err(e)
    return out


@router.get("/array-ops")
async def arr_ops():
    a = array.array("I", (5, 1, 9))
    b = array.array("I")
    b.frombytes(a.tobytes())
    out = {"eq": a == b, "eq_i": a == array.array("i", [5, 1, 9]), "eq_list": a == [5, 1, 9], "empty": repr(array.array("d")),
           "floats": repr(array.array("d", [1.5, 2])), "f32": array.array("f", [0.1]).tolist()}
    s = array.array("H", [1, 5, 9, 13])
    out["bisect"] = [bisect.bisect_right(s, 5), bisect.bisect_left(s, 6)]
    s.append(20)
    s.extend([21, 22])
    s[0] = 2
    out["after"] = s.tolist()
    out["count"] = s.count(21)
    out["truth"] = [bool(array.array("b")), bool(s)]
    def setbig():
        s[0] = 70000

    for name, f in (("setbig", setbig), ("index", lambda: s[99]), ("key", lambda: s["x"]),
                    ("str", lambda: array.array("I", "ab")), ("float", lambda: array.array("I", [1.5])),
                    ("bad", lambda: array.array("z")), ("long", lambda: array.array("bb")), ("int", lambda: array.array(1)),
                    ("hash", lambda: hash(a)), ("bytes", lambda: array.array("I", b"\x01\x02\x03"))):
        try:
            f()
            out[name] = "no error"
        except Exception as e:
            out[name] = _err(e)
    return out


@router.get("/ucd/{cp}")
async def ucd(cp: int):
    c = chr(cp)
    out = {"category": unicodedata.category(c), "bidi": unicodedata.bidirectional(c),
           "decomposition": unicodedata.decomposition(c), "name": unicodedata.name(c, None)}
    try:
        out["name_strict"] = unicodedata.name(c)
    except ValueError as e:
        out["name_strict"] = _err(e)
    return out


@router.get("/ucd-bad/{which}")
async def ucd_bad(which: str):
    f = {"category": unicodedata.category, "bidirectional": unicodedata.bidirectional, "name": unicodedata.name,
         "decomposition": unicodedata.decomposition}[which]
    out = []
    for arg in ("ab", "", 1):
        try:
            f(arg)
            out.append("no error")
        except TypeError as e:
            out.append(_err(e))
    return out


IPS = ["1.2.3.4", "", "1.2.3", "1.2.3.04", "1.2.3.256", "a.b.c.d", "1.2.3.1234", " 1.2.3.4", "1.2.3.4/8", "::1",
       "fe80::1:2", "1::2::3", ":1:2:3:4:5:6:7", "1:2:3:4:5:6:7:", "1:2:3:4:5:6:7", "1:2:3:4:5:6:7:8:9", "::ffff:1.2.3.4",
       "::ffff:1.2.3.400", "12345::", "g::", "1:2:3:4:5:6:7::", "x" * 50, "y" * 120, "fe80::1%", "1.2.3.4.5"]


@router.get("/ip/{i}")
async def ip(i: int, v6: bool = False):
    text = IPS[i]
    try:
        a = (ipaddress.IPv6Address if v6 else ipaddress.IPv4Address)(text)
        return {"str": str(a), "repr": repr(a), "version": a.version}
    except ipaddress.AddressValueError as e:
        return {**_err(e), "value_error": isinstance(e, ValueError)}


@router.get("/ip-int/{n}")
async def ip_int(n: int, v6: bool = False):
    try:
        return {"str": str((ipaddress.IPv6Address if v6 else ipaddress.IPv4Address)(n))}
    except ValueError as e:
        return _err(e)


@router.get("/ip-packed/{n}")
async def ip_packed(n: int, v6: bool = False):
    try:
        return {"str": str((ipaddress.IPv6Address if v6 else ipaddress.IPv4Address)(bytes(range(1, n + 1))))}
    except ValueError as e:
        return _err(e)


@router.get("/startup")
async def startup():
    return {"st": [ST_A, ST_B, ST_C], "letters": [LETTERS_A, LETTERS_B], "first": [first(b"ab"), first(7)]}


@router.get("/fromkeys")
async def fromkeys():
    return {"keys": list(dict.fromkeys(chain(["a", "b"], ["b", "c"])))}
