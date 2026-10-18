"""Scenario of fixtures/integapp: the libraries and the platform around the application (bcrypt, itsdangerous,
PyJWT, pywebpush, alembic, prometheus_client, logging, retries, tracing, aio-pika, outbound HTTP, background loops,
ATOM's small apps). Cases moved from tests/scenarios/dynapp.py, in their order; dynapp's tables
(`reset_tables`)."""
import os
import urllib.parse

# get_db commits after the response (FastAPI >= 0.121): pause after each write so that the reference has
# committed before the next request reads (the binary commits before answering)
SETTLE = 0.2
# the pause only after the writes that can do work after their response (tests/settle.py)
REFERENCE_APP = "fixtures.integapp.main:app"


def _pyjwt_cases() -> list:
    """Tokens built by PyJWT itself (fixed dates, or a margin of an hour around now): fixtures/integapp/pyjwt_auth.py."""
    import time
    import warnings
    from urllib.parse import quote

    import jwt

    warnings.filterwarnings("ignore", category=jwt.InsecureKeyLengthWarning)
    k = "s3cret-pyjwt-0123456789abcdef-0123"
    far, past, now = 4102444800, 946684800, int(time.time())
    good = jwt.encode({"sub": "ada@example.com", "exp": far, "type": "access", "n": 1.5, "l": [1, "é"]}, k)
    tokens = [
        good,
        jwt.encode({"sub": "ada", "exp": past}, k),
        jwt.encode({"sub": "ada", "exp": now - 5}, k),
        jwt.encode({"aud": "app", "iss": "idp"}, k),
        jwt.encode({"sub": "ada"}, "other-key-0123456789abcdef-012345"),
        jwt.encode({"sub": "ada"}, k, algorithm="HS384"),
        jwt.encode({"sub": "ada"}, k, algorithm="HS512", headers={"kid": "k1"}),
        jwt.encode({"sub": "ada"}, None, algorithm="none"),
        good[:-3] + "AAA", good + "=", good + "==", good + "===", good[:-1], good + "!!", good.replace(".", ".=", 1),
        "abc", "a.b", "a.b.c", "....", good.replace(".", "..", 1), "e30.e30.", "W10.e30.", "bm90.e30.", "_w.e30.",
        "eyJhbGciOiJIUzI1NiJ9.WzFd." + good.rsplit(".", 1)[1],
    ]
    steps = [("GET", f"/pyjwt/check?token={quote(t)}", None) for t in tokens]
    steps += [
        ("GET", f"/pyjwt/check?token={quote(tokens[2])}&leeway=3600", None),
        ("GET", f"/pyjwt/check?token={quote(tokens[3])}&aud=app&iss=idp", None),
        ("GET", f"/pyjwt/check?token={quote(tokens[3])}&aud=other", None),
        ("GET", f"/pyjwt/check?token={quote(tokens[3])}", None),
        ("GET", f"/pyjwt/check?token={quote(tokens[3])}&aud=app&iss=nope", None),
        ("GET", f"/pyjwt/check?token={quote(tokens[5])}&algs=HS384", None),
        ("GET", f"/pyjwt/check?token={quote(tokens[6])}&algs=HS256&algs=HS512", None),
        ("GET", f"/pyjwt/check?token={quote(tokens[7])}&algs=none", None),
        ("GET", f"/pyjwt/check?token={quote(good)}&key=%22s3cret%22", None),
        ("GET", f"/pyjwt/check?token={quote(good)}&key=", None),
        ("POST", "/pyjwt/issue", {"sub": "ada", "roles": ["a"], "é": "ü"}),
        ("POST", "/pyjwt/issue?alg=HS512&kid=key-1", {"sub": "ada"}),
        ("POST", "/pyjwt/issue?alg=HS384&kid=key-1&sort=false", {"sub": "ada"}),
        ("POST", "/pyjwt/issue?alg=none", {"sub": "ada"}),
        ("POST", "/pyjwt/issue?alg=XX", {"sub": "ada"}),
        ("POST", "/pyjwt/issue", {"sub": "ada", "iss": 5}),
        ("GET", "/pyjwt/me", None, {"authorization": f"Bearer {good}"}),
        ("GET", "/pyjwt/me", None, {"authorization": f"Bearer {tokens[1]}"}),
        ("GET", "/pyjwt/me", None, {"authorization": "Bearer nope"}),
        ("GET", "/pyjwt/me", None),
        ("GET", "/pyjwt/roundtrip", None),
        ("GET", "/pyjwt/cases", None),
    ]
    return steps

STEPS: list = [    *_pyjwt_cases(),
    # aio_pika (fixtures/integapp/amqp.py; RabbitMQ at BROKER_DSN)
    ("POST", "/amqp/roundtrip/alpha", None),
    ("GET", "/amqp/down", None),
    # tenacity (fixtures/integapp/retrying.py)
    ("GET", "/retry/case/flaky?arg=0", None),
    ("GET", "/retry/case/flaky?arg=2", None),
    ("GET", "/retry/case/flaky?arg=3", None),
    ("GET", "/retry/case/reraise", None),
    ("GET", "/retry/case/wrapped", None),
    ("GET", "/retry/case/callback?arg=7", None),
    ("GET", "/retry/case/typed-value", None),
    ("GET", "/retry/case/typed-other", None),
    ("GET", "/retry/case/pred-again", None),
    ("GET", "/retry/case/pred-stop", None),
    ("GET", "/retry/case/result?arg=1", None),
    ("GET", "/retry/case/result?arg=5", None),
    ("GET", "/retry/case/sync", None),
    ("GET", "/retry/case/bare", None),
    # prometheus_client (fixtures/integapp/prom.py); timer durations only in /prom/timers, last
    ("GET", "/prom/metrics", None),
    ("GET", "/prom/module", None),
    ("POST", "/prom/run", None),
    ("GET", "/prom/metrics", None),
    ("GET", "/prom/nocreated", None),
    ("GET", "/prom/default", None),
    ("GET", "/prom/targeted", None),
    ("GET", "/prom/single", None),
    ("GET", "/prom/errors", None),
    ("POST", "/prom/mutate", None),
    ("GET", "/prom/metrics", None),
    ("POST", "/prom/reset", None),
    ("GET", "/prom/exporter", None),
    ("GET", "/prom/timers", None),
    ("GET", "/bg/run", None),
    ("GET", "/bg/run", None),
    ("GET", "/bg/misc", None),
    # bytes.startswith(tuple) (fixtures/integapp/small.py), then json.loads on malformed bodies: CPython's
    # messages and code-point positions, NaN/inf in a 422 (JSONResponse refuses them: 500), BOM, UTF-16
    ("POST", "/small/sniff", {"b64": "iVBORw0KGgo="}),
    ("POST", "/small/sniff", {"b64": "/9j/2Q=="}),
    *[("POST", "/small/sniff", body) for body in [
        b"", b"{", b"[1,]", b'{"b64":"x",}', b'{"b64" "x"}', b"{1:2}", b'{"b64":"a\\x"}', b'{"b64":"\\u12"}',
        b"01", b"-", b"NaN", b'{"b64": Infinity}', b"tru", b'{"b64":"a\nb"}',
        b'\xef\xbb\xbf{"b64": "AA=="}', b'\xff\xfe{\x00}\x00', b"\xc3\x28", b'{"b64": "AA==", "b64": "iVBO"}',
        b'"\\ud83d\\ude00"', b"[1]x", b'{"b64": "\xc3\xa9"}  x',
    ]],
    # third-party libraries (fixtures/integapp/libs.py)
    ("GET", "/libs/bcrypt", None),
    ("GET", "/libs/bcrypt?pw=%C3%A9t%C3%A9%20%F0%9F%8C%9E", None),
    ("GET", "/libs/totp", None),
    ("POST", "/libs/thread", None),
    ("GET", "/libs/sent", None),
    ("POST", "/libs/alert/one", None),
    ("POST", "/libs/alert/two", None),
    ("GET", "/libs/http", None),
    ("GET", "/outbound/run", None),
    ("GET", "/traced/run", None),
    ("GET", "/traced/run", None),
    ("GET", "/libs/yarl", None),
    # pywebpush: tests/push_sink.py on port 8299 (started by scripts_start_dyn.sh)
    ("POST", "/libs/push?target=http://127.0.0.1:8299/sink", None),
    ("POST", "/libs/push?target=http://127.0.0.1:8299/sink&der=true&size=9000", None),
    ("POST", "/libs/push?target=http://127.0.0.1:8299/sink%3Fgone%3Dtrue", None),
    ("POST", "/libs/push?target=http://127.0.0.1:8299/sink&sub=admin", None),
    ("POST", "/libs/push-errors", None),
    ("POST", "/libs/savepoint", None),
    ("POST", "/libs/savepoint", None),
    ("GET", "/libs/enum", None),
    ("GET", "/libs/enum?rank=high", None),
    ("GET", "/libs/enum?rank=Estar", None),
    ("GET", "/libs/enum?rank=", None),
    ("GET", "/libs/enum?rank=zz", None),
    ("POST", "/libs/enum", {"rank": "ESTAR", "ranks": ["low", "", "estar"]}),
    ("POST", "/libs/enum", {"rank": "bad", "ranks": ["low", "worse"]}),
    *[("POST", "/libs/enum-missing", {"grade": g}) for g in ("a", "ve", "ke", "ae", "up", "wr", "zz", 1)],
    ("GET", "/libs/urllib", None),
    ("GET", "/libs/b64", None),
    ("GET", "/libs/psutil", None),
    ("GET", "/libs/sqlmisc", None),
    ("GET", "/libs/google", None),
    ("GET", "/libs/aliased", None),
    ("GET", "/libs/decimal", None),
    ("POST", "/libs/decimal-db", None),
    ("GET", "/libs/formatdate", None),
    ("GET", "/libs/smalllibs", None),
    ("GET", "/libs/unicodedata", None),
    ("GET", "/libs/unicodedata?s=", None),  # CPython returns "" before checking the form
    ("GET", "/libs/jsonfmt", None),
    ("GET", "/libs/datereplace", None),
    ("POST", "/libs/lastpositive", {"items": []}),
    ("POST", "/libs/lastpositive", {"items": [1, 2, 3, 4]}),
    ("POST", "/libs/lastpositive", {"items": [1, "x"]}),
    ("POST", "/libs/lastpositive", {"items": [1, -2]}),
    ("POST", "/libs/lastpositive", {"items": [3]}),
    ("POST", "/libs/lastpositive", {"items": ["3", "-2"]}),  # the error's input is the raw list
    ("POST", "/libs/lastpositive", {"items": [1], "tags": [1, 2], "meta": {"a": 1, "b": 2}}),
    ("POST", "/libs/lastpositive", {"items": [1], "tags": [1, 1], "meta": {"a": 1}, "pair": [4], "names": ["x"]}),
    ("POST", "/libs/lastpositive", {"items": [1, "x", 3, 4, 5], "pair": [1, "x", 3], "names": [1, 2, 3]}),
    ("POST", "/libs/lastpositive", {"items": ["x"], "pair": ["y"]}),
    ("POST", "/libs/lastpositive", {"items": [2], "tags": ["x", 1, 2], "pair": [1, 2, 3]}),
    ("GET", "/libs/unicodedata?s=%C3%85ngstr%C3%B6m%20%E2%84%AB%20%EA%9F%B1%20%F0%90%BB%BA%CC%A7", None),
    ("GET", "/libs/shapes", None),
    ("GET", "/libs/ipaddr", None),
    ("GET", "/libs/mimebase", None),
    ("GET", "/libs/template", None),
    ("GET", "/libs/ssrf?host=localhost", None),
    ("GET", "/libs/ssrf?host=127.0.0.1", None),
    ("GET", "/libs/ssrf?host=::1", None),
    # an unresolvable name: one the C library rejects itself (`!`), with no DNS query: a real resolver may answer
    # NXDOMAIN to one query and NODATA to the next (CI's CoreDNS, 13/10/2026: [Errno -2] then [Errno -5])
    ("GET", "/libs/ssrf?host=no!such.invalid", None),
    # CPython's idna codec, before any lookup: a label of 64 characters, an empty label (3.12: UnicodeError)
    ("GET", "/libs/ssrf?host=" + "a" * 64 + ".invalid", None), ("GET", "/libs/ssrf?host=x..invalid", None),
    ("GET", "/libs/ssrf?host=x." + "b" * 70, None),
    ("POST", "/libs/sanitize", {"html": "<p onclick=x>Hi <a href=\"javascript:alert(1)\">x</a> <a href=cid:abc target=_blank>y</a>"
                                        "<script>bad()</script><!-- c --><b class=\"k z\" style=\"color:red\">b</b>"
                                        "<img src=x onerror=1><style>p{}</style><table><td colspan=2>t</td></table></p>"}),
    ("POST", "/libs/sanitize", {"html": "plain & <text> \"q\" é"}),
    ("GET", "/libs/deque", None),
    ("GET", "/libs/deque", None),
    ("GET", "/libs/deque?key=b", None),
    ("GET", "/libs/deque", None),
    ("POST", "/libs/tagged", {"name": "a", "tags": None}),
    ("POST", "/libs/tagged", {"name": "", "mode": "m", "tags": ["t"]}),
    ("POST", "/libs/tagged", {"name": 5, "tags": ["t"]}),
    ("GET", "/libs/http-auth", None),
    ("POST", "/libs/bill", {"qty": 3, "unit": "12.50"}),
    ("POST", "/libs/bill", {"qty": 10, "unit": 11, "note": "rush", "extra": [1]}),
    ("POST", "/libs/bill", {"qty": 1, "unit": "1", "other": 99}),
    ("POST", "/libs/bill", {"qty": 0, "unit": 0.0, "": None, "x": None}),  # exclude_none drops None extras
    ("POST", "/libs/bill", {"qty": "x"}),
    ("POST", "/libs/bill-echo", {"qty": 9, "unit": "12", "note": "n"}),
    ("POST", "/libs/decimal-in", {"amount": 1}),
    ("POST", "/libs/decimal-in", {"amount": 1.1}),
    ("POST", "/libs/decimal-in", {"amount": "1.10"}),
    ("POST", "/libs/decimal-in", {"amount": " 2.5 "}),
    ("POST", "/libs/decimal-in", {"amount": "1_000"}),
    ("POST", "/libs/decimal-in", {"amount": "1e3"}),
    ("POST", "/libs/decimal-in", {"amount": 1e15}),
    ("POST", "/libs/decimal-in", {"amount": 0.30000000000000004}),
    ("POST", "/libs/decimal-in", {"amount": "-0"}),
    ("POST", "/libs/decimal-in", {"amount": 1e-7}),
    ("POST", "/libs/decimal-in", {"amount": "abc"}),
    ("POST", "/libs/decimal-in", {"amount": True}),
    ("POST", "/libs/decimal-in", {"amount": None}),
    ("POST", "/libs/decimal-in", {"amount": [1]}),
    ("POST", "/libs/decimal-in", {"amount": "NaN"}),
    ("POST", "/libs/decimal-in", {"amount": "-Infinity"}),
    ("POST", "/libs/decimal-in", {"amount": "snan"}),
    ("POST", "/libs/decimal-in", {}),
    ("POST", "/libs/decimal-in", {"amount": 1, "fee": "0.5"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "0"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": -1.5}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "600"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "500.5"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "1E+3"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "1E+2"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "123.456"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "12.345"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "100.000"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "1.2300"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "wide": "0.12345678901234567890123456789"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "wide": "1.234567890123456789012345678901"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "wide": "1.2345678901234567890123456785"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "wide": "1.2345678901234567890123456775"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "wide": "9.9999999999999999999999999999"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "wide": "12345678901234567890123456789"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "wide": "0.1000000000000000000000000000000"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "wide": "1.23456789012345678901234567"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "-1234.567"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "small": 0.05}),
    ("POST", "/libs/decimal-in", {"amount": 1, "small": 10}),
    ("POST", "/libs/decimal-in", {"amount": 1, "small": "9.95"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "small": "9.90"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "digits": "0.00001"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "digits": "1E+1"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "digits": 0}),
    ("POST", "/libs/decimal-in", {"amount": "x", "fee": "y", "capped": 0}),
    ("POST", "/libs/decimal-echo", {"amount": "1.10", "fee": 2, "small": 0.5}),
    ("POST", "/libs/decimal-echo", {"amount": 1e20, "capped": "1.5"}),
    ("POST", "/libs/decimal-out", {"amount": 1e20, "fee": 0.1, "capped": "2"}),
    ("POST", "/libs/decimal-out", {"amount": "bad"}),
    ("GET", "/libs/sqlmore", None),
    ("GET", "/libs/sets", None),
    ("GET", "/libs/startup", None),
    ("GET", "/libs/numtypes", None),
    ("GET", "/libs/script-settings", None),
    ("GET", "/libs/none-settings", None),
    ("GET", "/libs/jose-options", None),
    ("POST", "/libs/urls", {"site": "http://Example.COM/a b?x=1#frag", "any": "x:y", "redis": "redis://h"}),
    ("POST", "/libs/urls", {"site": "https://a.b:443/"}),
    ("POST", "/libs/urls", {"site": "ftp://x", "any": "http://", "redis": "http://x"}),
    ("POST", "/libs/urls", {"site": 5, "any": "nope", "redis": "redis://"}),
    ("POST", "/libs/urls", {"site": "http://" + "a" * 2100 + ".com"}),
    ("GET", "/libs/urllib?s=%F0%9F%8C%9E%20x%3B%3A%40%26%3D%2B%24%2C", None),
    # small ATOM apps (fixtures/integapp/small.py)
    ("GET", "/small/html?name=<b>", None),
    ("GET", "/small/registry/base", None),
    ("GET", "/small/registry/double?n=4", None),
    ("GET", "/small/registry/triple?n=4", None),
    ("GET", "/small/registry/upper?n=7", None),
    ("GET", "/small/registry/count", None),
    ("GET", "/small/registry/nope", None),
    ("GET", "/small/html-kinds/bytes", None),
    ("GET", "/small/html-kinds/none", None),
    ("GET", "/small/html-kinds/obj", None),
    ("GET", "/small/html-kinds/dict", None),
    ("GET", "/small/html-kinds/texte", None),
    ("POST", "/small/html-created", None),
    ("DELETE", "/small/html-gone", None),
    ("GET", "/small/html-status", None),
    ("GET", "/small/llms.txt", None),
    ("GET", "/small/raw", None),
    ("GET", "/small/go", None),
    ("GET", "/small/go?to=/ailleurs", None),
    ("GET", "/small/go-found", None),
    ("GET", "/small/file", None),
    ("POST", "/small/analyses", {"address": "1 rue A", "docs": [{"name": "pv"}]}),
    ("POST", "/small/analyses", {"id": "11111111-2222-4333-8444-555555555555", "address": "2 rue B", "docs": [{"name": "pv", "ref": "AAAAAAAA-BBBB-4CCC-9DDD-EEEEEEEEEEEE"}, {"name": "charges"}]}),
    ("POST", "/small/analyses", {"id": "{aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee}", "address": "3 rue C"}),
    ("POST", "/small/analyses", {"id": "urn:uuid:aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee", "address": "dup"}),
    ("POST", "/small/analyses", {"id": 5, "address": "x", "docs": [{"name": "a", "ref": "1234"}, {"name": "b", "ref": "1234567g-1234-5678-1234-567812345678"}]}),
    ("POST", "/small/analyses", {"id": "12345678-1234-5678-1234567812345678", "address": "x", "docs": [{"name": "a", "ref": ""}, {"name": "b", "ref": "12345678-1234-5678-1234-56781234567"}, {"name": "c", "ref": "ééé"}]}),
    ("GET", "/small/analyses/11111111-2222-4333-8444-555555555555", None),
    ("GET", "/small/analyses/aaaaaaaabbbb4ccc9dddeeeeeeeeeeee", None),
    ("GET", "/small/analyses/00000000-0000-0000-0000-000000000000", None),
    ("GET", "/small/analyses/nope", None),
    ("GET", "/small/analyses?ids=11111111-2222-4333-8444-555555555555&ids=aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee", None),
    ("GET", "/small/analyses?ids=11111111-2222-4333-8444-555555555555&ids=bad", None),
    ("GET", "/small/analyses", None),
    ("GET", "/small/uuid-ops/11111111-2222-4333-8444-555555555555?other=11111111222243338444555555555555", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=%7B1234567g-1234-5678-1234-567812345678%7D", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=urn:uuid:1234567-81234-5678-1234-567812345678", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=%7Baaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa%7D", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=a%C3%A9", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=%7B12345678-1234-5678-1234-5678123456789%7D", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=urn:uuid:12345678-1234-5678-1234-56781234567", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=%7B1234%7D", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=------------------------------------", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=12345678-1234-5678-1234-567812345678%7D", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=urn:uuid:aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee", None),
    # uuid crate of pydantic-core 2.46 (uuid 1.23.0) vs 2.50 (uuid 1.23.4): positions, length, requested form
    *[("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=" + urllib.parse.quote(u), None) for u in (
        "{}", "{", "urn:uuid:", "urn:uuid:123", "g" * 32, "-" * 32, "a" * 31, "a" * 33, "a" * 46, "a" * 100,
        "{12345678-1234-5678-1234-567812345678", "urn:uuid:12345678-1234-5678-1234-56781234567g",
        "URN:UUID:12345678-1234-5678-1234-567812345678", "12345678123456781234567812345678-", "šššš",
        "12345678-1234-5678-1234-56781234567š", "1234-5678", "12345678-1234-5678-1234-5678-12345678",
        "12345678_1234_5678_1234_567812345678", "{1234567812345678123456781234567}", "ĭĭĭĭ",
        "12345678-1234-5678-1234-5678123456ĭ", "{-}", "{" + "a" * 44 + "}")],
    ("GET", "/small/basic-opt", None),
    ("GET", "/small/basic-opt", None, {"authorization": 'Bearer x'}),
    ("GET", "/small/basic-opt", None, {"authorization": 'Basic !!!'}),
    ("GET", "/small/basic-opt", None, {"authorization": 'Basic YWRtaW46czNjcmV0'}),
    ("GET", "/small/basic-opt", None, {"authorization": 'basic YWRtaW46czNjcmV0'}),
    ("GET", "/small/basic-opt", None, {"authorization": 'Basic YWRtaW46bm9wZQ=='}),
    ("GET", "/small/basic-opt", None, {"authorization": 'Basic bm9jb2xvbg=='}),
    ("GET", "/small/basic-opt", None, {"authorization": 'Basic w6k6eA=='}),
    ("GET", "/small/basic-opt", None, {"authorization": 'Basic dXNlcjpwYXN'}),
    ("GET", "/small/basic-opt", None, {"authorization": 'Basic\tYWRtaW46czNjcmV0'}),
    ("GET", "/small/basic-opt", None, {"authorization": 'Basic'}),
    ("GET", "/small/pages", None),
    ("GET", "/small/pages", None, {"authorization": 'Bearer x'}),
    ("GET", "/small/pages", None, {"authorization": 'Basic !!!'}),
    ("GET", "/small/pages", None, {"authorization": 'Basic YWRtaW46czNjcmV0'}),
    ("GET", "/small/pages/11111111-2222-4333-8444-555555555555", None, {"authorization": 'Basic YWRtaW46czNjcmV0'}),
    ("GET", "/small/pages/00000000-0000-0000-0000-000000000000", None, {"authorization": 'Basic YWRtaW46czNjcmV0'}),
    ("GET", "/small/pages/11111111-2222-4333-8444-555555555555", None),
    ("GET", "/small/pages-txt", None),
    ("GET", "/small/pages-txt?y=<b>", None),
    ("GET", "/small/pages-missing", None),
]
STEPS += [    ("POST", "/libs/urls", {"site": ""}),  # pydantic-core: "input is empty" before the url parser
    ("POST", "/amqp/roundtrip/a;b", None),  # pamqp refuses the queue name (ValueError, 500) before sending
    ("POST", "/amqp/roundtrip/" + "q" * 250, None),  # 263 characters: "Max length exceeded for queue"
    ("POST", "/amqp/roundtrip/" + "q" * 236 + "%C3%A9", None),  # 250 characters, 256 bytes: pamqp's TypeError
    # uvicorn has status lines for 100..599 only: any other status drops the connection unanswered
    ("GET", "/libs/status/599", None),
    ("GET", "/libs/status/600", None),
    ("GET", "/libs/status/99", None),
    ("GET", "/libs/status/65736", None),  # not 200 once truncated to 16 bits
]
STEPS += [  # tests/smtp_sink.py --tls: a self-signed certificate for mail.other.invalid, dialled as 127.0.0.1
    ("POST", "/small/mail-tls?validate=false", None),  # neither the chain nor the host name is checked: sent
    ("POST", "/small/mail-tls?validate=true", None),  # refused
]


def _reset_broker() -> None:
    """Delete the test queues of fixtures/integapp/amqp.py; waits up to 60 s for the broker (CI service start)."""
    import asyncio
    import time

    import aio_pika

    async def run():
        url = os.environ.get("BROKER_DSN", "amqp://guest:guest@localhost:5672/")
        for attempt in range(60):
            try:
                conn = await aio_pika.connect(url)
                break
            except (aio_pika.exceptions.AMQPConnectionError, OSError):
                if attempt == 59:
                    raise
                time.sleep(1)
        async with conn:
            ch = await conn.channel()
            for name in ("alpha",):
                await ch.queue_delete(f"py2axum_test_{name}")

    asyncio.run(run())


def reset(db: str) -> None:
    from tests.scenarios.dynapp import fixed_file, reset_tables

    fixed_file()  # fixtures/integapp/small.py
    _reset_broker()
    reset_tables(db)


def normalize_text(text: str) -> str:
    """prometheus_client: the `_created` series and the exemplars hold instants, masked when they have their
    shape."""
    import re as _re
    text = _re.sub(r"(?m)^(\S+_created(?:\{.*\})?) \d\.\d+e\+09$", r"\1 <created>", text)
    return _re.sub(r"(?m)^(.* # \{.*\} \S+) \d{10}\.\d+$", r"\1 <timestamp>", text)


def normalize(body):
    """prometheus_client texts (fixtures/integapp/prom.py, tracing.py): see normalize_text."""
    import re as _re
    if isinstance(body, dict) and "events" in body and "metrics" in body:
        body = dict(body, metrics=normalize_text(body["metrics"]))
    if isinstance(body, dict) and "texts" in body and "om" in body:
        def walk(v):
            return normalize_text(v) if isinstance(v, str) else [walk(x) for x in v] if isinstance(v, list) else v

        def families(text):
            # a restricted registry collects a set of collectors: CPython's order follows their hashes
            return "".join(sorted(f for f in _re.split(r"(?m)^(?=# HELP )", text.removesuffix("# EOF\n")) if f))
        body = {k: walk(v) for k, v in body.items()}
        body["texts"] = [t[:4] + [families(t[4])] if "name[]" in str(t[0]) else t for t in body["texts"]]
        body["restricted"] = [families(t) for t in body["restricted"]]
    return body


# ---- logging (fixtures/integapp/logs.py): what each request prints is compared by the log conformance (--logs)
STEPS += [("GET", "/logs/levels", None), ("GET", "/logs/extra", None), ("GET", "/logs/extra?tenant=9", None),
          ("GET", "/logs/exc", None), ("GET", "/logs/exc?n=4", None),
          ("GET", "/logs/bad-format", None), ("GET", "/logs/raise", None), ("GET", "/logs/raise?n=2", None),
          ("GET", "/logs/sync", None), ("GET", "/logs/objects", None), ("GET", "/logs/config", None),
          ("GET", "/logs/root", None), ("GET", "/logs/levels", None), ("GET", "/logs/config", None)]

# ---- FastAPI parameter markers held in module variables (fixtures/integapp/markers.py)
STEPS += [(m, f"/markers/{p}", None, h) for m, p, h in (
    ("GET", "search", {}), ("GET", "search?radius=1", {}), ("GET", "search?radius=7&count=3&ratio=2", {}),
    ("GET", "search?radius=x", {}), ("POST", "search/k?count=0", {}), ("POST", "search/k?radius=12.5", {}),
    ("GET", "annotated?radius=9", {}), ("GET", "annotated?radius=9&sort-by=size", {}),
    ("GET", "annotated?radius=9&sort-by=x", {}), ("GET", "annotated", {}),
    ("GET", "shared?tag=a&tag=b&limit=5", {"x-trace": "t1"}), ("GET", "shared?limit=500", {}), ("GET", "shared", {}),
    ("GET", "paged?page=2&size=3", {}), ("GET", "paged?page=0&size=60", {}), ("GET", "paged", {}))]

# ---- routers of a library included last (fixtures/extlib/extroutes, --python-side mount): relayed to the reference
# server when no translated route fully matches. The library keeps its notes in the reference process, shared by
# both passes: cleared first.
STEPS += [("DELETE", "/ext/notes", None), ("GET", "/ext/notes", None), ("POST", "/ext/notes", {"text": "first"}),
          ("POST", "/ext/notes", {"text": "second", "level": 3}), ("POST", "/ext/notes", {"level": "x"}),
          ("GET", "/ext/notes", None), ("GET", "/ext/notes/2", None), ("GET", "/ext/notes/9", None),
          ("GET", "/ext/notes/x", None), ("GET", "/ext/echo/a/b%20c?x=1&x=2", None, {"x-probe": "yes"}),
          ("POST", "/health", None), ("GET", "/health", None), ("PUT", "/health", None),
          ("GET", "/made/whoami", None), ("GET", "/made", None), ("GET", "/ext", None), ("GET", "/nowhere/at/all", None),
          ("DELETE", "/ext/notes", None)]

# documented differences of the log conformance (tests/logcmp.py): records left out on both sides
LOG_IGNORE = []
