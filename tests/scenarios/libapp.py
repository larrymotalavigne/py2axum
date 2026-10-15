"""Scenario of fixtures/libapp: orjson, PyYAML, croniter, Unidecode, aio-pika, ecs-logging and a plain class's
__new__ (moved out of fixtures/dynapp, whose crate grew past the CI build containers' memory). No database; RabbitMQ
at BROKER_DSN. Compare the logs too (--logs): ecs-logging's output is what fixtures/libapp/ecslogs.py checks."""
import os

STEPS: list = []


def _orjson_cases() -> list:
    """fixtures/libapp/orjsonx.py: orjson.loads on valid, cut and damaged documents (yyjson's messages and
    positions), dumps of JSON values with every option, of Python values (datetimes, UUID, Enum, dataclass...),
    default=, limits, ORJSONResponse."""
    import base64
    import random
    docs = ["1", "-0", "-0.0", "0", "01", "-01", "1.", "1.e5", ".5", "+1", "-", "1e", "1e+", "1E-2", "1e400", "-1e400",
            "1e-400", "0.1", "123456789012345678901234567890", "18446744073709551616", "-9223372036854775808",
            "-9223372036854775809", "9223372036854775807", "1.7976931348623157e308", "2.2250738585072014e-308",
            "0e99999999999999999999", "1e0000000000000000000001", "true", "false", "null", "tru", "fals", "nul",
            "trux", "nan", "NaN", "Infinity", "-Infinity", "t", "f", "n", "x", "[", "{", "]", "}", "[]", "{}", '""',
            "[1,]", "[1 2]", "{,}", '{"a"}', '{"a":}', '{"a":1,}', '{"a":1 "b":2}', '{"a" 1}', "{1:2}", '{"a":1}x',
            "[1] [2]", "  ", " \t\n\r", "\n\n  [1,\n 2,\n x]", '"\\u00e9"', '"\\ud83d\\ude00"', '"\\ud83d"', '"\\ud83dx"',
            '"\\ud83d\\u0041"', '"\\ude00"', '"\\ud83d\\uzzzz"', '"\\x41"', '"\\u12"', '"\\u12g4"', '"a\tb"', '"a\nb"',
            '"abc', '"\\', '"\\u', '"\\u00', '"\\ud83d\\', '"\\ud83d\\u', '"\\ud83d\\ud', '"\\ud83d\\ude', '"é',
            '"\\/\\b\\f\\n\\r\\t\\"\\\\"', '{"a":1,"a":2,"b":3}', '{"a":{"b":[1,{"c":null}]}}', '[  1  ,  2  ]',
            "[" * 1023 + "]" * 1023, "[" * 1024 + "]" * 1024, "[" * 1100, '{"a":' * 1023 + "1" + "}" * 1023,
            '{"a":' * 1024 + "1" + "}" * 1024, "[1,[2,[3", '{"a":[1,{"b"', "[tru]", "[nul", "[1e]", "[-]", '["a\x01"]',
            "﻿[1]", "﻿1", "é", "[é]", '{"é":"ü"}', "[1.5e-7, 2.5E+16, -0.0, 1e22]", "[0.30000000000000004]"]
    rnd = random.Random(8)
    base = ['{"a": [1, 2.5, "x\\u00e9", true, null], "b": {"c": -0.0}}', '[{"k": "v"}, [], {}, 1e-7, "\\ud83d\\ude00"]']
    for d in base:
        docs += [d[:i] for i in range(1, len(d), 3)]
        for _ in range(40):
            i = rnd.randrange(len(d))
            docs.append(d[:i] + rnd.choice('{}[],:"\\ 0-.eEtfnx\x00\x1f') + d[i + 1:])
    steps = [("POST", "/orj/loads", {"doc": d}) for d in docs]
    steps += [("POST", "/orj/loads", {"doc": d, "as_bytes": True}) for d in docs[:20]]
    steps += [("POST", "/orj/loads", {"b64": base64.b64encode(b).decode()})
              for b in (b"\xff", b"[1, \xc3]", b"\xef\xbb\xbf[1]", b"", b"\xef\xbb\xbf")]
    steps += [("POST", "/orj/loads", {"kind": k}) for k in ("int", "none")]
    values = [None, True, 1, -1, 2**53, 9223372036854775807, 0.1, -0.0, 1e16, 1e-7, 1e300, 5e-324, "é \x00", [],
              {}, [1, [2, [3]]], {"b": 1, "a": {"d": [], "c": {}}, "": None}, {"z": 1, "é": 2, "B": 3, "aa": 4}]
    for opt in (0, 1, 32, 33, 1024, 1025, 64, 4, 4 | 32 | 1, 8 | 128, 4096, -1):
        steps += [("POST", "/orj/dumps", {"value": v, "opt": opt}) for v in values]
    for name in ("datetimes", "dates", "times", "uuid", "enums", "dataclass", "tuple", "floats", "ints", "strings",
                 "keys", "nonstr", "dup", "nested", "empty"):
        for opt in (0, 1, 2, 4, 8, 32, 128, 2 | 128, 4 | 32, 4 | 32 | 8 | 128, 256, 512, 2048, 1 | 1024):
            steps.append(("GET", f"/orj/catalog/{name}?opt={opt}", None))
    for name in ("decimal", "bytes", "set", "delta", "tz", "zone", "model", "type", "exc"):
        steps += [("GET", f"/orj/unsupported/{name}?default={d}", None) for d in ("none", "fn", "str", "forever")]
    for what in ("list", "dict", "tuple"):
        steps += [("GET", f"/orj/limits/{what}?n={n}", None) for n in (253, 254, 255, 300)]
    steps += [("GET", f"/orj/errors/{w}", None) for w in ("strict", "strict_ok", "badopt", "badopt_type", "key",
                                                          "key_str", "key_sorted", "passthrough_dt",
                                                          "passthrough_dt_default", "passthrough_dc", "positional")]
    steps += [("GET", "/orj/roundtrip", None), ("GET", "/orj/response", None), ("GET", "/orj/response?kind=nonstr", None),
              ("GET", "/orj/response?kind=bad", None), ("GET", "/orj/response_class", None), ("GET", "/orj/response_model", None)]
    return steps


STEPS += _orjson_cases()


def _unidecode_cases() -> list:
    """fixtures/libapp/unidec.py: every code point (Unidecode's tables), the errors= modes, UnidecodeError."""
    steps = [("GET", f"/unidecode/blocks/{start}?count=8192", None) for start in range(0x80, 0x110000, 8192)]
    texts = ["", "abc", "Κνωσός", "北亰 Москва", "naïve café", "😀 é", "\U000f0000 x \U000e0001", "ﬁ ① Ⅻ ½", "\xa0\u200b"]
    for t in texts:
        steps.append(("POST", "/unidecode/run", {"text": t}))
        for fn in ("ascii", "nonascii"):
            steps.append(("POST", "/unidecode/run", {"text": t, "fn": fn}))
        for errors in ("ignore", "strict", "replace", "preserve", "bogus"):
            steps.append(("POST", "/unidecode/run", {"text": t, "errors": errors, "replace_str": "[?]"}))
    steps += [("GET", f"/unidecode/bad?what={w}", None) for w in ("int", "int_nonascii", "replace_int")]
    return steps


STEPS += _unidecode_cases()

# a plain class's __new__ (fixtures/libapp/plainnew.py); twice: the singleton keeps its state
STEPS += [("GET", "/plainnew/run", None), ("GET", "/plainnew/run", None)]

# aio-pika exchanges, bindings, consumers, RPC, broker errors (fixtures/libapp/amqpx.py; RabbitMQ at BROKER_DSN)
AMQPX_TAGS = ("a1", "b2")
STEPS += [("POST", f"/amqpx/{route}/{tag}", None) for tag in AMQPX_TAGS
          for route in ("exchanges", "consume", "callback", "requeue", "rpc", "errors")]

# ecs-logging (fixtures/libapp/ecslogs.py): what matters is in the logs (--logs)
STEPS += [("GET", "/ecs/basic", None), ("GET", "/ecs/basic?n=5", None), ("GET", "/ecs/errors", None), ("GET", "/ecs/errors?n=0", None)]
STEPS += [("GET", f"/ecs/bad?what={w}", None) for w in ("limit", "limit_type", "exclude", "exclude_items")]
def _cronx_cases() -> list:
    """fixtures/libapp/cronx.py: croniter 6.2. Hand-written cases (DST transitions of Paris and New York both ways,
    folds, L/W/#, names, aliases, H, 6 and 7 fields, options, errors), then cases drawn from a fixed seed."""
    import random

    seq = lambda **c: ("POST", "/cronx/seq", c)  # noqa: E731
    steps = [("GET", "/cronx/misuse", None)]
    paris = [("2026-03-29T01:30:00", 0), ("2026-03-29T03:00:00", 0), ("2026-10-25T01:59:00", 0), ("2026-10-25T02:30:00", 0),
             ("2026-10-25T02:30:00", 1), ("2026-10-25T03:00:00", 0)]
    ny = [("2026-03-08T01:30:00", 0), ("2026-03-08T03:10:00", 0), ("2026-11-01T01:30:00", 0), ("2026-11-01T01:30:00", 1),
          ("2026-11-01T02:05:00", 0)]
    for tz, starts in (("Europe/Paris", paris), ("America/New_York", ny)):
        for start, fold in starts:
            for expr in ("30 2 * * *", "0 * * * *", "*/30 1-3 * * *", "0 2 * * sun", "15,45 * * * * 30", "59 1 * * *"):
                for mode in ("nextdt", "prevdt"):
                    steps.append(seq(expr=expr, start=start, tz=tz, fold=fold, mode=mode, n=4))
    for expr, extra in (
        ("*/15 9-17 * * mon-fri", {}), ("0 0 l * *", {}), ("0 12 15w * *", {}), ("0 12 w1 * *", {}), ("0 12 31w * *", {}),
        ("0 9 * * 1#2,5#5", {}), ("0 9 * * l5", {}), ("0 9 * * mon#1", {}), ("0 0 1,15 * fri", {}),
        ("0 0 1,15 * fri", {"day_or": False}), ("0 0 * * 0#3,1#4", {}), ("0 0 29 2 *", {"mode": "prev"}),
        ("0 0 31 * *", {}), ("0 6 30 3 *", {"mode": "prev"}), ("0 0 1 jan-mar,oct-dec *", {}), ("0 0 * apr-jan/3 *", {}),
        ("0 0 * * fri-mon", {}), ("0 0 * * 5-7", {}), ("0 0 * * 7", {}), ("@hourly", {}), ("@weekly", {"hash_id": "job-1"}),
        ("@midnight", {"hash_id": "job-2"}), ("@yearly", {"mode": "prev"}), ("H H * * *", {"hash_id": "build-42"}),
        ("H(0-29) H(9-17)/2 * * H", {"hash_id": "x", "hash_bytes": True}), ("H/15 * * * *", {"hash_id": "é"}),
        ("*/20 * * * * */10", {}), ("*/20 0 * * * * ", {"sab": True}), ("0 0 1 1 * 0 2028-2030", {"n": 4}),
        ("0 0 1 1 * 0 2030", {"max_years": 2}), ("0 0 * * * 0 2020", {}), ("*/7 * * * *", {"efst": True}),
        ("0 */5 * * *", {"efst": True, "mode": "prev"}), ("* * 1 * mon", {"bug": True}), ("0 0 ? * mon", {}),
        ("0 0 * ? *", {}), ("1 2 3 4 5 6 7", {}), ("0 0 1 * * 0 1969", {}), ("61 * * * *", {}), ("* * * * 8", {}),
        ("* * * * * * *", {"mode": "all_next"}), ("0 0 1 1 * 0 2030", {"mode": "all_next", "max_years": 50}),
        ("0 0 1 1 * 0 2030", {"mode": "all_prev", "max_years": 50}), ("0 0 1 1 * 0 2030", {"mode": "all_next"}),
        ("*/10 * * * *", {"mode": "iter"}), ("*/10 * * * *", {"mode": "iter", "is_prev": True}),
        ("*/10 * * * *", {"mode": "peek"}), ("*/10 * * * *", {"mode": "alt", "ret": "datetime"}),
        ("*/10 * * * *", {"ret": "int"}), ("*/10 * * * *", {"ret": "none"}), ("*/10 * * * *", {"ret": "str", "mode": "nextdt"}),
        ("*/10 * * * *", {"tz": "+330"}), ("*/10 * * * *", {"tz": "-600", "mode": "prev"}), ("0 0 * * *", {"tz": "utc"}),
        ("0 0 * * *", {"tz": "Australia/Lord_Howe", "start": "2026-04-05T01:00:00"}),
        ("*/30 * * * *", {"tz": "Australia/Lord_Howe", "start": "2026-10-04T01:00:00"}),
        ("0 2 * * *", {"tz": "Pacific/Chatham", "start": "2026-09-26T12:00:00"}), ("0 0 * * *", {"start": 1767225599.9999995}),
        ("0 0 * * *", {"start": -86401.25, "mode": "prev"}), ("* * * * *", {"start": "9999-12-31T23:58:00"}),
        ("* * * * *", {"start": "0001-01-01T00:01:00", "mode": "prev"}),
        ("* * * * *", {"start": 7e16}), ("not a cron", {}), ("* * * *", {}), ("*/0 * * * *", {}), ("1-5/0 * * * *", {}),
        ("* * * * * * * *", {}), ("? * * * *", {}), ("* * ?? * *", {}), ("-1 * * * *", {}), ("a-b * * * *", {}),
        ("* * * foo *", {}), ("* * * * 1#6", {}), ("* * * * 1,2#3", {}), ("* * 15w,1 * *", {}), ("* * 32w * *", {}),
        ("H * * * *", {}), ("H(5-1) * * * *", {"hash_id": "a"}), ("H(1-5)/0 * * * *", {"hash_id": "a"}),
        ("60-61 * * * *", {}), ("5-70 * * * *", {}), ("* * 0 * *", {}), ("* * 0 * * *", {}), ("* * * 0 * *", {}),
        ("٣ * * * *", {}), ("* * * * ½#9", {}), ("@HOURLY", {}), ("0 0　* * *", {}),
    ):
        c = {"expr": expr, "start": "2026-02-27T13:37:00", "n": 6, "mode": "nextdt", **extra}
        steps.append(seq(**c))
    steps += [("POST", "/cronx/restart", {"expr": e, "start": s, "tz": tz})
              for e, s, tz in (("*/30 * * * *", "2026-01-01T10:00:00", "America/New_York"), ("0 2 * * *", 1700000000, None),
                               ("0 0 * * * 0", "2026-06-01T00:00:00", "utc"))]

    rnd = random.Random(20261014)
    ranges = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 6), (0, 59), (1970, 2099)]
    zones = [None, "utc", "Europe/Paris", "America/New_York", "Australia/Lord_Howe", "Asia/Kolkata", "+90", "-300",
             "Pacific/Chatham", "America/Santiago", "Europe/Dublin"]

    def atom(f):
        lo, hi = ranges[f]
        r = rnd.random()
        if r < 0.1 and f == 3:
            return rnd.choice(["jan", "mar-may", "oct", "dec-feb", "jun/2"])
        if r < 0.1 and f == 4:
            return rnd.choice(["mon", "tue-thu", "sat-sun", "fri", f"{rnd.randint(0, 6)}#{rnd.randint(1, 5)}"])
        if r < 0.15 and f == 2:
            return rnd.choice(["l", f"{rnd.randint(1, 28)}w"])
        if r < 0.45:
            return "*"
        if r < 0.6:
            return f"*/{rnd.randint(1, 12)}"
        if r < 0.75:
            a = rnd.randint(lo, hi)
            return f"{a}-{min(hi, a + rnd.randint(0, 9))}" + (f"/{rnd.randint(1, 4)}" if rnd.random() < 0.4 else "")
        return ",".join(str(rnd.randint(lo, hi)) for _ in range(rnd.randint(1, 3)))

    def expr():
        n = rnd.choice([5, 5, 5, 6, 7])
        fs = [atom(i) for i in range(min(n, 6))]
        if n == 7:
            fs.append(rnd.choice(["*", f"{rnd.randint(2026, 2030)}-{rnd.randint(2030, 2040)}", str(rnd.randint(2026, 2032))]))
        return " ".join(fs)

    def when():
        if rnd.random() < 0.2:
            return rnd.randint(946684800, 2524608000)
        base = rnd.choice(["2026-03-29T0", "2026-10-25T0", "2026-03-08T0", "2026-11-01T0", "2027-0" + str(rnd.randint(1, 9)) + "-15T1"])
        return f"{base}{rnd.randint(0, 3)}:{rnd.randint(0, 59):02d}:00"

    for _ in range(140):
        c = {"expr": expr(), "start": when(), "tz": rnd.choice(zones), "n": rnd.randint(2, 8),
             "mode": rnd.choice(["next", "prev", "nextdt", "prevdt", "alt", "all_next", "all_prev"])}
        if rnd.random() < 0.15:
            c["hash_id"] = "job"
            c["expr"] = c["expr"].replace("*", "H", 1)
        if rnd.random() < 0.1:
            c["day_or"] = False
        if rnd.random() < 0.1:
            c["sab"] = True
        steps.append(seq(**c))
    for _ in range(30):
        tz = rnd.choice(zones)
        if rnd.random() < 0.3:
            a = rnd.randint(1774000000, 1800000000)
            start, stop = a, a + rnd.randint(-90000, 90000)
        else:
            start = when()
            start = start if isinstance(start, str) else "2026-03-29T00:30:00"
            stop = start[:11] + f"{rnd.randint(0, 23):02d}:{rnd.randint(0, 59):02d}:00"
        e = rnd.choice(["*/15 * * * *", "0 * * * *", "30 1,2,3 * * *", "0 2 * * *", "*/20 * * * * 0,30", "0 */3 * * *", expr()])
        steps.append(("POST", "/cronx/range", {"expr": e, "start": start, "stop": stop, "tz": tz, "exclude_ends": rnd.random() < 0.3,
                                               "lazy": rnd.random() < 0.4, "n": rnd.randint(1, 20),
                                               **({"ret": rnd.choice(["float", "datetime"])} if rnd.random() < 0.3 else {})}))
    steps += [("POST", "/cronx/range", {"expr": "* * * * *", "start": 0, "stop": 1.5}),
              ("POST", "/cronx/range", {"expr": "0 * * * *", "start": "2026-01-01T00:00:00", "stop": "2026-01-01T03:00:00",
                                        "tz": "Europe/Paris", "tz_stop": "utc"}),
              ("POST", "/cronx/range", {"expr": "0 * * * *", "start": "2026-01-01T05:00:00", "stop": "2026-01-01T00:00:00"}),
              ("POST", "/cronx/range", {"expr": "0 0 1 1 * 0 2100", "start": "2026-01-01T05:00:00", "stop": "2027-01-01T00:00:00"})]
    for _ in range(25):
        tz = rnd.choice(zones)
        dates = [when() for _ in range(rnd.randint(1, 3))]
        dates = [d if isinstance(d, str) else "2026-05-05T05:05:00" for d in dates]
        m = {"expr": rnd.choice([expr(), "0 2 * * *", "*/5 * * * *", "0 * * * * 0", "30 1 * * *"]), "dates": dates, "tz": tz}
        if rnd.random() < 0.3:
            m["to"] = dates[0][:11] + "23:59:59"
        if rnd.random() < 0.3:
            m["precision"] = rnd.choice([1, 30, 3600, 0.5])
        steps.append(("POST", "/cronx/match", m))
    steps += [("POST", "/cronx/match", {"expr": "0 2 * * *", "dates": ["2026-03-29T02:00:00", "2026-03-29T03:00:00"], "tz": "Europe/Paris"}),
              ("POST", "/cronx/match", {"expr": "30 1 * * *", "dates": ["2026-11-01T01:30:00"], "tz": "America/New_York",
                                        "to": "2026-11-01T01:59:59"})]
    for _ in range(25):
        v = {"exprs": [expr() for _ in range(rnd.randint(1, 4))]}
        if rnd.random() < 0.3:
            v["hash_id"] = "job"
        if rnd.random() < 0.3:
            v["strict"] = True
        steps.append(("POST", "/cronx/valid", v))
    steps += [("POST", "/cronx/valid", {"exprs": ["0 0 31 2 *", "0 0 30 2 *", "0 0 29 2 *", "0 0 29 2 * 0 2025", "0 0 29 2 * 0 2028",
                                                  "0 0 31 4,6 *", "0 0 l 2 *", "0 0 15w 2 *"], "strict": True}),
              ("POST", "/cronx/valid", {"exprs": ["0 0 29 2 *"], "strict": True, "strict_year": 2025}),
              ("POST", "/cronx/valid", {"exprs": ["0 0 29 2 *"], "strict": True, "strict_year": [2025, 2028]}),
              ("POST", "/cronx/valid", {"exprs": ["H * * * *", "@daily", "* * * * 0#1,1", "* * * * 0#1,1#1,2#1,3#1,4#1,5#1,6#1",
                                                  "0 0 0 * * *", "1 * * * * * 1970-2099", "* * 1-31 * *", "* * 1-31 * 0-6",
                                                  "0-59 * * * 1-7", "* * * *", "x y z w v", ""]}),
              ("POST", "/cronx/valid", {"exprs": ["* * * * * 5", "5 * * * * *"], "sab": True})]
    return steps


STEPS += _cronx_cases()
# ---- PyYAML (fixtures/libapp/yamlx.py): safe_load of every construct and error, safe_dump/dump of every option
YAML_DOCS = [
    # values and implicit types
    "a: 1\nb: [x, y]\n", "", "   ", "# only a comment", "---", "--- \n...", "~", "null", "Null", "NULL", "nUll",
    "yes", "No", "ON", "off", "TRUE", "y", "n", "0", "-0", "+12", "1_000", "0b1010", "-0b1_1", "0o17", "017", "0x1F",
    "-0xff_ff", "190:20:30", "-1:30", "9223372036854775807", "-9223372036854775808", "0b_",
    "0x_", "08", "1:60", "1:7", "1.0", "-1.5e+3", "1e3", "1.5e3", ".5", ".inf", "-.Inf", "+.INF", ".nan", ".NaN",
    "1_000.5", "190:20:30.15", "1.", "0.0", "-0.0", "3.14159265358979323846", "1.0e+400", "12e-400",
    "2001-12-14", "2001-12-14t21:59:43.10-05:00", "2001-12-14 21:59:43.10 -5", "2001-12-15T02:59:43.1Z",
    "2001-12-14 21:59:43.1234567", "2001-1-1 1:02:03", "2001-02-30", "2001-13-01", "0000-01-01",
    "2001-12-14 25:00:00", "2001-12-14T21:59:43+00:00", "2001-12-14 21:59:43 -00:30", "2001-12-14 21:59:43.",
    # explicit tags
    "!!int '12'", "!!int abc", "!!int ''", "!!int -", "!!int ' 7 '", "!!int 0b12", "!!int 0o17", "!!int 0x", "!!int +-5",
    "!!int 1:x", "!!int 1.5", "!!int " + "1" * 4301, "!!int 0b0b1", "!!float abc", "!!float ' 1.5 '", "!!float ''",
    "!!float -ABC", "!!float inf", "!!float -nan", "!!float 1:", "!!float 1e5:30", "!!bool maybe", "!!bool On",
    "!!str 12", "!!null x", "!!binary aGVsbG8=", "!!binary |\n  aGVsbG8g\n  d29ybGQ=\n", "!!binary é", "!!binary aéè",
    "!!binary aGVsbG8", "!!binary a", "!!binary ''", "!!timestamp 2001-1-1", "!!timestamp nope", "!!timestamp [1]",
    "!!timestamp {=: 2001-01-01}", "!!timestamp 2001-12-14 21:59:43 +24:00", "!!timestamp 2001-12-14 1:2:3 -99:99",
    "!!timestamp 2001-12-14 21:59:43 +05:99", "!!set {a, b, a}", "!!set [a]", "!!omap [a: 1, b: 2]", "!!omap [a, b]",
    "!!omap [{a: 1, b: 2}]", "!!omap {a: 1}", "!!pairs [a: 1, a: 2]", "!!pairs a", "!!seq {a: 1}", "!!map [1]",
    "!!str [1]", "!!str {a: 1}", "!foo bar", "!<tag:yaml.org,2002:str> 5", "!<!> 5",
    "%TAG !e! tag:example.com,2000:\n---\n!e!foo bar", "!e!foo bar", "! 12", "! a", "!!int {=: 5}", "!!str {=: {=: x}}",
    "!!python/tuple [1]", "!%41 x", "!%ff x", "!%4 x", "!<%E2%82%AC> x", "!!int\n- 1", "&a !!str 1", "!!str &a 1",
    # anchors and aliases
    "a: &x 1\nb: *x", "&a [*a]", "a: *nope", "a: &x 1\nb: &x 2", "[&a [1], *a]", "a: &k {b: 1}\nc: *k",
    "&a {x: *a}", "&a !!str {=: *a}", "[&a, *a]", "&anchor_1-x [1]", "*a", "&a",
    # merge keys and value keys
    "base: &b {x: 1, y: 2}\nderived:\n  <<: *b\n  y: 3\n", "a: &a {x: 1}\nb: &b {x: 2, y: 2}\nc:\n  <<: [*a, *b]\n  z: 3",
    "<<: 1", "a:\n  <<: [1]", "<<", "- <<", "=: 3", "[=]", "{a: 1, =: 2}", "a: &m {<<: {k: 1}}\nb: {<<: *m, j: 2}",
    "&a {<<: *a}", "a: &a {x: 1}\nb: {<<: [*a, *a]}",
    # collections
    "{a: 1, b: [1, 2, {c: d}]}", "[a, b, ]", "[a, b,, c]", "{a, b: c}", "[a: 1, b]", "? complex\n: value",
    "? [a, b]\n: c", "? {a: 1}\n: c", "{[1]: 2}", "- - - x", "- a\n-\n- c", "a:\n- 1\n- 2", "a: \n  - 1\n  -  2",
    "[? a : b]", "{? : x}", "[a, ? b]", "{a: [b], c: {d: e}}", "!!set\n? a\n? b\n", "{1: a, 1.0: b, true: c}",
    "1: a\n1.0: b\ntrue: c\n", "null: x\n~: y", "2001-01-01: d", ".nan: 1\n.nan: 2", "a: 1\na: 2", "[1, [2, [3, [4]]]]",
    # block scalars
    "a: |\n  line1\n  line2\n", "a: |-\n  x\n\n", "a: |+\n  x\n\n", "a: >\n  folded\n  text\n\n  para\n",
    "a: >-\n  x\n   indented\n  y\n", "a: |2\n    two\n", "a: |0\n x", "a: |x", "a: | # c\n  x", "a: >+\n\n  x\n\n\n",
    "- |\n  a\n- >\n  b\n", "a: |\n\n\n  x\n", "a: |-2\n   x\n", "a: >\n x\n\n\n y\n", "a: |\n x\n  y\n z\n",
    "- >\n\n folded\n line\n\n  more\n", "a: | x", "|\n  top\n", ">\n", "a: |+\n", "a: >2-\n   b\n",
    # quoted
    '"\\x41\\u00e9\\U0001F600"', '"\\q"', '"\\xZ1"', "'it''s'", '"a\n  b\n\n  c"', '"unterminated', "'a\\b'",
    '"\\\n  x"', '"tab\\tx"', '"\\0\\a\\b\\e\\N\\_\\L\\P\\/\\ \\v\\f\\r\\""', '"\\U00110000"', "'a\n\n  b'",
    "'multi\n  line  \n  text'", '"x\n---\ny"', "'x\n... y'", '"a\\\tb"', "''", '""', "'  '", "\"a\" b", "'a' : b",
    # plain scalars
    "a b c", "a: b: c", "a:b", "http://x.y/z?q=1#frag", "a #comment", "a#b", "- -1\n- -x", "key:  value  ", "  leading",
    "x\n  continued\n  lines", "a: b\n c", "@foo", "`x", "%x", "a\tb", "a:\tb", "\ta: 1", "a: 1\n\tb: 2", "a\u2028b", "-",
    "- ", "?", ":", "?x", ":x", "-x", "[a]b", "{a: b}c", "a\n\n\nb", "a: b\n\n# c\n\nd: e", "[a b, c\n d]", "{a: b c\n d}",
    # documents and directives
    "a\n---\nb", "--- a\n--- b", "--- >\n  x\n...\n", "...\n", "%YAML 1.1\n--- x", "%YAML 1.2\n--- x", "%YAML 2.0\n--- x",
    "%YAML 1.1\n%YAML 1.1\n--- x", "%TAG ! !x\n%TAG ! !y\n--- a", "%FOO bar\n--- x", "%YAML 1.1 x\n--- y",
    "%YAML 1.\n--- y", "%YAML a\n", "%TAG !x tag\n--- a", "%\n", "x\n%YAML 1.1\n--- y", "%YAML 1.1\nx", "--- !!str\n---",
    "%TAG !e! tag:x,\n--- !e!y\n...\n--- !e!y", "%YAML 01.1\n--- a", "%TAG !e! x # c\n--- a", "%TAG !e!x y\n--- a",
    "%YAML 1.1 # c\n---", "--- |\n  a\n--- b\n", "---\n---\n", "...\n...\n",
    # scanner and parser errors
    "a: b\nc", "[1, 2", "{a: 1", "]", "}", "a: [b}", "key: - a", "- a\nb: c", "a:\n  b\n c: d", "? a\n? b\n: c", "&", "*",
    "&a!b c", "!<x y", "!a!", "!!", "'\n---\n'", '"x\n...\n"', "a: 'b\n...'", "a:\n- b\n  c: d", "a: b: c: d",
    "[a, {b: c]", "{a: [b}", "- a\n - b", "a:\n  - b\n  c", "a: !!str", "- !foo", "? - a", "x: &a", "a: b\n  c: d",
    "key: 'unclosed\n", "\"\\", "a: [1, 2]]", ",", "a, b", "a: ,", "[,]", "{,}", "- [\n- ]", "a: |\nb: c",
    # unicode and special characters
    "é: ü", "emoji: 😀", "a: \u2028b", "\ufeffa: 1", "a: \ufeffb", "x: \x85y", "\x00", "a\x07b", "a: \u00a0b",
    "a\r\nb: c\r\n", "a: 1\rb: 2", "a\u0085b: c", "\u2029x", "'\u00e9\u2028'", "a: \ue000", "\U0010ffff", "\ufffe",
    "a: 1\n" + "b" * 1100 + ": 2", "[" + "x" * 1100 + ": 1]", "k: " + "word " * 100,
    "key: value " + "x" * 100 + " : bad", "x" * 100 + ": [" + "y" * 100, "- " + "z" * 80 + "\n- \x07",
]

YAML_BYTES = [
    b"a: 1\n", b"\xef\xbb\xbfa: 1\n", "a: \u00e9".encode("utf-16-le"), b"\xff\xfe" + "a: \u00e9\U0001F600".encode("utf-16-le"),
    b"\xfe\xff" + "b: \u00fc".encode("utf-16-be"), b"a: \xff", b"a: \xe2\x82", b"a: \xe2\x28\xa1", b"\xff\xfea", b"\xff\xfe\x00\xd8a\x00",
    b"\xff\xfe\x00\xdc", b"\xff\xfe\x00\xd8", b"\xfe\xff\xd8\x00\x00a", b"a: \xc0\x80", b"a: \xed\xa0\x80", b"", b"\x00a",
    b"a: \xf4\x90\x80\x80", b"\xef\xbb\xbf", b"a: b\n\x07", "café: crème".encode("latin-1"), b"[1, \xf0\x9f\x98",
]


def _yaml_value_cases() -> list:
    """Values for the dump routes, in yamlx.value()'s JSON form."""
    strings = ["yes", "123", "1.5", "~", "", " lead", "trail ", "a: b", "#c", "- x", "multi\nline", "multi\nline\n",
               "tab\tx", "é", "😀", "\u2028", "\x85", "\x00", "\ufeff", "'quote'", '"dq"', "back\\slash", "x" * 90,
               ("word " * 30).strip(), "a  b", "\n", "\n\n", " ", "---", "...x", "? x", "@x", "%x", "a\n b", "a \nb",
               "null", "Null", "2001-01-01", "1:30", "0x1F", "=", "<<", "!x", "&x", "*x", "a,b", "[x]", "{x}", "a#b",
               "a #b", ":x", "x:", "\u00a0", "\ue000", "\U0010ffff", "\x07", "\x7f", "a\r\nb", "\r", "  \n  x"]
    floats = [{"$float": f} for f in ("1e17", "1e-05", "nan", "inf", "-inf", "-0.0", "0.1", "0.3333333333333333", "1e300", "123456789.0", "1e16")]
    return [
        {"b": 1, "a": [1, 2.5, None, True, False, "x"]}, strings, floats, {s: s for s in strings[:20]},
        {"$map": [[1, "int"], [{"$float": "2.5"}, "f"], [None, "n"]]}, {"$map": [[1, "a"], ["b", 2]]},
        {"$map": [[{"$tuple": [1, 2]}, "t"], [{"$tuple": [0, 5]}, "u"]]}, {"$map": [[{"$date": "2001-01-02"}, 1], [{"$date": "2000-01-01"}, 2]]},
        [{"$date": "2001-12-14"}, {"$dt": "2001-12-14T21:59:43.100000"}, {"$dt": "2001-12-14T21:59:43+05:30"},
         {"$dt": "2001-12-14T21:59:43+00:00"}, {"$dt": "2001-12-14T00:00:00-08:00"}],
        {"$bytes": "aGVsbG8gd29ybGQ="}, {"$bytes": "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+P0BBQkNE"},
        {"$bytes": ""}, {"$set": ["b", "a", "c"]}, {"$set": []}, {"$tuple": [1, "a", None]}, {"$tuple": []}, [], {},
        [[], {}, [[]], [{}]], {"nested": {"deep": {"deeper": [1, {"x": [2, 3]}]}}}, {"k": [{"a": 1, "b": [1, 2]}, "s"]},
        "plain", 42, -7, None, True, {"$float": "3.5"}, "x" * 200, ("abc " * 40).strip(), "line one\nline two\n\nthree",
        {"long key " * 20: "v"}, {"text": "folded " * 30}, [("a b " * 50).strip()],
        {"$complex": "1+2j"}, {"$complex": "-1.5j"}, {"$complex": "3"}, {"$complex": "1-2j"},
    ]


def _yaml_cases() -> list:
    import base64

    steps = [("POST", "/yamlx/load", {"text": t}) for t in YAML_DOCS]
    steps += [("POST", "/yamlx/load/safeloader", {"text": t}) for t in ("a: 1", "[", "!!python/name:os.system ''", "")]
    steps += [("POST", "/yamlx/load/bytes", {"b64": base64.b64encode(b).decode()}) for b in YAML_BYTES]
    # through file objects: errors past the first 4096 characters come later (the reader reads by chunks)
    big = ("k: v\n" * 900 + "x: [\n" + "y: 1\n" * 200 + "z: \x07\n").encode()
    late = ("a: 1\n" * 1000 + "\x07").encode()
    for b in (b"a: 1\nb: [x, y]\n", b"a: [1", b"\xef\xbb\xbfa: \xc3\xa9\n", b"a: \xff", b"a\r\nb: c\rd: e", big, late,
              ("x: " + "\u00e9" * 3000 + "\n").encode(), "a: \u00e9".encode("utf-16"), b"\xff\xfe\x00", b""):
        for how in ("text", "binary", "stringio", "bytesio"):
            if how in ("text", "stringio") and (b"\xff" in b or b"\xfe" in b):
                continue
            steps.append(("POST", "/yamlx/load/stream", {"b64": base64.b64encode(b).decode(), "how": how}))
    values = _yaml_value_cases()
    steps += [("POST", "/yamlx/dump", {"value": v}) for v in values]
    steps += [("POST", "/yamlx/dump", {"value": v, "full": True}) for v in values]
    sample = {"name": "x", "items": [1, "two", {"three": [3.0, None]}], "text": "a long line " * 12, "multi": "a\nb\n",
              "uni": "é😀", "empty": [], "map": {}, "q": "yes"}
    for kw in ({"default_style": '"'}, {"default_style": "'"}, {"default_style": "|"}, {"default_style": ">"},
               {"default_style": ""}, {"default_style": "|>"}, {"default_flow_style": None}, {"default_flow_style": True},
               {"canonical": True}, {"indent": 1}, {"indent": 4}, {"indent": 9}, {"indent": 10}, {"width": 10},
               {"width": 3}, {"width": 1000}, {"width": {"$float": "inf"}}, {"allow_unicode": True}, {"line_break": "\r\n"},
               {"line_break": "\r"}, {"line_break": "x"}, {"encoding": "utf-8"}, {"encoding": "utf-16"},
               {"encoding": "utf-16-le"}, {"encoding": "UTF-16"}, {"encoding": "ascii"}, {"encoding": "ascii", "allow_unicode": True},
               {"encoding": "latin-1", "allow_unicode": True}, {"explicit_start": True}, {"explicit_end": True},
               {"explicit_start": True, "explicit_end": True}, {"version": [1, 1]}, {"version": [1, 2], "explicit_end": True},
               {"version": [2, 0]}, {"tags": {"!e!": "tag:example.com,2000:"}}, {"tags": {"!bad": "x"}},
               {"tags": {"!e!": ""}}, {"tags": {"!e!": "x y"}}, {"tags": {"!a-b!": "!local"}}, {"sort_keys": False},
               {"canonical": True, "allow_unicode": True, "default_flow_style": None}, {"indent": 4, "width": 20, "default_flow_style": None}):
        steps.append(("POST", "/yamlx/dump", {"value": sample, "kw": kw}))
        steps.append(("POST", "/yamlx/dump", {"value": ["x " * 60, {"k": "v", "e": "é\u2028"}, "|literal\n"], "kw": kw, "full": True}))
    steps += [("POST", "/yamlx/dump", {"value": {"$map": [[1, "a"], ["b", 2]]}, "kw": {"sort_keys": False}}),
              ("POST", "/yamlx/dump", {"value": {"$set": ["b", "a"]}, "kw": {"sort_keys": True}}),
              ("POST", "/yamlx/dump", {"value": "x", "kw": {"version": [1, 1], "explicit_start": False}}),
              ("POST", "/yamlx/dump", {"value": "multi\nline\n", "kw": {"default_style": "|", "explicit_end": True}}),
              ("POST", "/yamlx/dump", {"value": ["a\n", "b\n\n"], "kw": {"default_style": ">"}})]
    steps += [("POST", "/yamlx/dump/dumper", {"value": v, "safe": s}) for v in ({"b": [1, {"$tuple": [2, 3]}], "a": None},
                                                                                   [{"$complex": "1+1j"}]) for s in (True, False)]
    steps += [("POST", "/yamlx/dump/all", {"docs": [{"a": 1}, [1, 2], "x", None]}),
              ("POST", "/yamlx/dump/all", {"docs": [{"a": 1}, "x"], "start": True}),
              ("POST", "/yamlx/dump/all", {"docs": [{"$tuple": [1]}, "y"], "full": True}), ("POST", "/yamlx/dump/all", {"docs": []})]
    for how in ("stringio", "bytesio", "text", "binary"):
        for kw in ({}, {"encoding": "utf-8"}, {"encoding": "utf-16-le", "allow_unicode": True}):
            steps.append(("POST", "/yamlx/dump/stream", {"value": {"a": [1, "é"], "b": "x"}, "kw": kw, "how": how}))
        steps.append(("POST", "/yamlx/dump/stream", {"value": {"a": 1}, "kw": {"version": [3, 0]}, "how": how}))
    steps += [("GET", "/yamlx/shared", None), ("POST", "/yamlx/unrepresentable", {"kinds": ["time", "delta", "frozen", "nested"]})]
    steps += [("GET", f"/yamlx/deep/{k}/{n}", None) for k in ("load", "loadmap", "dump") for n in (5, 100, 5000)]
    steps += [("POST", "/yamlx/roundtrip", {"value": v, "unicode": u, "width": w}) for v in values[:12] for u, w in ((False, 80), (True, 20))]
    return steps


STEPS += _yaml_cases()


def reset(db: str) -> None:
    """Delete what a failed run of fixtures/libapp/amqpx.py may have left; waits up to 60 s for the broker (CI service
    start)."""
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
            for tag in AMQPX_TAGS:
                for q in ("pq", "pc", "pcb", "pr", "prpc", "pe"):
                    await ch.queue_delete(f"{q}_{tag}")
                for x in ("px_{}_direct", "px_{}_fanout", "px_{}_topic", "pe_x_{}"):
                    await ch.exchange_delete(x.format(tag))

    asyncio.run(run())
