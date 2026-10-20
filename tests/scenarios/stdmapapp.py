"""Scenario of fixtures/stdmapapp: standard-library names the libraries translated from their source reach (errno and
sys constants, collections.abc as isinstance targets, the Warning classes and warnings the default filters ignore,
sys.version_info chains, str.join(chain(...)), importlib.import_module of an installed module, array.array).
No database. Compare the logs too (--logs): a dropped warning prints nothing."""

STEPS: list = [("GET", "/health", None), ("GET", "/s/errno", None), ("GET", "/s/sys", None)]
STEPS += [("GET", f"/s/abc/{k}", None) for k in (
    "dict", "list", "tuple", "str", "bytes", "set", "int", "none", "deque", "counter", "defaultdict", "named", "float",
    "bool", "empty")]
STEPS += [("GET", "/s/abc-merge", None), ("GET", "/s/warn", None), ("GET", "/s/warn-catch", None)]
STEPS += [("GET", f"/s/chain/{n}", None) for n in (0, 1, 4)]
STEPS += [("GET", f"/s/bisect/{x}" + q, None) for x in (0, 3, 4, 9) for q in ("", "?lo=1", "?hi=2", "?lo=2&hi=2")]
STEPS += [("GET", f"/s/bisect-bad/{k}", None) for k in ("type", "typer", "lo", "hi", "seq", "loty")]
STEPS += [("GET", "/s/chain-bad", None), ("GET", "/s/import/x", None)]

# fixtures/stdmapapp/stdmore.py: parse_qs, array.array, unicodedata tables, IPv4Address/IPv6Address, startup blocks
STEPS += [("GET", f"/m/qs/{c}", None) for c in ("plain", "blank", "keep", "strict", "strict-noeq", "plus", "semi", "max",
                                                 "max-ok", "badsep", "empty")]
STEPS.append(("GET", "/m/qs-none", None))
STEPS += [("GET", f"/m/array/{tc}?v={v}", None) for tc in "bBhHiIlLqQfd" for v in (0, -1, 300, 70000, 2**31, 2**32, 2**62)]
STEPS += [("GET", "/m/array-ops", None)]
STEPS += [("GET", f"/m/ucd/{cp}", None) for cp in (65, 0x301, 0xE9, 0x5D0, 0x660, 0x378, 0x4E00, 0xAC00, 0xF900,
                                                   0x1F600, 0xFB01, 0x2028, 0x10FFFF, 0)]
STEPS += [("GET", f"/m/ucd-bad/{w}", None) for w in ("category", "bidirectional", "name", "decomposition")]
STEPS += [("GET", f"/m/ip/{i}" + q, None) for i in range(25) for q in ("", "?v6=true")]
STEPS += [("GET", f"/m/ip-int/{n}" + q, None) for n in (-1, 0, 4294967295, 4294967296) for q in ("", "?v6=true")]
STEPS += [("GET", f"/m/ip-packed/{n}" + q, None) for n in (3, 4, 16) for q in ("", "?v6=true")]
STEPS += [("GET", "/m/startup", None), ("GET", "/m/fromkeys", None)]


def reset(db: str) -> None:
    pass
