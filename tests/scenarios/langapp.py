"""Scenario of fixtures/langapp: the Python language, its standard library and Pydantic (dunders, decorators,
pickle, collections, Python-version messages, the documentation corpora, the security review, tests/difftest.py's
findings, real-world shapes). Cases moved from tests/scenarios/dynapp.py, in their order; dynapp's tables
(`reset_tables`)."""
import os

# get_db commits after the response (FastAPI >= 0.121): pause after each write so that the reference has
# committed before the next request reads (the binary commits before answering)
SETTLE = 0.2
# the pause only after the writes that can do work after their response (tests/settle.py)
REFERENCE_APP = "fixtures.langapp.main:app"

# a local TCP port whose connections time out (fixtures/langapp/stdmore.py /tcp and /sock): a listener that never
# accepts, its queue filled at reset. Under Linux (CI) a SYN to a full queue is dropped, the connect times out
# whatever the network; macOS completes the handshake anyway (both servers then connect alike). Before: 10.255.255.1,
# whose answer depended on the network (refused or silent).
BLACKHOLE = int(os.environ.get("PY2AXUM_BLACKHOLE_PORT", "8548"))
_blackhole: list = []


def _start_blackhole() -> None:
    import socket
    if _blackhole:
        return
    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)  # the previous run's queued connections
    listener.bind(("127.0.0.1", BLACKHOLE))
    listener.listen(0)
    _blackhole.append(listener)
    for _ in range(8):
        s = socket.socket()
        s.setblocking(False)
        try:
            s.connect(("127.0.0.1", BLACKHOLE))
        except BlockingIOError:
            pass
        _blackhole.append(s)


def _cpython_pickles() -> tuple[str, str]:
    """Pickles written by CPython itself (protocols 4 and 5) for /pk/load."""
    import base64
    import pickle

    from fixtures.langapp.pk import Entry, sample
    e = Entry(sample(), 1.5, 2.0, 3.0)
    return tuple(base64.b64encode(pickle.dumps(e, protocol=p)).decode() for p in (4, 5))


def _unpickled(b64: str):
    """A pickle written by either server, read back by CPython: compared by value, not by bytes."""
    import base64
    import pickle

    from fixtures.langapp.pk import Bag, Entry

    def desc(o):
        if isinstance(o, Entry):
            return {"entry": [desc(getattr(o, s)) for s in Entry.__slots__]}
        if isinstance(o, Bag):
            return {"bag": sorted((k, desc(v)) for k, v in vars(o).items())}
        if isinstance(o, dict):
            return {k: desc(v) for k, v in o.items()}
        if isinstance(o, (list, tuple)):
            return [type(o).__name__, [desc(x) for x in o]]
        return repr(o)
    try:
        return desc(pickle.loads(base64.b64decode(b64)))
    except Exception as e:  # noqa: BLE001
        return f"unreadable by CPython: {type(e).__name__}: {e}"


def _multipart(parts: list) -> tuple[bytes, dict]:
    """(name, value) text fields and (name, filename, content_type, bytes) files -> body, headers."""
    b = "py2axumBOUNDARY"
    out = b""
    for p in parts:
        out += f"--{b}\r\n".encode()
        if len(p) == 2:
            out += f'Content-Disposition: form-data; name="{p[0]}"\r\n\r\n'.encode() + p[1].encode() + b"\r\n"
        else:
            ct = f"Content-Type: {p[2]}\r\n" if p[2] else ""
            out += (f'Content-Disposition: form-data; name="{p[0]}"; filename="{p[1]}"\r\n{ct}\r\n').encode() + p[3] + b"\r\n"
    out += f"--{b}--\r\n".encode()
    return out, {"content-type": f"multipart/form-data; boundary={b}"}

STEPS: list = [    # dunder methods, hashability, frozen models (fixtures/langapp/dunders.py)
    ("GET", "/dunders/eq", None),
    ("GET", "/dunders/hash", None),
    ("GET", "/dunders/frozen", None),
    ("POST", "/dunders/vdefault", {"bad": 1}),
    ("POST", "/dunders/vdefault", {"bad": 1, "nickName": "z", "upper": "q"}),
    ("POST", "/dunders/vdefault", {}),
    ("GET", "/dunders/jsonresp", None),
    ("GET", "/dunders/raise/unavailable", None),
    ("GET", "/dunders/raise/limited", None),
    ("GET", "/dunders/raise/provider", None),
    ("GET", "/dunders/raise/base", None),
    ("GET", "/dunders/raise/none", None),
    ("GET", "/dunders/builtins", None),
    ("GET", "/dunders/types", None),
    ("GET", "/dunders/classvalue", None),
    ("GET", "/dunders/smallbatch", None),
    # --python-side /dunders/proxied/{name}: relayed by the binary to PY2AXUM_PYTHON_URL
    ("POST", "/dunders/proxied/abc?q=1", b"raw body", {"x-probe": "yes", "content-type": "text/plain"}),
    ("GET", "/dunders/jsonresp?kind=model", None),
    ("GET", "/dunders/jsonresp?kind=dt", None),
    ("GET", "/dunders/jsonresp?kind=nan", None),
    ("POST", "/dunders/vdefault", {"bad": 2, "nick": "ignored", "rank": "x"}),
    # pickle in CPython's format (fixtures/langapp/pk.py)
    ("GET", "/pk/dump", None),
    ("POST", "/pk/load", {"b64": _cpython_pickles()[0]}),
    ("POST", "/pk/load", {"b64": _cpython_pickles()[1]}),
    ("GET", "/pk/errors", None),
    # redis.asyncio (fixtures/langapp/rds.py; Redis database 13, emptied by reset)
    ("POST", "/rds/basic", None),
    ("POST", "/rds/pickled", None),
    ("GET", "/rds/down", None),
    # redis (synchronous client), ConnectionPool.from_url, pipelines (fixtures/langapp/rds.py)
    ("POST", "/rds/sync", None),
    ("GET", "/rds/sync-def", None),
    ("POST", "/rds/pipe", None),
    ("POST", "/rds/apool", None),
    # collections.defaultdict, string.Formatter, str.format errors (fixtures/langapp/colls.py)
    ("POST", "/colls/count", ["ab", "abc", "b", "bcd", "a"]),
    ("POST", "/colls/count", []),
    # dateutil.relativedelta (fixtures/langapp/extras.py)
    ("GET", "/extras/jumps", None),
    ("GET", "/extras/jumps?stop=0", None),
    ("GET", "/extras/jumps?stop=9", None),
    ("GET", "/extras/reldelta?day=2024-03-31&months=1", None),
    ("GET", "/extras/reldelta?day=2024-02-29&months=12", None),
    ("GET", "/extras/reldelta?day=2024-01-31&months=-13&weeks=2", None),
    ("GET", "/extras/reldelta?day=2023-12-31&hours=5", None),
    ("GET", "/extras/reldelta?day=2024-05-15", None),
    ("GET", "/extras/proxy", None),
    ("POST", "/extras/computed", {"a": 1, "b": 2}),
    ("POST", "/extras/computed", {"a": 1, "total": 5, "_seen": [3]}),
    ("POST", "/extras/computed", {"a": "x"}),
    ("POST", "/extras/computed/raw", {"a": 4, "c": "y", "label": "no"}),
    ("GET", "/extras/computed/dump", None),
    ("POST", "/extras/enums", {"group": ["street", "city"], "rel": "MASTER"}),
    ("POST", "/extras/enums", {"group": [1, "a"]}),
    ("POST", "/extras/enums", {"group": "TEXT"}),
    ("POST", "/extras/xml", {"docs": ["<a><b>1</b><b>2</b><c x='1'>t</c><d/><e x='2'/><f>  sp  </f></a>", '<a>text<b>1</b>tail</a>', '<?xml version=\'1.0\' encoding=\'utf-8\'?>\n<r xmlns=\'http://example.com/a\' xmlns:p=\'http://example.com/p\'>\n  <p:i p:k=\'v\' k="w">1</p:i><j>&amp;&lt;&#233;&#x41;</j><![CDATA[ cd ]]></r>', '<a><!-- c --><b>x</b><?pi x?></a>', '<a><b><c>1</c></b><b>2</b><b/></a>', '<a>  </a>', "<a x='1'>  </a>", "<a x='a\tb'></a>", '\n<a>\r\n x \r\n</a>\n', '<a>', '<a></b>', '', 'x', '<a>&nope;</a>', '<a/><b/>', '<a><b></a>', "<a x='1' x='2'/>", '<p:a/>', '<a>é</a>']}),
    ("POST", "/extras/xml/form", b"doc=%3Cr%3E%3Cs%3E1%3C%2Fs%3E%3C%2Fr%3E", {"content-type": "application/x-www-form-urlencoded"}),
    ("GET", "/colls/dup/a", None),
    ("GET", "/colls/dup/two/b?n=3", None),
    ("GET", "/colls/dup/two/b?n=x", None),
    ("POST", "/colls/render", {"template": "{street} {number}, {city}", "values": {"street": "Main", "city": "X"}}),
    ("POST", "/colls/render", {"template": "{a!r} {b:>5} {c!s}", "values": {"a": "q", "b": "z"}}),
    ("POST", "/colls/render", {"template": "{}", "values": {}}),
    ("POST", "/colls/render", {"template": "a } b", "values": {}}),
    ("POST", "/colls/render", {"template": "a { b", "values": {}}),
    ("POST", "/colls/render", {"template": "{x!z}", "values": {"x": 1}}),
    ("POST", "/colls/render", {"template": "{0}{}", "values": {}}),
    ("POST", "/colls/format", {"template": "{} {}", "args": [1]}),
    ("POST", "/colls/format", {"template": "{0} {}", "args": [1, 2]}),
    ("POST", "/colls/format", {"template": "{} {0}", "args": [1, 2]}),
    ("POST", "/colls/format", {"template": "{name}", "kw": {}}),
    ("POST", "/colls/format", {"template": "{0!r:>8}|{n:05d}", "args": ["é"], "kw": {"n": 42}}),
    # float presentations g/e (str.format and %)
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [123456.0, 123456.0, 123456.0, 123456.0, 123456.0, 123456.0, 123456.0, 123456.0, 123456.0, 123456.0, 123456.0]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [1234567.0, 1234567.0, 1234567.0, 1234567.0, 1234567.0, 1234567.0, 1234567.0, 1234567.0, 1234567.0, 1234567.0, 1234567.0]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [0.0001, 0.0001, 0.0001, 0.0001, 0.0001, 0.0001, 0.0001, 0.0001, 0.0001, 0.0001, 0.0001]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [-2.5, -2.5, -2.5, -2.5, -2.5, -2.5, -2.5, -2.5, -2.5, -2.5, -2.5]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [1e+22, 1e+22, 1e+22, 1e+22, 1e+22, 1e+22, 1e+22, 1e+22, 1e+22, 1e+22, 1e+22]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [9.9999, 9.9999, 9.9999, 9.9999, 9.9999, 9.9999, 9.9999, 9.9999, 9.9999, 9.9999, 9.9999]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1]}),
    ("POST", "/colls/format", {"template": "{:x}|{:X}|{:o}|{:b}|{:#x}", "args": [255, 255, 8, 5, 1]}),
    ("POST", "/colls/percent", {"fmt": "%g|%.3g|%e|%.2E|%G|%10.3g|%-8.2g|%X|%o", "args": [0.5, 0.0, 1234.5, -0.00012, 1e-07, 3.14159, 2.0, 255, 8]}),
    ("POST", "/colls/percent", {"fmt": "%.3g s", "args": [0.0]}),
    ("POST", "/colls/percent", {"fmt": "%.3g s", "args": [1.5]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [0.0, 0.0, 0.0, 0.0, 0.0]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [1.0, 1.0, 1.0, 1.0, 1.0]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [12.0, 12.0, 12.0, 12.0, 12.0]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [123.0, 123.0, 123.0, 123.0, 123.0]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [0.5, 0.5, 0.5, 0.5, 0.5]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [99.95, 99.95, 99.95, 99.95, 99.95]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [1e+16, 1e+16, 1e+16, 1e+16, 1e+16]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [-2.5, -2.5, -2.5, -2.5, -2.5]}),
    ("POST", "/colls/format", {"template": "{:.3}", "args": [5]}),
    # property setters and read-only properties (fixtures/langapp/decoprops.py)
    ("GET", "/decop/setter", None),
    ("GET", "/decop/setter?n=0", None),
    ("GET", "/decop/readonly", None),
    ("GET", "/decop/inherit", None),
    ("GET", "/decop/instance-attr", None),
    ("GET", "/decop/methods", None),
    ("GET", "/decop/ctx-errors", None),
    # project decorators (fixtures/langapp/decos.py)
    ("GET", "/decos/fetch", None),
    ("GET", "/decos/fetch?n=4&fail=true", None),
    ("GET", "/decos/attrs", None),
    ("GET", "/decos/compute", None),
    ("GET", "/decos/bare", None),
    ("GET", "/decos/step", None),
    ("GET", "/decos/step?b=0", None),
    ("GET", "/decos/nested", None),
    ("GET", "/decos/types", None),
]

# fixtures/langapp/edges.py: what tests/difftest.py found (generation from the OpenAPI). Bodies as FastAPI decodes
# them: CPython's json module (BOM, UTF-16, NaN, positions and messages), `null` = no body, strict content type
FORM_CT = {"content-type": "application/x-www-form-urlencoded"}
DEEP = b"[" * 12000 + b"]" * 12000  # deeper than the decoder's limit
STEPS += [
    ("POST", "/edges/model", b"null"),
    ("POST", "/edges/opt", b"null"),
    ("POST", "/edges/opt", b'{"name": "a"}'),
    ("POST", "/edges/embed", b"null"),
    ("POST", "/edges/embed", b"[1, 2]"),
    ("POST", "/edges/embed", b'"text"'),
    ("POST", "/edges/embed", b'{"item": null, "k": null}'),
    ("POST", "/edges/embed", b'{"item": {"name": "a"}}'),
    ("POST", "/edges/model", b'\xef\xbb\xbf{"name": "bom"}'),
    ("POST", "/edges/model", '{"name": "utf16"}'.encode("utf-16")),
    ("POST", "/edges/model", '{"name": "utf16le"}'.encode("utf-16-le")),
    ("POST", "/edges/model", b'{"name": "\xff"}'),
    ("POST", "/edges/model", b'{"name": "x"}', {"content-type": ""}),
    ("POST", "/edges/model", b'{"name": "x"}', {"content-type": "text/plain"}),
    ("POST", "/edges/model", b'{"name": "x"}', {"content-type": "application/vnd.api+json; charset=utf-8"}),
    ("POST", "/edges/model", b'{"name": NaN}'),
    ("POST", "/edges/model", b'{"name": "x", "n": 1e309}'),
    ("POST", "/edges/model", b'{"name": "x", "n": -0.0, "flag": 2.0}'),
    ("POST", "/edges/model", b'{"name": "x", "flag": 0.5}'),
    ("POST", "/edges/model", b'{"name": "a", "n": 1, "name": "b"}'),
    # (nested deeper than the decoder's limit: CPython's depends on its C stack, docs/supported.md; see /edges/loads)
    *[("POST", "/edges/model", t) for t in (b"{", b'{"name": "a",}', b"[1,]", b'{"name" 1}', b'{"name": "a\\x"}',
                                            b'"\\u12"', b'{"name": "a\nb"}', b"01", b"  ", b'{"a": 1}x',
                                            b'\n\n  {"name": }')],  # a lone surrogate: docs/supported.md
    # json.loads in the application: CPython's values and messages
    *[("POST", "/edges/loads", t, {"content-type": "text/plain"}) for t in (
        b'{"a": [1, 2.5, -0.0, 1e2, "\\ud83d\\ude00", "\\u00e9"], "c": 0, "c": [], "b": null}', b'[NaN]', b"[-Infinity, 1e400]", b"{",
        b"[1,]", b'{"k": tru}', b'\xef\xbb\xbf[]', DEEP)],
    ("GET", "/edges/bool", None),
    ("GET", "/edges/loop", None),
    ("GET", "/edges/strclass", None),
    ("GET", "/edges/tuple-in", None),
    ("GET", "/edges/dates", None),
    ("GET", "/edges/fmt", None),
    ("GET", "/edges/nul/a%00b", None),
    ("GET", "/edges/nul/fixed.pdf", None),
    ("GET", "/edges/inf", None),
    ("GET", "/edges/inf?x=1.5", None),
    # numbers beyond 64 bits: int_parsing_size (a 422), bool_type
    ("POST", "/edges/model", {"name": "x", "n": 9.3e18}),
    ("POST", "/edges/model", {"name": "x", "n": -9.223372036854775808e18, "flag": 9.2e18}),
    ("POST", "/edges/model", {"name": "x", "flag": -1.37e158}),
    # speedate's numeric strings: a float needs its point, an int must fit 64 bits, no spaces
    *[("GET", f"/edges/types?dt={v}", None) for v in ("1.0e3", "1.e3", "5e3", ".5", "-.5", "%2B1.5e3", "1.5e400",
                                                      "12345678901234567890", "%201700000000%20")],
    # int from a string: only zeros after the point; timestamps within years 0000-9999 (speedate)
    *[("GET", f"/edges/types?i={v}", None) for v in ("12.0", "12.000", "-0.0", "12.", "12.5", "1e3", "1.8446742972109271e%2B19", "%2012%20", "1_000")],
    *[("GET", f"/edges/types?d={v}&dt={v}", None) for v in ("100000000000000000", "-100000000000000000", "253402300800000", "86400", "0")],
    *[("POST", "/edges/stamp", {"d": v, "dt": v}) for v in (1e17, -1e17, 4255305914356606.0, 253402300799999, 86400.0, 1700000000.5)],
    # speedate's fraction: cut off towards zero, rounded to the microsecond, carried
    *[("POST", "/edges/stamp", {"dt": v}) for v in (-1e-175, -6e-7, -4e-7, 5e-7, 1.9999999, -1.0000001, -1.9999999,
                                                     -0.9999999, -2.25, -1700000000.1234567, 1700000000.9999996)],
    ("POST", "/edges/stamp", {"d": -1e-175}),
    # Starlette's form limits: a field over 1 MiB (name + value as sent), more than 1000 fields; multipart boundary
    ("POST", "/edges/form", b"username=" + b"x" * (1024 * 1024 - 8), FORM_CT),
    ("POST", "/edges/form", b"username=" + b"x" * (1024 * 1024 - 7), FORM_CT),
    ("POST", "/edges/form", b"&".join([b"username=a"] * 1000), FORM_CT),
    ("POST", "/edges/form", b"&".join([b"username=a"] * 1001), FORM_CT),
    ("POST", "/edges/form", b"username=a&&&password=b&", FORM_CT),
    ("POST", "/edges/upload", b"--x\r\n", {"content-type": "multipart/form-data"}),
    ("POST", "/edges/upload", b"--x\r\n", {"content-type": "multipart/form-data; boundary="}),
    # a float read back from JSON/JSONB columns keeps its shortest repr (serde_json float_roundtrip)
    # (JSONB writes 6.6e+205 back as an 206-digit integer: CPython reads an int, the binary a float, docs/supported.md)
    ("POST", "/edges/jsonb/edge-f", {"data": [6.618213479614815e-205, 0.1, 1e-7, 5e-324, 2.2250738585072014e-308,
                                              123456789.12345679, -0.0, 1e18, 9.2e18],
                                     "raw": [6.618213479614815e+205, 1.7976931348623157e308, 0.1, -0.0, 5e-324]}),
    ("GET", "/edges/jsonb/edge-f", None),
    # JSONB operators: contains (a list, a dict, a scalar), contained_by, has_key, has_any, has_all (SQLAlchemy binds
    # the list of has_any/has_all as JSONB: `jsonb ?| jsonb` does not exist, a 500 on both sides)
    ("POST", "/edges/jsonb/jq1", {"data": {"tags": ["a", "b"], "n": 1}, "raw": None}),
    ("POST", "/edges/jsonb/jq2", {"data": ["a", "c"], "raw": None}),
    ("POST", "/edges/jsonb/jq3", {"data": {"tags": ["a"], "x": None}, "raw": None}),
    ("POST", "/edges/jsonb/jq4", {"data": "a", "raw": None}),
    ("POST", "/edges/jsonb-ops", {"op": "contains", "value": ["a"]}),
    ("POST", "/edges/jsonb-ops", {"op": "contains", "value": {"tags": ["a"]}}),
    ("POST", "/edges/jsonb-ops", {"op": "contains", "value": {"tags": ["b", "a"], "n": 1}}),
    ("POST", "/edges/jsonb-ops", {"op": "contains", "value": "a"}),
    ("POST", "/edges/jsonb-ops", {"op": "contains", "value": {"x": None}}),
    ("POST", "/edges/jsonb-ops", {"op": "contained_by", "value": {"tags": ["a", "b"], "n": 1, "x": None}}),
    ("POST", "/edges/jsonb-ops", {"op": "contained_by", "value": ["a", "b", "c"]}),
    ("POST", "/edges/jsonb-ops", {"op": "has_key", "value": "tags"}),
    ("POST", "/edges/jsonb-ops", {"op": "has_key", "value": "a"}),
    ("POST", "/edges/jsonb-ops", {"op": "has_any", "value": ["n", "x"]}),
    ("POST", "/edges/jsonb-ops", {"op": "has_all", "value": ["tags", "n"]}),
    ("POST", "/edges/jsonb-ops", {"op": "has_all", "value": []}),
    # an index of a JSON / JSONB column and its typed accessors (as_boolean(), as_string(), ...)
    ("POST", "/edges/jsonb/ji1", {"data": {"ok": True, "n": 5, "f": 1.5, "p": 3.14159, "s": "x"},
                                  "raw": {"ok": True, "env": "ci", "n": 5, "f": 1.5, "p": "2.005", "o": {"a": 1}}}),
    ("POST", "/edges/jsonb/ji2", {"data": {"ok": "false", "n": "7", "f": 2, "p": -1},
                                  "raw": {"ok": False, "env": "prod", "n": "7", "f": "2.5e1", "o": [1, 2]}}),
    ("POST", "/edges/jsonb/ji3", {"data": ["a", "b", True, 3], "raw": ["a", "b", "yes", 3]}),
    ("POST", "/edges/jsonb/ji4", {"data": None, "raw": None}),
    ("POST", "/edges/jsonb/ji5", {"data": {"n": None, "bad": "x"}, "raw": {"env": None, "ok": None, "bad": "x"}}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "ok", "acc": "boolean"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "ok", "acc": "string"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "n", "acc": "integer"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "n", "acc": "float"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "n", "acc": "numeric"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "n", "acc": "string"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "n", "acc": "json"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "n", "acc": "bare"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "f", "acc": "float"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "f", "acc": "string"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "p", "acc": "numeric"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "p", "acc": "float"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "env", "acc": "string"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "o", "acc": "json"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "o", "acc": "bare"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "o", "acc": "string"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": 0, "acc": "string"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": 0, "acc": "json"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": 0, "acc": "bare"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": 2, "acc": "boolean"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": 2, "acc": "string"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": 3, "acc": "integer"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "missing", "acc": "integer"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "missing", "acc": "boolean"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "missing", "acc": "json"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "bad", "acc": "integer"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "bad", "acc": "boolean"}),
    ("POST", "/edges/json-index", {"col": "raw", "key": "bad", "acc": "float"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "ok", "acc": "boolean"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "ok", "acc": "string"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "n", "acc": "integer"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "n", "acc": "float"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "n", "acc": "numeric"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "n", "acc": "string"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "n", "acc": "json"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "n", "acc": "bare"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "f", "acc": "float"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "f", "acc": "string"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "p", "acc": "numeric"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "p", "acc": "float"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "env", "acc": "string"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "o", "acc": "json"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "o", "acc": "bare"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "o", "acc": "string"}),
    ("POST", "/edges/json-index", {"col": "data", "key": 0, "acc": "string"}),
    ("POST", "/edges/json-index", {"col": "data", "key": 0, "acc": "json"}),
    ("POST", "/edges/json-index", {"col": "data", "key": 0, "acc": "bare"}),
    ("POST", "/edges/json-index", {"col": "data", "key": 2, "acc": "boolean"}),
    ("POST", "/edges/json-index", {"col": "data", "key": 2, "acc": "string"}),
    ("POST", "/edges/json-index", {"col": "data", "key": 3, "acc": "integer"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "missing", "acc": "integer"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "missing", "acc": "boolean"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "missing", "acc": "json"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "bad", "acc": "integer"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "bad", "acc": "boolean"}),
    ("POST", "/edges/json-index", {"col": "data", "key": "bad", "acc": "float"}),
    ("GET", "/edges/json-where", None),
]


# silently wrong values made exact or refused (fixtures/langapp/silent.py): frozenset, TypeAdapter titles, model_type,
# decode/encode errors=, what a Request offers
STEPS += [
    ("GET", "/silent/frozen/type", None),
    ("GET", "/silent/frozen/isinstance", None),
    ("GET", "/silent/frozen/repr", None),
    ("GET", "/silent/frozen/eq", None),
    ("GET", "/silent/frozen/iter", None),
    ("GET", "/silent/frozen/add", None),
    ("GET", "/silent/frozen/update", None),
    ("GET", "/silent/frozen/remove", None),
    ("GET", "/silent/frozen/methods", None),
    ("GET", "/silent/frozen/key", None),
    ("GET", "/silent/frozen/setkey", None),
    ("GET", "/silent/frozen/json", None),
    ("GET", "/silent/frozen/dumps", None),
    ("GET", "/silent/frozen/pickle", None),
    ("GET", "/silent/frozen/model", None),
    ("GET", "/silent/frozen/modelerr", None),
    ("GET", "/silent/frozen/fromfrozen", None),
    ("GET", "/silent/frozen/hash", None),
    ("GET", "/silent/frozen/set", None),
    ("GET", "/silent/frozen/counter", None),
    ("GET", "/silent/adapter/int", None),
    ("GET", "/silent/adapter/str", None),
    ("GET", "/silent/adapter/float", None),
    ("GET", "/silent/adapter/bool", None),
    ("GET", "/silent/adapter/listint", None),
    ("GET", "/silent/adapter/setint", None),
    ("GET", "/silent/adapter/dict", None),
    ("GET", "/silent/adapter/tuple", None),
    ("GET", "/silent/adapter/tuplevar", None),
    ("GET", "/silent/adapter/opt", None),
    ("GET", "/silent/adapter/union", None),
    ("GET", "/silent/adapter/listsub", None),
    ("GET", "/silent/adapter/sub", None),
    ("GET", "/silent/adapter/listopt", None),
    ("GET", "/silent/adapter/frozen", None),
    ("GET", "/silent/adapter/json", None),
    ("GET", "/silent/adapter/errors", None),
    ("GET", "/silent/adapter/ok", None),
    ("POST", "/silent/modeltype/outer", {"v": 5}),
    ("POST", "/silent/modeltype/outer", {"v": "x"}),
    ("POST", "/silent/modeltype/outer", {"v": [1]}),
    ("POST", "/silent/modeltype/outer", {"v": None}),
    ("POST", "/silent/modeltype/outer", {"v": {"x": "a"}}),
    ("POST", "/silent/modeltype/outer", {"v": {"x": 1}}),
    ("POST", "/silent/modeltype/optional", {"v": 5}),
    ("POST", "/silent/modeltype/optional", {"v": "x"}),
    ("POST", "/silent/modeltype/top", {"v": 5}),
    ("POST", "/silent/modeltype/top", {"v": "x"}),
    ("POST", "/silent/modeltype/init", {"v": 5}),
    ("POST", "/silent/modeltype/attrs", {"v": 5}),
    ("POST", "/silent/modeltype/adapter", {"v": 5}),
    ("POST", "/silent/modeltype/ok", {"v": 1}),
    ("GET", "/silent/decode/strict", None),
    ("GET", "/silent/decode/replace", None),
    ("GET", "/silent/decode/ignore", None),
    ("GET", "/silent/decode/backslash", None),
    ("GET", "/silent/decode/ascii", None),
    ("GET", "/silent/decode/latin", None),
    ("GET", "/silent/decode/valid", None),
    ("GET", "/silent/decode/sig", None),
    ("GET", "/silent/decode/encode", None),
    ("GET", "/silent/decode/encodestrict", None),
    ("GET", "/silent/decode/surrogate", None),
    ("GET", "/silent/req_basic?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_url?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_url_cmp?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_query_dict?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_query_get?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_query_iter?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_query_neg?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_headers_get?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_headers_type?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_headers_getlist?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_headers_multi?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_headers_keys?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_headers_items?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_headers_values?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_headers_iter?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_headers_dict?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_headers_len?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_headers_neg?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_headers_repr?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_cookies?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_cookies_neg?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_client?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_scope?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_state?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_app_router?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_path_params?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_body?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_json?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_receive?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_disconnected?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_eq?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_body_cache?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_app?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/silent/req_scope_known?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("GET", "/procx/flock/a", None),
    ("GET", "/procx/flock/b?op=2", None),
    ("GET", "/procx/flock/c?op=5", None),
    ("GET", "/procx/flock/d?op=0", None),
    ("GET", "/procx/flock/e?op=99", None),
    ("GET", "/procx/flock-errors", None),
    ("GET", "/procx/dbapi", None),
    ("GET", "/silent/req_scope_mw_keys", None, {"cookie": "session=abc"}),
    ("GET", "/silent/req_scope_mw_key_missing", None),
    ("GET", "/silent/req_query_str?a=1&a=2&b=x%20y&q=z", None, {"x-a": "val-a", "x-multi": "val-m", "cookie": "a=1; b=two; a=3"}),
    ("POST", "/silent/req_body", b'{"k": [1, 2]}', {"content-type": "application/json"}),
    ("POST", "/silent/req_json", b'{"k": [1, 2]}', {"content-type": "application/json"}),
]


def reset(db: str) -> None:
    from tests.scenarios.dynapp import fixed_file, reset_tables

    fixed_file()  # fixtures/langapp/edges.py
    _start_blackhole()
    # fixtures/langapp/sec.py /sec/resolve: a symlink inside BASE leading out of it
    os.makedirs("/tmp/py2axum-sec/base/sub", exist_ok=True)
    os.makedirs("/tmp/py2axum-sec/outside", exist_ok=True)
    if not os.path.islink("/tmp/py2axum-sec/base/link"):
        os.symlink("/tmp/py2axum-sec/outside", "/tmp/py2axum-sec/base/link")
    import redis as _redis  # fixtures/langapp/rds.py, kwargs.py
    _redis.Redis.from_url(os.environ.get("REDIS_URL", "redis://127.0.0.1:6379/13")).flushdb()
    reset_tables(db)


def _zip_untimed(b64: str) -> str:
    """A zip written now: its DOS time and date fields (local headers, central directory) zeroed."""
    import base64
    import struct
    b = bytearray(base64.b64decode(b64))
    i = 0
    while i + 4 <= len(b):
        sig = bytes(b[i:i + 4])
        if sig == b"PK\x03\x04":
            b[i + 10:i + 14] = b"\0\0\0\0"
            n, x = struct.unpack("<HH", b[i + 26:i + 30])
            i += 30 + n + x + struct.unpack("<L", b[i + 18:i + 22])[0]
        elif sig == b"PK\x01\x02":
            b[i + 12:i + 16] = b"\0\0\0\0"
            n, x, c = struct.unpack("<HHH", b[i + 28:i + 34])
            i += 46 + n + x + c
        else:
            break
    return base64.b64encode(bytes(b)).decode()


def normalize(body):
    """A zip written now (fixtures/langapp/stdmore.py): its times zeroed. Pickles (pk.py): read back by CPython."""
    if isinstance(body, dict) and "zip" in body and "stored" in body:
        body = dict(body, zip=_zip_untimed(body["zip"]), stored=_zip_untimed(body["stored"]))
    if isinstance(body, dict) and set(body) == {"slots", "proto4"}:
        body = {k: _unpickled(v) for k, v in body.items()}
    return body


def _pydmore_cases() -> list:
    """fixtures/langapp/pydmore.py: validate_assignment with validators, min_items/max_items, File(default=[])."""
    d = {"name": "Example.COM", "catch_all": "", "port": 25}
    steps = [
        ("POST", "/pydmore/domains", d),
        ("POST", "/pydmore/domains?name=Other.org&catch_all=me@x.org&port=26", d),
        ("POST", "/pydmore/domains?name=a..b&catch_all=nobody&port=0", d),
        ("POST", "/pydmore/domains?name=ab&catch_all=%20%20", d),
        ("POST", "/pydmore/domains?name=bad%20name", {"name": "ok.org", "catch_all": "x"}),
        ("POST", "/pydmore/domains", {"name": "a..b"}),
        ("POST", "/pydmore/domains", {"name": " ok.org ", "port": 0}),
        ("POST", "/pydmore/orders", {"domains": ["a"]}),
        ("POST", "/pydmore/orders", {"domains": []}),
        ("POST", "/pydmore/orders", {"domains": ["a", "b", "c", "d"], "servers": ["1", "2", "3"], "tags": []}),
        ("POST", "/pydmore/orders", {"domains": "x"}),
    ]
    for parts in ([("to", "a@b.c"), ("subject", "S")],
                  [("to", "a@b.c"), ("files", "1.txt", "text/plain", b"abc"), ("files", "2.txt", None, b"de")],
                  [("to", "a@b.c"), ("files", "")],
                  [("to", "a@b.c"), ("extra", "not a file")],
                  [("subject", "no to")]):
        body, h = _multipart(parts)
        steps.append(("POST", "/pydmore/compose", body, h))
    steps.append(("POST", "/pydmore/compose", b"to=a%40b.c", {"content-type": "application/x-www-form-urlencoded"}))
    return steps


STEPS += _pydmore_cases()


def _pydc_cases() -> list:
    """fixtures/langapp/pydcorpus.py: aliases, Field options, str config on items, strict mode, discriminated unions."""
    P = "/pydc"
    steps = []
    for b in ({"names": ["Ada", "Lovelace"]}, {"names": ["Ada"], "zip": "75001"}, {"names": "Ada"}, {},
              {"names": ["Ada", 5], "contact": {"address": {"city": "Paris"}}, "town": "x"},
              {"names": ["A"], "town": "Lyon", "city": None}, {"names": ["A"], "first": "B"},
              {"names": ["A"], "contact": {"address": {}}, "postcode": 1}, {"names": [None]}):
        steps += [("POST", P + "/paths", b), ("POST", P + "/paths/dump", b)]
    for b in ({"firstName": "Ada"}, {"first_name": "Ada", "lang": "en"}, {"firstName": "Ada", "languageCode": "en"},
              {"firstName": "Ada", "first_name": "B", "extra": 1}, {"lastName": 1}):
        steps += [("POST", P + "/gen", b), ("POST", P + "/gen/dump", b)]
    for b in ({"FIELD_ONE": 1}, {"field_one": 1}, {"FIELD_ONE": 1, "OTHER": "z"}):
        steps.append(("POST", P + "/gen2", b))
    for b in ({"my_field": 1, "http_code": 2}, {"myField": 1}, {"h_t_t_p_code": 3}):
        steps.append(("POST", P + "/snake", b))
    for b in ({"my_field": "a"}, {"my_alias": "a"}):
        steps.append(("POST", P + "/byname", b))
    for b in ({"name": " ab "}, {"name": "abcdefgh"}, {"name": ""}, {"name": "a", "tags": [" x ", "toolongtag", ""],
              "labels": {" k ": " v ", "toolongkey": "x"}}, {"name": "a", "code": "abcdefg"},
              {"name": "a", "hidden": 3, "secret": "zz"}, {"name": "a", "step": 15}, {"name": "a", "step": 7},
              {"name": "a", "step": 110}, {"name": "a", "step": "10"}, {"name": "a", "ratio": 0.3},
              {"name": "a", "ratio": 0.35}, {"name": "a", "even": 3}, {"name": "a", "even": 4.0}, {"name": "a", "even": -4.5},
              {"name": "a", "either": "1", "other": "1"}, {"name": "a", "either": 1, "other": 1.5},
              {"name": "a", "either": None, "other": None}, {"name": "a", "fixed": "given"}):
        steps += [("POST", P + "/opts", b), ("POST", P + "/opts/model", b)]
    for b in ({}, {"when": "2020-01-01"}, {"at": "2020-01-01T00:00:00"}, {"n": "1", "f": "1", "s": 1, "b": 1},
              {"n": 1.0, "f": 1, "s": "x", "b": True}, {"pair": [1, 2]}, {"items": [1, "2"]}, {"items": "x"},
              {"inner": {"v": "3"}}, {"inner": 1}, {"lax": "4"}, {"n": True}, {"f": True}, {"when": None}):
        steps.append(("POST", P + "/strict", b))
    for b in ({"n": "1"}, {"nums": ["1", 2]}, {"nums": "1"}, {"day": "2020-01-01"}, {"day": None}, {"loose": "2"},
              {"n": 1, "nums": [True]}, {"site": "http:////example.com"}, {"site": "https://ok.org/a"},
              {"site": "http:\\\\x.org"}, {"site": "HTTP://X.org"}, {"site": "x"}):
        steps.append(("POST", P + "/fieldstrict", b))
    steps += [("POST", P + "/strict/python", {"when": "2020-01-02"}), ("POST", P + "/strict/python", {})]
    for b in ({"pet": {"pet_type": "cat", "meows": "x"}}, {"pet": {"pet_type": "dog", "barks": 1}},
              {"pet": {"pet_type": "lizard"}}, {"pet": {"pet_type": "reptile", "scales": "yes"}}, {"pet": {}},
              {"pet": {"pet_type": 1}}, {"pet": {"pet_type": None}}, {"pet": "x"}, {"pet": [1]}, {"pet": None},
              {"pet": {"pet_type": ["cat"]}}, {}, {"pet": None, "nested": {"pet_type": "kitty", "color": "white", "white_name": "W"}},
              {"pet": None, "nested": {"pet_type": "kitty", "color": "grey"}}, {"pet": None, "nested": {"pet_type": "kitty"}},
              {"pet": None, "nested": {"pet_type": "dog", "barks": "loud"}}, {"pet": None, "nested": {"color": "black"}}):
        steps.append(("POST", P + "/owner", b))
    steps.append(("POST", P + "/owner/python", None))
    for b in ({}, {"dessert": {"fruit": "apple", "n": "1"}}, {"dessert": {"filling": "pumpkin", "n": "x"}}, {"dessert": {}},
              {"dessert": {"fruit": "pear"}}, {"dessert": 5}, {"dessert": {"fruit": 5}}, {"value": "a"}, {"value": 1.5},
              {"value": [1]}, {"pie": {"filling": "pumpkin"}}, {"pie": {"x": 1}}):
        steps.append(("POST", P + "/dinner", b))
    for b in ({}, {"code": "café"}, {"code": "éé"}, {"code": "abcd"}, {"code": "ab1"}, {"code": "ab"}, {"low": "AB"},
              {"low": "ab"}, {"names": ["foo", 123]}, {"hook": 1}, {"hook": None}, {"dots": 1}, {"plain": {}}, {"plain": "x"},
              {"ident": "4"}, {"ident": "x"}):
        steps.append(("POST", P + "/misc", b))
    for x in ("invalid", [1, 2], "1/3", "3", 0.5, 2, " 2/4 ", "1.25", "-1/0", "1e2", None, True, 0.1, "1_000/3", ".5", "5.",
              "+3/6", "1E-2", "0x10", "1 /2", "nan", "inf", 1e300, -0.0, "2/-3", "1.5e1/2", "-.25e+1", 123456789012345678):
        steps += [("POST", P + "/ratio", {"x": x}), ("POST", P + "/ratio/use", {"y": x})]
    steps.append(("POST", P + "/ratio/use", {}))
    for x in ("abc", "1+2j", 3, 1.5, [1, 2], "2j", " 1 + 2j ", "(1+2j)", {"real": 1}, "1-2j", "1e3j", "inf", True, "1e20+1e-7j",
              "-0+2j", "-2j", "nan+infj", "0.1+0.2j", "1e300+1e-300j", "0j", "-1-0j", "1_0j", "j", "-j", "+1.5J", " (1+2j) ",
              "( 1+2j )", "infj", "-nan+1j", "1e", "1+", "1_+2j", ".5j", "5.j", "INFINITYj", None, ""):
        steps += [("POST", P + "/cplx", {"n": x}), ("POST", P + "/cplx/use", {"n": x})]
    steps += [("POST", P + "/pets", b) for b in (["a", "b"], ["a", 1], "x", {"root": ["a"]})]
    steps += [("POST", P + "/pets/use", ["dog", "cat"])]
    steps += [("POST", P + "/kennel", b) for b in ({"pets": ["x"]}, {"pets": [1]}, {"pets": [], "counts": {"a": "1"}},
                                                   {"pets": [], "counts": {"a": "x"}})]
    steps += [("POST", P + "/shout", b) for b in ({"v": "ab"}, {"v": "AB"}, {})]
    steps += [("POST", P + "/custom", b) for b in ({"name": None}, {"name": "n", "w": "x"}, {"x": 84}, {"w": None}, {"x": 1, "note": "n"})]
    for b in ({"password": "abc"}, {"password": "x", "other": "y", "uid": "886313e1-3b8a-4372-9b90-0c9aee199e5d"},
              {"password": 1}, {"password": None}, {"password": ""}, {"password": "p", "uid": "a6cc5730-2261-11ee-9c43-2eb5a363657c"},
              {"password": "p", "uid": "nope"}):
        steps += [("POST", P + "/secrets", b), ("POST", P + "/secrets/use", b)]
    for b in ({"a": 11}, {"a": 1}, {"a": 11, "b": 3}, {"a": "x", "c": "y"}, {}):
        steps.append(("POST", P + "/defaults", b))
    for b in ({}, {"number": 2}, {"number": 3}, {"number": "4"}, {"alias_even": 5}, {"label": 3, "number": 2},
              {"label": -1}, {"numbers": 5}, {"numbers": ["1", 2]}, {"numbers": "x"}, {"plain": 3}, {"plain": "x"},
              {"plain": None}, {"stamp": "now"}, {"stamp": 7}, {"stamp": "x"}, {"number": 1, "label": "x"}):
        steps.append(("POST", P + "/checked", b))
    for b in ({}, {"short": "abcdefgh"}, {"short": 1}, {"nums": ["1", 2]}, {"nums": ["x", 1, "y"]}, {"nums": "boom"},
              {"nums": 3}, {"when": "epoch"}, {"when": "2020-01-01T10:00:00.123456"}, {"when": "x"}, {"when": None},
              {"word": "abc"}, {"word": "abcd"}, {"word": 5}):
        steps.append(("POST", P + "/wrapped", b))
    for b in ({}, {"t": [1, "2", True]}, {"t": [1, 2]}, {"t": [1, 2, 3, 4]}, {"t": ["x", 2, 3, 4]}, {"t": ["x"]},
              {"t": "abc"}, {"one": []}, {"one": [1, 2]}, {"empty": [1]}, {"empty": []}, {"pair": [{"v": "1"}, "a"]},
              {"pair": [{"v": "x"}]}, {"kind": "Inner"}, {"model": {}}, {"note": "n"}):
        steps.append(("POST", P + "/shapes", b))
    for b in ({}, {"number": 4, "when": "2021-05-01T00:00:00", "text": "hi", "tags": ["a", "b"], "ratio": 0.5, "kept": 3},
              {"ratio": 0}, {"kept": 1, "number": 0}, {"tags": ["x"]}):
        steps += [("POST", P + "/sered", b), ("POST", P + "/sered/model", b)]
    for b in ({}, {"flat": {"username": "u", "password": "p"}}, {"listed": [{"username": "a"}, {"username": "b", "inner": {"v": 2}}]},
              {"flat": {"username": "u"}}):
        steps += [("POST", P + "/holder", b), ("POST", P + "/holder/dump", b)]
    for b in ({}, {"dq": [1, "2", 3]}, {"dq": "abc"}, {"dq": {"a": 1}}, {"dq": ["x"]}, {"fs": [1, 2, 2, 3]}, {"fs": "ab"},
              {"fs": {}}, {"anyfs": [1, 2]}, {"counts": {"a": 2, "b": "3"}}, {"counts": ["a"]}, {"counts": {"a": "x"}},
              {"ordered": {"b": 1, "a": "2"}}, {"ordered": [1]}, {"seq": ["a", "b"]}, {"seq": "ab"}, {"seq": {"a": 1}},
              {"seq": [1, "a"]}, {"seq": None}):
        steps += [("POST", P + "/colls", b), ("POST", P + "/colls/use", b)]
    for path, bodies in (("/env/int", ({"data": "1"}, {"data": "x"}, {})), ("/env/inner", ({"data": {"v": "2"}}, {"data": 1})),
                         ("/env/raw", ({"data": {"any": [1]}}, {})), ("/paged", ({"data": ["1", 2], "total": 2}, {"data": "x"})),
                         ("/holding", ({"item": {"v": 3}}, {"item": 3})), ("/pair", ({"key": 1, "value": "2"}, {"key": "k"}))):
        steps += [("POST", P + path, b) for b in bodies]
    return steps

STEPS += _pydc_cases()
STEPS += [
    ("POST", "/pydmore/notices", {"meta": {"k": [1, 2]}, "hash": "h1", "content": "hello blob"}),
    ("POST", "/pydmore/notices", {"meta": None, "label": "info", "hash": "h1"}),
    ("POST", "/pydmore/notices", {"label": "info"}),
    ("POST", "/pydmore/notices", {"meta": "s", "label": "x" * 30}),
    ("GET", "/pydmore/notices", None),
    ("GET", "/pydmore/notices?label=info", None),
    ("PATCH", "/pydmore/notices/2", {"a": 1}),
    ("PATCH", "/pydmore/notices/99", {"a": 1}),
    ("GET", "/pydmore/notices", None),
    ("GET", "/pydmore/notices/1/blob", None),
    ("GET", "/pydmore/notices/2/blob?how=plain", None),
    ("GET", "/pydmore/notices/2/blob?how=top", None),
    ("GET", "/pydmore/notices/3/blob", None),
    ("GET", "/pydmore/notices/9/blob", None),
    ("GET", "/pydmore/blobs/h1", None),
    ("GET", "/pydmore/blobs/h1?undeferred=true", None),
    ("GET", "/pydmore/blobs/nope", None),
]
STEPS += [
    ("DELETE", "/pydmore/notices?pattern=%5Ein", None),
    ("DELETE", "/pydmore/notices?pattern=NONE&flags=i", None),
    ("DELETE", "/pydmore/notices?pattern=%5Ex&flags=g", None),
    ("POST", "/stdmore/counter", ["10.0.0.1", "10.0.0.2", "10.0.0.1", "", "b", "10.0.0.2", "10.0.0.1"]),
    ("POST", "/stdmore/counter", []),
    ("POST", "/stdmore/format-map", {"data": {"name": "Zoé", "n": 3, "rent": "1 234,56 €"},
                                      "templates": ["Bonjour {name}, {n:>4} mois, {rent}", "{missing} et {name!r}", "{0} positionnel",
                                                    "{n:05d}", "{name!s:>6}", "{{échappé}} {name}", "{", "plain", "{:>3}"]}),
    # arguments the runtime used to drop (audit of 09/10/2026): fixtures/langapp/kwargs.py
    ("POST", "/kw/encode", {"text": "Bâtiment € 3"}),
    ("POST", "/kw/encode", {"text": "plain"}),
    ("POST", "/kw/encode", {"text": "éé€x😀"}),
    *[("GET", f"/kw/find?s=abcabc&sub={sub}{q}", None) for sub in ("b", "", "c", "zz") for q in (
        "", "&start=2", "&start=2&end=4", "&start=-2", "&start=10", "&start=4&end=2", "&start=0&end=-1", "&start=3&end=3")],
    ("POST", "/kw/seq", [1, 2, 3, 2, 1]),
    ("POST", "/kw/seq", [2]),
    *[("GET", f"/kw/resp/{k}", None) for k in ("text", "html", "redirect", "file", "filehdr")],
    ("GET", "/kw/mime", None),
    ("POST", "/kw/path", {"text": "Clé à 5 $"}),
    ("POST", "/kw/path", {"text": "plain"}),
    ("POST", "/kw/csv", {"text": "a,b,c\n1,2\n1,2,3,4,5\n\n7,8,9\n"}),
    ("POST", "/kw/redis", None),
    ("POST", "/kw/isinstance", {"vals": [1, 2.5, "s", True, [1], {"a": 1}]}),
    ("GET", "/stdmore/tcp", None),
    ("GET", "/stdmore/tcp?host=localhost", None),
    ("GET", "/stdmore/tcp?host=no!such.invalid&port=25", None),  # rejected by the C library, see /libs/ssrf
    ("GET", "/stdmore/tcp?host=.invalid&port=25", None),
    ("GET", "/stdmore/tcp?port=6379", None),
    ("GET", "/stdmore/tcp?port=6379&send=PING", None),
    ("GET", f"/stdmore/tcp?port={BLACKHOLE}&timeout=0.2", None),
]
# secrets (and random.choice): random values by their shape, errors as CPython's (fixtures/langapp/stdmore.py)
_SECRET_CASES = ("choice_str", "choice_list", "choice_tuple", "choice_range", "choice_dict0", "choice_dict", "choice_set",
                 "choice_empty", "choice_int", "random_choice_dict", "below", "below1", "below_true", "below_kw",
                 "below0", "below_neg", "below_float", "below_float0", "below_str", "below_none", "bits0", "bits8", "bits63",
                 "bits_true", "bits_neg", "bits_float", "bits_str", "bytes", "bytes5", "bytes_kw", "bytes_true", "bytes0",
                 "bytes_none", "bytes_str", "bytes_float", "hex_str", "hex_float", "hex_true", "urlsafe_list", "urlsafe7", "entropy")
STEPS += [("GET", f"/stdmore/secrets/{c}", None) for c in _SECRET_CASES]
STEPS += [
    ("GET", "/stdmore/rsa", None),
    ("GET", "/stdmore/rsa?bits=2048", None),
    ("GET", "/stdmore/rsa?bits=1024&e=3", None),
    ("GET", "/stdmore/rsa?bits=1023", None),
    ("GET", "/stdmore/rsa?e=5", None),
]
STEPS += [
    ("PATCH", "/pydmore/domains", {}),
    ("PATCH", "/pydmore/domains", {"catch_all": None}),
    ("PATCH", "/pydmore/domains", {"name": "x", "catch_all": "a@b.c"}),
]
STEPS += [
    ("GET", "/stdmore/sock", None),
    ("GET", "/stdmore/sock?host=localhost", None),
    ("GET", "/stdmore/sock?host=mail!server.invalid&port=587", None),
    ("GET", "/stdmore/sock?host=" + "m" * 64 + "&port=587", None),
    ("GET", f"/stdmore/sock?port={BLACKHOLE}&timeout=0.2", None),
    ("GET", "/stdmore/sock?port=6379", None),
]
STEPS += [
    ("POST", "/stdmore/zip", {"profile.json": '{"a": 1, "b": [1, 2, 3]}' * 20, "é/notes.txt": "héllo", "empty.bin": "",
                              "messages.csv": "id,subject\r\n1,Hello\r\n"}),
    ("POST", "/stdmore/zip", {}),
]
STEPS += [
    ("POST", "/stdmore/lines", {"text": "a\r\nb\rc\n\nd\x0be\x1cf g"}),
    ("POST", "/stdmore/lines", {"text": "trailing\n"}),
    ("POST", "/stdmore/lines", {"text": ""}),
]
STEPS += [
    ("POST", "/pydmore/domains", {"name": "ok.org", "catch_all": " nobody "}),
    ("POST", "/pydmore/domains?catch_all=%20nobody%20", {"name": "ok.org"}),
]


# ---- security review (fixtures/langapp/sec.py, docs/advanced/security.md): request values used as SQL names are quoted,
# values bound (the owners count after each step shows the table intact); symlinks followed by resolve();
# no message in a 500; Starlette's multipart limits; sizes from the request raise instead of aborting
from urllib.parse import quote  # noqa: E402
_SQLI = ["n", "N", "order", 'a"b', "x FROM owners; DROP TABLE owners; --", "' OR 1=1 --", "%_\\", ":n", "$1", "é"]
# the owners and memberships these cases read: those tests/scenarios/dynapp.py's earlier cases left when they ran in
# the same app (fixtures/dynapp), so that every answer stays what it was
STEPS += [("SQL", "INSERT INTO owners (id, name) VALUES (1, 'Ada'), (2, 'ada-lovelace'), (3, 'GRACE-HOPPER'), (4, 'o1'), "
           "(5, 'o2'), (6, 't'), (7, 'u'), (8, 'sp'), (9, 'sp'), (10, 'al-owner'), (11, 'dec'), (12, 'more'), (13, 'lbl'); "
           "INSERT INTO memberships (owner_id, role, note) VALUES (4, 'admin', 'changed'), (7, 'b', NULL), (7, 'a', 'kept'), "
           "(7, 'e', NULL); SELECT setval('owners_id_seq', 13)", None)]
STEPS += [("GET", f"/sec/label?name={quote(v)}", None) for v in _SQLI]
STEPS += [("GET", f"/sec/filter?q={quote(v)}", None) for v in _SQLI]
STEPS += [("GET", f"/sec/text?q={quote(v)}", None) for v in _SQLI]
STEPS += [("GET", f"/sec/sub?name={quote(v)}", None) for v in ("anon", "Sub", "select", 'q"x', "s) AS t; DROP TABLE owners; --")]
STEPS += [("GET", f"/sec/order?sort={quote(v)}", None) for v in ("n", "id; DROP TABLE owners", "n DESC")]
STEPS += [("POST", f"/sec/upsert?col={quote(v)}", None) for v in ("role", "Role", "role) DO NOTHING; DROP TABLE owners; --")]
STEPS += [("GET", f"/sec/resolve?name={quote(v)}", None) for v in (
    "a.txt", "sub/new.txt", "sub/../a", "../escape", "link/new.txt", "link", "sub/../link/x/../y", "/etc/passwd", "a/../../x")]
STEPS += [
    ("GET", "/sec/header?v=plain", None),
    ("GET", "/sec/boom", None),
    ("GET", "/sec/dup", None),
    ("GET", "/sec/token?n=8", None),
    ("GET", "/sec/token?n=0", None),
    ("GET", "/sec/token?n=-1", None),
    ("GET", "/sec/repeat?n=3", None),
    ("GET", "/sec/repeat?n=-2", None),
    ("GET", f"/sec/repeat?n={2**62}", None),
    ("GET", f"/sec/repeat?n={10**15}", None),
]
# fixtures/langapp/sec.py, second review: exponents, digit counts, widths and precisions chosen by the request
_DEC_PAIRS = [("1e999999999", "1"), ("1", "1e-999999999"), ("1e-999999999", "1"), ("-1", "3"), ("-3", "3"),
              ("0", "0E-999999999"), ("0E-5", "1.5"), ("1E+999999", "10"), ("1.5", "1e-1000020"), ("123.456", "0.01"),
              ("5", "1e999999999"), ("1e999999999999999999", "-1e999999999999999999"),
              ("12345678901234567890123456789", "1"), ("-0", "0"), ("9.99E+999999", "1E+999999"), ("1E-1000020", "3")]
STEPS += [("GET", f"/sec/decimal/ops?a={quote(a)}&b={quote(b)}", None) for a, b in _DEC_PAIRS]
STEPS += [("POST", "/sec/decimal", {"price": v}) for v in ("1e999999999", "-1e999999999", "1e-999999999",
                                                            "1e999999999999999999", "1e9999999999999999999", 5)]
STEPS += [("POST", "/sec/decimal/digits", {"amount": v}) for v in ("1" + "0" * 200000, "12.345", "1" + "0" * 10 + ".00",
                                                                  "1e-999999999", "1e-1000026", "1e-1000027")]
STEPS += [("POST", "/sec/fraction", {"r": v}) for v in ("1" * 5000, "1/3", "1." + "1" * 4301)]
STEPS += [("GET", f"/sec/pad?how={how}&{k}={v}", None) for how, k, v in (
    ("zfill", "n", 5), ("zfill", "n", -3), ("zfill", "n", 10**15), ("ljust", "n", 2**62), ("center", "n", 7),
    ("ljust", "n", -1), ("format", "w", "99999999999999999999"), ("format", "w", "9223372036854775807"),
    ("format", "w", "10"), ("precision", "w", "2147483648"), ("precision", "w", "99999999999999999999"),
    ("precision", "w", "3"), ("percent", "w", "99999999999999999999"), ("percent", "w", "10000000000000000000"),
    ("percent", "w", "9223372036854775807"), ("percent", "w", "5"), ("pprec", "w", "9999999999"),
    ("pprec", "w", "99999999999999999999"), ("pprec", "w", "2"))]
STEPS += [("GET", f"/sec/range?start={a}&stop={b}&step={c}", None) for a, b, c in (
    (0, 2**63 - 1, 2**62), (2**63 - 2, -2**63, -2**63 + 1), (-5, 5, 3))]
STEPS += [("GET", f"/sec/regexp?flags={quote(v)}", None) for v in ("i", "c", "x'y", "\\'||(SELECT 1)||'", "'); DROP TABLE owners; --")]
STEPS += [
    # Cookie() parameters (fixtures/langapp/docsx.py, the FastAPI docs' cookie examples)
    ("GET", "/docsx/cookies", None),
    ("GET", "/docsx/cookies", None, {"cookie": "ads_id=abc; n=7; session-id=s1"}),
    ("GET", "/docsx/cookies", None, {"cookie": 'ads_id="q\\"x\\073y"; n=zz; n=3'}),
    ("GET", "/docsx/cookies", None, {"cookie": "n=nope; ads_id"}),
    ("GET", "/docsx/cookie-required", None),
    ("GET", "/docsx/cookie-required?q=x", None, {"cookie": "token=ab", "x-a": "y"}),
    ("GET", "/docsx/cookie-required", None, {"cookie": "token=abcd", "x-a": "2"}),
    # models of parameters
    ("GET", "/docsx/pm/query?limit=10&offset=5&order_by=updated_at&tags=tag1&tags=tag2", None),
    ("GET", "/docsx/pm/query", None),
    ("GET", "/docsx/pm/query?limit=150&offset=-1&order_by=x", None),
    ("GET", "/docsx/pm/query?limit=10&tool=plumbus&tool=x", None),
    ("GET", "/docsx/pm/loose?q=a&page=2&extra=1&extra=2&other=", None),
    ("GET", "/docsx/pm/loose?page=", None),
    ("GET", "/docsx/pm/headers", None, {"host": "testserver", "save-data": "true", "if-modified-since": "yesterday", "traceparent": "123",
                                         "x-tag": "one"}),
    ("GET", "/docsx/pm/headers", None, {"host": "testserver", "save-data": "1"}),
    ("GET", "/docsx/pm/headers", None, {"host": "testserver", "save_data": "true"}),
    # the cookies tests/scenarios/dynapp.py's earlier answers left in the client when these cases ran there
    ("GET", "/docsx/pm/strict-headers", None, {"host": "testserver", "save-data": "true",
                                                "cookie": 's=abc.def-_; q="a b\\073c\\"\\351"; seen=oui'}),
    ("GET", "/docsx/pm/raw-headers", None, {"host": "testserver", "save_data": "true", "x_tag": "a"}),
    ("GET", "/docsx/pm/cookies", None, {"cookie": "session_id=123; fatebook_tracker=good-list-of-cookies; x=1"}),
    ("GET", "/docsx/pm/cookies", None, {"cookie": "fatebook_tracker=1"}),
    ("POST", "/docsx/pm/form", b"username=Foo&password=secret&tags=a&tags=b&extra=e", {"content-type": "application/x-www-form-urlencoded"}),
    ("POST", "/docsx/pm/form", b"username=Foo&remember=", {"content-type": "application/x-www-form-urlencoded"}),
    ("POST", "/docsx/pm/form", b"", {"content-type": "application/x-www-form-urlencoded"}),
    *[("POST", path, *_multipart(parts)) for path, parts in [
        ("/docsx/files/one", [("file", "a.txt", "text/plain", "héllo wörld".encode()), ("token", "t")]),
        ("/docsx/files/one", [("file", "as text"), ("token", "t")]),
        ("/docsx/files/one", [("file", ""), ("token", "")]),
        ("/docsx/files/opt", [("file", "a.bin", None, b"\xff\x00")]),
        ("/docsx/files/opt", [("file", "a.bin", None, b"x")]),
        ("/docsx/files/opt", [("other", "x")]),
        ("/docsx/files/many", [("files", "a", None, b"ab"), ("files", "b", None, b"cde")]),
        ("/docsx/files/many", [("files", "a", None, b"ab"), ("files", "text")]),
        ("/docsx/files/many", [("other", "x")]),
    ]],
    ("POST", "/docsx/files/blob", {"data": "héllo"}),
    ("POST", "/docsx/files/blob", {"data": 1}),
    ("GET", "/docsx/classdep", None),
    ("GET", "/docsx/classdep?q=a&skip=2&limit=5", None),
    ("GET", "/docsx/classdep?skip=x&limit=99", None),
    ("POST", "/docsx/encoder", {"title": "t", "when": "2024-01-02T03:04:05+02:00", "tags": ["a"], "price": "2.25"}),
    ("POST", "/docsx/encoder", {"title": "t", "when": "2024-01-02T03:04:05.123456"}),
    ("PUT", "/docsx/when", {"process_after": 300, "repeat_at": "15:30:00"}),
    ("PUT", "/docsx/when", {"process_after": 1.5, "repeat_at": "15:30:00.123"}),
    ("PUT", "/docsx/when", {"process_after": "P1DT2H3M4.5S", "repeat_at": "25:00"}),
    ("PUT", "/docsx/when", {"process_after": "-1 day, 23:00:00"}),
    ("PUT", "/docsx/when", {"process_after": "300", "repeat_at": "1530"}),
    ("PUT", "/docsx/when", {"process_after": True, "repeat_at": True}),
    ("PUT", "/docsx/when", {"process_after": 1e20, "repeat_at": None}),
    *[("POST", "/docsx/schedule", {"every": v}) for v in [
        300, -1, 0, 0.5, "PT90M", "P1Y38D", "P1W", "10:20", "1d", "100:00:00", "PT1.5H30M", "P", "-PT5S",
        "10:20:30.1234567", "86399999913600"]],
    ("POST", "/docsx/schedule", {"every": 86399999913600}),
    ("POST", "/docsx/schedule", {"every": "1 day, 01:00:00", "at": "23:59:59.999999"}),
    *[("GET", f"/docsx/models/{m}", None) for m in ("alexnet", "resnet", "lenet", "vgg")],
    # the request's exception raised at a dependency's `yield`, Depends(scope=) (fixtures/langapp/corpusx.py)
    *[("GET", f"/corpusx/owner/{i}", None) for i in ("plumbus", "missing", "portal-gun")],
    *[("GET", f"/corpusx/swallow/{i}", None) for i in ("plumbus", "foo", "portal-gun")],
    ("GET", "/corpusx/log", None),
    *[("GET", f"/corpusx/reraise/{i}", None) for i in ("plumbus", "portal-gun")],
    ("GET", "/corpusx/log", None),
    *[step for m in ("ok", "http", "value", "key", "ok?n=x") for step in
      (("GET", f"/corpusx/any/{m}", None), ("GET", "/corpusx/log", None))],
    ("GET", "/corpusx/scope/function", None),
    ("GET", "/corpusx/scope/late", None),
    # (the request-scoped exit code runs once the response is sent: a following /log races with it in Python)
    ("GET", "/corpusx/scope/request", None),
    ("GET", "/corpusx/scope/raises", None),
    # unannotated parameters, a Response class as the return annotation, response_model_* options
    ("GET", "/corpusx/untyped/7", None),
    ("GET", "/corpusx/untyped/x?q=1&q=2&n=abc", None),
    ("GET", "/corpusx/teleport", None),
    ("GET", "/corpusx/teleport?to=https://example.com/a%20b", None),
    ("GET", "/corpusx/json-ann", None),
    ("GET", "/corpusx/resp-ann", None),
    ("GET", "/corpusx/resp-ann?raw=1", None),
    *[("GET", f"/corpusx/rm/{k}/{key}", None) for k in ("unset", "unset-model", "none", "include", "exclude", "ann")
      for key in ("foo", "bar", "baz")],
    # an instance with __call__ as a dependency, FastAPI's default exception handlers delegated to
    ("GET", "/corpusx/checker", None),
    ("GET", "/corpusx/checker?q=foobar&k=x&m=3", None),
    ("GET", "/corpusx/checker?m=x", None),
    ("GET", "/corpusx/checker-pre?m=2", None),
    ("GET", "/corpusx/nope", None),
    # val_json_bytes / ser_json_bytes (base64, hex)
    *[("POST", "/corpusx/bytes/b64", {"description": "d", "data": d}) for d in (
        "aGk=", "aGk", "aGk-_w==", "aGk+/w==", "aGk==", "a", "aG=k", "a+-/", "YR==", "", "é", 12, None)],
    *[("POST", "/corpusx/bytes/hex", body) for body in (
        {"data": "6869"}, {"data": "686"}, {"data": "zz"}, {"data": "6G"}, {"data": "ABcd", "more": ["00", "ff"]},
        {"data": "00", "more": ["0"]}, {"data": "00", "opt": "0a"}, {"data": "00", "inner": {"description": "x", "data": "_w"}})],
    ("POST", "/corpusx/bytes/plain", {"data": "aGk=", "inner": {"description": "x", "data": "aGk="}}),
    ("GET", "/corpusx/bytes/out", None),
    ("GET", "/corpusx/bytes/out?raw=", None),
    # pwdlib's recommended Argon2 hasher
    *[("GET", f"/corpusx/pwd?{q}", None) for q in ("", "password=nope", "which=own&password=x", "which=weak&password=pw",
                                                   "which=weak&password=no", "which=bcrypt")],
    # generator endpoints: JSON Lines, Server-Sent Events, raw (an exception mid-stream cuts the connection)
    ("GET", "/corpusx/gen/jsonl", None),
    ("GET", "/corpusx/gen/jsonl?n=0", None),
    ("GET", "/corpusx/gen/jsonl?n=x", None),
    ("GET", "/corpusx/gen/jsonl-bad", None),
    ("GET", "/corpusx/gen/jsonl-any", None),
    ("GET", "/corpusx/gen/sse", None),
    ("GET", "/corpusx/gen/sse", None, {"last-event-id": "0"}),
    ("GET", "/corpusx/gen/sse-items", None),
    ("GET", "/corpusx/gen/sse-plain", None),
    ("GET", "/corpusx/gen/sse-bad", None),
    ("GET", "/corpusx/gen/sse-bad?kind=event", None),
    ("GET", "/corpusx/gen/raw", None),
    ("GET", "/corpusx/gen/raw-bad", None),
    # a StreamingResponse chunk neither str nor bytes: AttributeError in Starlette's `chunk.encode()`, the
    # connection dropped (the binary converted it with str() before)
    ("GET", "/corpusx/stream/bad-chunk", None), ("GET", "/corpusx/stream/bad-list", None),
]


def _multipart_limits() -> list:
    steps = []
    for n in (1000, 1001):
        body, h = _multipart([("files", f"{i}.txt", "text/plain", b"x") for i in range(n)])
        steps.append(("POST", "/sec/upload", body, h))
    for n in (1000, 1001):
        body, h = _multipart([(f"f{i}", "v") for i in range(n)])
        steps.append(("POST", "/sec/upload", body, h))
    for size in (1024 * 1024, 1024 * 1024 + 1):
        body, h = _multipart([("caption", "x" * size)])
        steps.append(("POST", "/sec/upload", body, h))
    body, h = _multipart([("files", "big.bin", "text/plain", b"x" * (2 * 1024 * 1024))])
    steps.append(("POST", "/sec/upload", body, h))
    for name in ("C:\\Users\\x\\evil.txt", "\\\\host\\share\\f.txt", "../../etc/passwd", "a\\b.txt", "D:rel.txt"):
        body, h = _multipart([("files", name, "text/plain", b"x")])
        steps.append(("POST", "/sec/upload", body, h))
    return steps


STEPS += _multipart_limits()


def _realworld_cases() -> list:
    """fixtures/langapp/realworld.py: aliases, closures and nonlocal, TypedDict, Model.__table__, StaleDataError, html.unescape."""
    steps = [("GET", "/realworld/notices", None), ("POST", "/realworld/notices", None), ("GET", "/realworld/notices", None),
             ("GET", "/realworld/notices/read", None), ("GET", "/realworld/notices/orm", None), ("GET", "/realworld/notices/first", None)]
    for body in ({"userName": "a", "town": "Lyon", "zip_code": "69"}, {"user_name": "a", "c": "Lyon"}, {"userName": "a", "c": "x"},
                 {}, {"userName": 3, "zip": "1"}):
        steps += [("POST", "/realworld/renamed", body), ("POST", "/realworld/renamed/dump", body)]
    for body in ({"fullName": "A B"}, {"full_name": "A B"}, {"name": "A B"}, {"fullName": 1}):
        steps.append(("POST", "/realworld/populated", body))
    steps += [("POST", "/realworld/nonlocal", {"codes": [" 45000000 ", "72000000", "45000000", ""]}),
              ("POST", "/realworld/nonlocal", {"codes": []}),
              ("GET", "/realworld/typeddict?amount=2.5", None), ("GET", "/realworld/typeddict?amount=-1", None),
              ("PATCH", "/realworld/notices/1/fields", {"metadata": {"a": 1}, "lbl": "x", "extra_data": 1, "nid": 3}),
              ("PATCH", "/realworld/notices/99/fields", {}),
              ("POST", "/realworld/notices/2/stale", None), ("POST", "/realworld/notices/2/stale", None), ("GET", "/realworld/notices", None)]
    texts = ["plain", "&amp &ampx &notit; &notin &#128;x &#0; &#xD800; &#1114112; &#11; &#x110000 &#65 &;&#; &#x;",
             "&abcdefghijklmnopqrstuvwxyzabcdefghij; &\u00e9; &AMP;&#X41 &\tx &lt\n&gt\r&quot", "&#" + "0" * 4298 + "65;",
             "&#" + "1" * 4301 + ";", "&#x" + "0" * 5000 + "41;", "caf&eacute; &CounterClockwiseContourIntegral; &nGt; &#x1F600;",
             "&" * 5 + "amp;" * 3, 42, None]
    steps.append(("POST", "/realworld/unescape", {"texts": texts}))
    steps.append(("POST", "/realworld/notices/json", None))
    items = [{"title": "Marché, lot 1; maintenance \\ logiciel", "day": "2026-11-30", "lines": ["Acheteur: Ville", "é" * 60]},
             {"title": "x" * 200, "day": "2027-01-01", "before": {"hours": -3}},
             {"title": "Questions — " + "à" * 50, "day": "2026-12-01", "lines": ["a\r\nb", "\\N"], "before": {"days": -1, "hours": 2}},
             {"title": "t", "day": "2026-12-02", "before": {"seconds": 90}}, {"title": "z", "day": "2026-12-03", "before": {}}]
    steps += [("POST", "/realworld/calendar.ics", {"items": items}), ("POST", "/realworld/calendar.ics", {"items": []}),
              ("POST", "/realworld/remove", None)]
    return steps


STEPS += _realworld_cases()


def _pyver_cases() -> list:
    """fixtures/langapp/pyver.py (every Python), py314.py and py315.py (from those Pythons only: the conformance
    runs on the reference app's Python)."""
    import sys

    from fixtures.langapp.pyver import CASES

    steps = [("GET", f"/pyver/{name}", None) for name in CASES]
    steps += [("GET", f"/pyver/cm/{k}", None) for k in ("async-lock", "sync-semaphore", "int")]
    from fixtures.langapp.pyver import AIO_DELAYS
    steps += [("GET", f"/pyver/aio/{fn}/{k}", None) for fn in ("sleep", "sleep_kw", "wait_for") for k in AIO_DELAYS
              # a delay wait_for refuses leaves its coroutine never awaited: a RuntimeWarning on stderr in Python
              if fn != "wait_for" or k not in ("decimal", "str")]
    steps += [("GET", f"/pyver/mime/{n}", None) for n in (
        "a.yaml", "a.yml", "a.md", "a.wav", "a.avi", "a.woff2", "a.ttf", "a.otf", "a.gz", "a.tar.gz", "a.tgz", "a.docx",
        "a.avif", "a.heic", "a.mjs", "a.js", "a.json", "a.webp", "a.epub", "a.7z", "a.rar", "a.ogg", "a.flac", "a.mkv",
        "a.m4a", "a.php", "a.glb", "a.stl", "a.ics", "a.PDF", "a.jpeg", "a.svg", "a.csv", "a.xlsx", "a.odt", "a.weba",
        "a.ai", "a.texi", "a.unknownext", ".bashrc", "noext", "a.tar.bz2", "a.svgz", "a.json.gz")]
    if sys.version_info >= (3, 14):
        steps += [("GET", "/py314/except?v=4", None), ("GET", "/py314/except", None), ("GET", "/py314/finally", None),
                  ("POST", "/py314/forward", {"inner": {"v": "3"}, "items": [{"v": 1}]}),
                  ("POST", "/py314/forward", {"inner": {"v": "x"}}), ("POST", "/py314/forward", {}),
                  ("GET", "/py314/lazy-annotation?n=2", None), ("GET", "/py314/uuid7", None),
                  ("GET", "/py314/pattern-error", None), ("GET", "/py314/pattern-error?p=a%2B", None)]
    if sys.version_info >= (3, 15):
        steps += [("GET", "/py315/unpack", None), *[("GET", f"/py315/unpack-error/{k}", None) for k in ("list", "set", "gen", "dict")],
                  ("GET", "/py315/prefixmatch", None), ("GET", "/py315/prefixmatch?s=baa", None),
                  ("GET", "/py315/padded", None), ("GET", "/py315/padded?s=YWJjYQ%3D%3D", None),
                  *[("GET", f"/py315/match?v={v}", None) for v in (1, -1, 2)]]
    return steps


STEPS += _pyver_cases()

# Model.model_construct(), functions held as staticmethod(...) class attributes, `from m import *`
# (fixtures/langapp/construct.py)
STEPS += [("GET", f"/construct/{p}", None) for p in (
    "basic", "aliases", "fields-set", "extra", "defaults", "nested", "private", "inherited", "on-instance", "spread",
    "spread?name=x&kind=y", "response", "response-unset", "list", "static", "static", "star/200", "star/418", "star/500")]

# documented differences of the log conformance (tests/logcmp.py): records left out on both sides
LOG_IGNORE = []
if __import__("sys").version_info >= (3, 14):
    # docs/supported.md "Deeply nested JSON on Python 3.14+" (and docs/python-versions.md): from 3.14, json's
    # decoder is bounded by the C stack, not by 10 000 levels; the deep
    # document of /edges/loads passes json.loads, then FastAPI's jsonable_encoder exceeds the recursion limit.
    # The binary keeps the 10 000 levels (RecursionError in json.loads): the same 500, another traceback
    LOG_IGNORE.append(r"RecursionError: maximum recursion depth exceeded")

# ---- Model.__table__ as a Core table, aliased with_polymorphic() (fixtures/langapp/ormx.py)
STEPS += [
    ("GET", "/ormx/table/join", None),
    *[("GET", f"/ormx/table/outerjoin?how={h}", None) for h in ("outerjoin", "isouter", "full", "join")],
    ("GET", "/ormx/table/chain", None),
    ("GET", "/ormx/table/rows", None),
    ("GET", "/ormx/table/name", None),
    ("GET", "/ormx/poly/aliased", None),
    ("GET", "/ormx/poly/aliased?flat=false", None),
    ("GET", "/ormx/poly/filter", None),
    ("GET", "/ormx/poly/star", None),
]
