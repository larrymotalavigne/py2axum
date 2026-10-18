"""Scenario of fixtures/libapp: orjson, PyYAML, croniter, Unidecode, aio-pika, ecs-logging and a plain class's
__new__ (then pytz, dateutil's parser and tz, cachetools: 0.9) (moved out of fixtures/dynapp, whose crate grew past the CI build containers' memory). No database; RabbitMQ
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


def _itsx_cases() -> list:
    """fixtures/libapp/itsx.py: itsdangerous.URLSafeSerializer (deterministic tokens: compared byte for byte)."""
    values = [None, 1, -2.5, "é \x00", [], {}, {"id": 7, "u": "unsubscribe"}, ["a" * 200], {"k": list(range(60))},
              {"z": 1, "a": [True, False, None]}]
    steps = [("POST", "/itsx/dumps", {"value": v}) for v in values]
    steps += [("POST", "/itsx/dumps", {"value": {"id": 3}, **o}) for o in (
        {"salt": "desabonnement-marketing"}, {"salt": "none"}, {"keys": "rotated"}, {"call_salt": "other"},
        {"salt": "s", "call_salt": "t"}, {"salt": "none", "call_salt": "t"})]
    # tokens made by the reference itsdangerous 2.2.0 (secret "lib-secret")
    good = "eyJpZCI6N30.VIS4fzdZEmy7OPxyd6u9r748Y30"
    salted = "eyJpZCI6N30.PMoE2aSfhVv6iWQTraiclaGTA1c"
    for tok in (good, salted, "WzEsMl0.x", "nodot", "", ".", "a.b.c", "eyJpZCI6N30.!!", good + "x", good.upper()):
        steps += [("GET", f"/itsx/loads?token={tok}&{q}", None) for q in (
            "", "salt=desabonnement-marketing", "keys=rotated", "keys=old", "salt=none", "how=ignored", "how=bytes",
            "call_salt=desabonnement-marketing&how=positional", "call_salt=desabonnement-marketing&how=keyword",
            "how=positional")]
    steps += [("GET", f"/itsx/garbage/{n}", None) for n in ("b64", "zlib", "json", "utf8", "empty", "cut", "zlib_short")]
    steps += [("GET", f"/itsx/tamper?what={w}", None) for w in ("sig", "nodot", "payload", "sig_b64", "empty", "dot")]
    steps += [("GET", f"/itsx/mixed?direction={d}", None) for d in ("timed_to_plain", "plain_to_timed")]
    steps += [("GET", f"/itsx/errors?what={w}", None) for w in ("set", "int_token", "too_many", "key_int")]
    return steps


STEPS += _itsx_cases()


def _mimex_cases() -> list:
    """fixtures/libapp/mimex.py: mimetypes on the built-in tables of the project's Python and the host's files."""
    names = ["a.txt", "A.TXT", "photo.JPG", "photo.jpg", "x.tar.gz", "x.tgz", "x.TGZ", "x.tar.GZ", "x.svgz", "x.tar.bz2",
             "x.tar.xz", "x.br", "x.Z", "x.gz", "x.pdf.gz", "noext", ".bashrc", "..gz", "dir.d/file", "dir.d/", "a.b.c.json",
             "/abs/path/report.PDF", "C:\\docs\\a.pdf", "c:a.png", "a.png?x=1", "a.png#f", "song.mp3", "font.woff2",
             "a.webp", "a.avif", "a.yaml", "a.docx", "a.7z", "a.rtf", "a.mid", "a.pict", "a.xul", "a.md", "a.heic", "é.PNG",
             "a.İ", "a.\u212a", "", ".", "a.", "http://h/a.png", "HTTP://h/a.PDF?q=1#x", "https://h/dir/", "https://h",
             "ftp://h/a.tar.gz;type=i", "http://h/a.pdf;v=1", "file:///tmp/a.css", "s3://bucket/key.json",
             "x:a.png", "xy:a.png", "1x:a.png", " http://h/a.png", "\thttp://h/a.\npng", "data:,hello",
             "data:image/png;base64,iVBOR", "data:text/plain;charset=utf-8,x", "data:image/svg+xml,<svg>", "data:foo,x",
             "data:a=b/c,x", "data:image/png", "DATA:image/gif,x", "data:;base64,x", "data://h/x,y",
             "http://[::1]/a.png", "http://[::1/a.png", "http://::1]/a.png", "http://[v1.x]/a.png", "http://[V1.x]/a.png",
             "http://[v1]/a.png", "http://[vg.x]/a.png", "http://[1.2.3.4]/a.png", "http://[fe80::1%eth0]/a.png",
             "http://[fe80::1%]/a.png", "http://[nope]/a.png", "http://u@[::1]:80/a.png", "http://x[::1]/a.png",
             "http://[::1]x/a.png", "http://ℍ/a.png", "http://℀/a.png", "http://h／x/a.png", "http://﹖/a.png"]
    steps = [("POST", "/mimex/guess", {"names": names}), ("POST", "/mimex/guess", {"names": names, "strict": False}),
             ("POST", "/mimex/guess", {"names": names[:40], "as": "path"}),
             ("POST", "/mimex/guess", {"names": names[:40], "as": "path", "strict": 0}),
             ("POST", "/mimex/guess", {"names": names[:60], "as": "bytes"}),
             ("POST", "/mimex/file", {"names": names[:45]}), ("POST", "/mimex/file", {"names": names[:45], "strict": False})]
    types = ["text/plain", "TEXT/PLAIN", "image/jpeg", "image/jpg", "image/png", "application/pdf", "audio/midi",
             "image/pict", "application/rtf", "text/xul", "application/octet-stream", "application/json", "text/html",
             "audio/mpeg", "video/mp4", "application/zip", "application/x-tar", "image/svg+xml", "text/csv",
             "application/vnd.openxmlformats-officedocument.wordprocessingml.document", "font/woff2", "image/webp",
             "application/yaml", "text/markdown", "nope/nope", "", "image/heic", "text/javascript", "application/xml"]
    steps += [("POST", "/mimex/ext", {"types": types, **o}) for o in (
        {}, {"strict": False}, {"all": True}, {"all": True, "strict": False}, {"strict": 1})]
    steps += [("GET", f"/mimex/upload?{q}", None) for q in (
        "filename=a.pdf", "filename=scan.JPEG", "filename=x", "content_type=image/png", "filename=a.pdf&content_type=text/csv",
        "", "filename=a.unknownext", "content_type=application/x-unknown")]
    steps += [("GET", f"/mimex/bad?what={w}", None) for w in ("int", "none_ext", "int_all")]
    steps += [("GET", f"/mimex/file_response?name={n}", None) for n in ("a.pdf", "a.unknown", "x.tar.gz", "http://[x/a.png")]
    return steps


STEPS += _mimex_cases()


def _email_cases() -> list:
    """fixtures/libapp/emailx.py: EmailMessage under policy.default (and SMTP, SMTPUTF8, HTTP). A table of header values
    (unstructured and address headers: blanks, non-ASCII, encoded words, specials, long words) and bodies (each
    Content-Transfer-Encoding), then token soups from a fixed seed that reach the folder's corner cases (comments,
    quoted strings, groups, obsolete local parts, domain literals, junk after an address). The folder changed in
    security releases: every one was checked against this table (3.12.12 to 3.15.0)."""
    import random

    subjects = [
        "", "hello", " hello", "hello ", "  two  spaces  ", "tab\tinside", "\tleading tab", "café", "é",
        "Réunion de l'équipe demain à 10h", "日本語の件名", "emoji 🎉 party", "🎉", "a" * 70, "a" * 78, "a" * 100, "word " * 30,
        "é" * 40, "mot é " * 20, "x" * 50 + " é" + " y" * 30, "=?utf-8?q?caf=C3=A9?=", "=?utf-8?b?w6k=?= plain",
        "re: =?iso-8859-1?q?a?= =?iso-8859-1?q?b?=", "specials ()<>@,;:\\\".[]", "(comment) text", '"quoted" text', "a\\b",
        "end with é", "é start", "mixed ascii and ünïcödé words that go on and on beyond the limit of seventy eight characters",
        "a" * 92 + "é", " ".join(["é"] * 40), "Fwd: [ticket #1234] Ça marche — pas tout à fait…", "(no subject)", "x" * 200,
        "ascii " * 10 + "é" + " ascii" * 10, "ü ü", "ü  ü", "a ü b", "ü\tü", " ü", "ü ", " nbsp", "zero​width",
        "ascii-only-but-very-long-" * 5, "a=b?c_d", "=?", "?=",
    ]
    addresses = [
        "", "a@b.com", "John Doe <john@example.com>", '"Doe, John" <john@example.com>', "Jöhn Doé <john@example.com>",
        "a@b.com, c@d.com", "invalid", "<>", "x@", "@x", "a b@c.com", "group: a@b.com, c@d.com;", "jö@x.com", "a@exämple.com",
        ", ".join(f"user{i}@example{i}.com" for i in range(8)), "a@b.com (Comment)", '"quoted local"@x.com', "a@b.com junk",
        "Undisclosed recipients:;", "<a@b.com>", "Name <a@b.com", "a@[1.2.3.4]",
        "Very Long Display Name That Goes On And On And On Forever <very.long.address@some-domain.example.com>",
        "Élodie Müller-Lüdenscheidt et compagnie internationale <elodie@example.com>, José <jose@example.com>",
        "a.b@c.com", "a..b@c.com", ".a@c.com", "a.@c.com", "a@b.com,", ",a@b.com", "a@b.com;c@d.com", 'a"b@c.com',
        "(comment) a@b.com", "Name (comment) <a@b.com>", "=?utf-8?q?J=C3=B6hn?= <j@x.com>", '"Jöhn" <j@x.com>',
        "noreply@mailer.example", "user+tag@sub.domain.co.uk", "Robert'); DROP TABLE <a@b.com>",
    ]
    bodies = [
        "", "hello", "hello\n", "é", "line1\nline2", "line1\r\nline2\r\n", "a\rb", "x" * 77, "x" * 78, "x" * 79, "é" * 39,
        "é" * 40, "é" * 100, "ascii\n" * 12 + "é", "é\n" * 12, "From me\n", ".\n", "a=b\n", "trailing  \n", "tab\there",
        "\x00nul", "(no body available — message id 42)", "mixed é " * 30, "日本語", "🎉" * 30, "line\n" * 15, "x" * 1000,
        "ascii line\n" + "é" * 200 + "\n", "=\n=" * 10, "  leading", "\n\n\n",
    ]
    run = lambda **c: ("POST", "/emailx/run", c)  # noqa: E731
    steps = []
    for s in subjects:
        steps += [run(before=[["Subject", s]], body="x"), run(before=[["X-Custom-Header-With-A-Long-Name", s]])]
    for a in addresses:
        steps += [run(before=[["From", a], ["To", a]], body="x"), run(before=[["Cc", a]])]
    for b in bodies:
        steps.append(run(body=b))
        for kw in ({"subtype": "html"}, {"cte": "7bit"}, {"cte": "8bit"}, {"cte": "base64"}, {"cte": "quoted-printable"},
                   {"charset": "us-ascii"}, {"charset": "ascii"}):
            steps.append(run(body=b, kw=kw))
    rnd = random.Random(42)
    for _ in range(300):
        words = [rnd.choice(["a", "é", "word", "ünïcödé", "x" * rnd.randint(1, 30), "日本", "🎉", " ", "\t", "=?", "(", '"', "<", "@"])
                 for _ in range(rnd.randint(1, 25))]
        s = rnd.choice([" ", "", "  "]).join(words)
        steps.append(run(before=[["Subject", s]], body=s))
    for _ in range(150):
        names = ["", "John", "Jöhn Doé", "A, B", "Ünïcödé Nämé Lönger Than Usual", "x" * rnd.randint(1, 60), "日本 語"]
        locs = ["a", "john.doe", "jö", "x" * rnd.randint(1, 40), "a+b"]
        doms = ["b.com", "exämple.com", "sub." * rnd.randint(1, 5) + "example.org"]
        addrs = []
        for _ in range(rnd.randint(1, 4)):
            n, lp, d = rnd.choice(names), rnd.choice(locs), rnd.choice(doms)
            addrs.append(f"{n} <{lp}@{d}>" if n else f"{lp}@{d}")
        steps.append(run(before=[["To", ", ".join(addrs)]], body="b"))
    steps += [
        run(before=[["From", "a@b.com"], ["From", "c@d.com"]]), run(before=[["Subject", "a"], ["subject", "b"]]),
        run(before=[["Subject", "line\nbreak"]]), run(before=[["Subject", "line\rbreak"]]), run(before=[["Subject", "end\n"]]),
        run(before=[["Subject", "a\x0bb"]]), run(before=[["X-Two", "a"], ["X-Two", "b"]], body="z"),
        run(body="z", after=[["Subject", "after content"]]), run(body="é", kw={"cte": "7bit"}),
        run(body="x" * 100, kw={"cte": "7bit"}), run(body="x" * 100, kw={"cte": "8bit"}),
        run(before=[["subject", "lower name"], ["FROM", "a@b.com"], ["x-lower", "v"]], body="z"), run(), run(before=[["To", "a@b.com"]]),
        run(before=[["To", ["a@b.com", "Bé <c@d.com>"]], ["Cc", None], ["Subject", None]], body="z"),
    ]
    # token soups (seeded): every address-list path, several policies, str() and bytes() of the message
    atoms = ["a", "john", "doe", "é", "ü", "日本", "🎉", "x" * 30, "y" * 75, "=?utf-8?q?=C3=A9?=", "=?utf-8?b?w6k=?=",
             "=?iso-8859-1?q?caf=E9?=", "=?us-ascii?q?plain?=", "=?utf-8?q?bad", "=?x?y?z?=", "=?utf-8?Q?a_b?=", "word",
             "Réunion", "naïve", "élève", " ", "a.b", "a..b", ".", "..", "'", "!", "#", "%"]
    seps = [" ", "  ", "\t", " \t ", "", ",", ", ", ";", ":", "@", "<", ">", "(", ")", '"', "\\", "[", "]", "( c )",
            "(nested (c))", "(é)", '"q s"', '"é"', '"a\\"b"', " (c) "]
    pols = [None, None, None, "SMTP", "HTTP", "SMTPUTF8"]
    rnd = random.Random(7)
    for _ in range(1200):
        s = "".join(rnd.choice(atoms) if rnd.random() < 0.6 else rnd.choice(seps) for _ in range(rnd.randint(1, 18)))
        name = rnd.choice(["Subject", "To", "From", "Cc", "Reply-To", "X-Thing", "Resent-To", "Comments"])
        c = {"before": [[name, s]], "body": rnd.choice(["b", "é", "x" * 90, "line\r\nline"]), "str": rnd.random() < 0.3}
        p = rnd.choice(pols)
        if p:
            c["policy"] = p
        steps.append(run(**c))
    for _ in range(500):
        disp = "".join(rnd.choice(atoms) if rnd.random() < 0.6 else rnd.choice(seps) for _ in range(rnd.randint(0, 6)))
        local = rnd.choice(["a", "a.b", '"x y"', "a..b", ".a", "a.", "jö", "x" * 50, "a(c).b", "a . b", '""', '"a'])
        dom = rnd.choice(["b.com", "exämple.org", "[1.2.3.4]", "[1.2", "b. com", "sub(c).b.com", "", "b.com.", "-b-.com", "x" * 60 + ".com"])
        addr = f"{disp} <{local}@{dom}>" if rnd.random() < 0.6 else f"{local}@{dom}"
        if rnd.random() < 0.3:
            addr += rnd.choice([" junk", ", b@c.com", ";", " (c)", ",,", ", ,x@y.z", " <x@y.z>"])
        if rnd.random() < 0.2:
            addr = f"Group {disp}: {addr};"
        c = {"before": [[rnd.choice(["To", "From", "Cc", "Sender"]), addr]], "body": "b", "str": rnd.random() < 0.3}
        p = rnd.choice(pols)
        if p:
            c["policy"] = p
        steps.append(run(**c))
    steps.append(("GET", "/emailx/api", None))
    steps += [("GET", f"/emailx/policies?policy={p}&how={h}", None) for p in ("default", "SMTP", "SMTPUTF8", "HTTP")
              for h in ("bytes", "string", "str", "as_smtp", "utf8")]
    return steps


STEPS += _email_cases()


def _dns_cases() -> list:
    """fixtures/libapp/dnsx.py: dnspython against tests/dns_stub.py (DNS_STUB_PORT, started by
    scripts_start_libapp.sh): every behaviour of the stub over UDP and TCP, timeouts and lifetimes (their durations
    masked), failover, names, rdata types, resolv.conf files, the default resolver, a mail service's verification."""
    from urllib.parse import quote

    t = "p2a.test"
    resolve = lambda **c: ("POST", "/dnsx/resolve", c)  # noqa: E731
    steps = []
    for q, rdtype in [("mx", "MX"), ("mx", "TXT"), ("txt", "TXT"), ("txt", "MX"), ("cname", "TXT"), ("cname", "MX"),
                      ("cname", "CNAME"), ("dangling", "TXT"), ("loop", "TXT"), ("nodata", "MX"), ("nx", "MX"),
                      ("nxanswer", "TXT"), ("servfail", "MX"), ("refused", "MX"), ("formerr", "MX"), ("notimp", "MX"),
                      ("yx", "MX"), ("rcode12", "MX"), ("tc", "TXT"), ("tcfail", "TXT"), ("badid", "TXT"), ("case", "MX"),
                      ("a", "A"), ("a", "AAAA"), ("ns", "NS"), ("edns", "TXT"), ("extra", "TXT"), ("escape", "MX"),
                      ("_dmarc", "TXT"), ("default._domainkey", "TXT"), ("other.zone", "MX")]:
        name = f"{q}.{t}" if q != "other.zone" else "example.invalid"
        steps += [resolve(qname=name, rdtype=rdtype), resolve(qname=name + ".", rdtype=rdtype.lower(), kw={"tcp": True})]
    steps += [
        resolve(qname=f"nodata.{t}", rdtype="MX", kw={"raise_on_no_answer": False}),
        resolve(qname=f"cname.{t}", rdtype="MX", kw={"raise_on_no_answer": False}),
        resolve(qname=f"a.{t}"), resolve(qname=f"edns.{t}", rdtype="TXT", edns=True),
        resolve(qname=f"servfail.{t}", rdtype="MX", retry_servfail=True, lifetime=0.5),
        resolve(qname=f"timeout.{t}", rdtype="MX", timeout=0.2, lifetime=0.5),
        resolve(qname=f"slow.{t}", rdtype="TXT", timeout=0.25, lifetime=0.4),
        resolve(qname=f"junk.{t}", rdtype="TXT", timeout=0.2, lifetime=0.3),
        resolve(qname=f"mx.{t}", rdtype="MX", port="dead", timeout=0.2, lifetime=0.3),
        resolve(qname=f"mx.{t}", rdtype="MX", nameservers=["127.0.0.1", "127.0.0.1"]),
        resolve(qname=f"mx.{t}", rdtype="MX", nameservers=["::1", "127.0.0.1"], ports={"::1": "dead", "127.0.0.1": "stub"}, timeout=0.2),
        resolve(qname=f"servfail.{t}", rdtype="MX", nameservers=["::1", "127.0.0.1"], ports={"::1": "dead", "127.0.0.1": "stub"},
                timeout=0.2, lifetime=0.6),
        resolve(qname=f"mx.{t}", rdtype="MX", nameservers=[]), resolve(qname=f"mx.{t}", rdtype="MX", kw={"lifetime": 0.0}),
        resolve(qname="mx", rdtype="MX", kw={"search": True}),
    ]
    steps += [("GET", f"/dnsx/rdtypes?qname=mx.{t}&rdtype={r}", None)
              for r in ("MX", "mx", "TXT", "ANY", "AXFR", "OPT", "FOO", "TYPE15", "TYPE99999", "SOA", "SRV", "TYPE16", "CNAME")]
    for n in ("a..b", ".a", "a.", "", ".", "@", "a\\.b.c", "a\\065b", "a\\0", "a\\999x", "a\\", "x" * 64, ".".join(["x" * 60] * 5),
              "UPPER.Case", "w\\(e\\)ird\\;", "sp ace", "tab\tx", "a.b.c.", "\\046"):
        steps += [("GET", f"/dnsx/names?text={quote(n)}", None), ("GET", f"/dnsx/names?text={quote(n)}&relative=true", None),
                  ("GET", f"/dnsx/badnames?text={quote(n)}", None)]
    steps += [("GET", f"/dnsx/conf/{c}", None) for c in ("full.conf", "domain.conf", "nons.conf", "edns.conf", "badname.conf", "absent.conf")]
    steps += [("GET", "/dnsx/system", None), ("GET", "/dnsx/unconfigured", None), ("GET", "/dnsx/classes", None)]
    steps += [("GET", f"/dnsx/module?qname={q}.{t}&rdtype={r}", None) for q, r in (("mx", "MX"), ("txt", "TXT"), ("nodata", "MX"), ("nx", "TXT"))]
    steps += [("GET", f"/dnsx/module?qname=nodata.{t}&rdtype=MX&raise_on_no_answer=false", None)]
    steps += [("GET", f"/dnsx/verify?domain={d}", None) for d in (t, f"nx.{t}", f"servfail.{t}")]
    return steps


STEPS += _dns_cases()


def _pytz_cases() -> list:
    """fixtures/libapp/pytzx.py: every pytz zone's localize/fromutc over transitions, methods, lookups, errors."""
    import pytz
    WALLS = ["0001-01-01T00:30:00", "1850-06-01T12:00:00", "1900-01-01T00:00:00", "1911-03-11T00:00:00", "1945-07-01T12:00:00",
             "1970-01-01T00:00:00", "2000-03-26T02:30:00", "2026-03-08T02:30:00", "2026-03-29T02:30:00", "2026-03-29T01:59:59.999999",
             "2026-10-25T02:30:00", "2026-11-01T01:30:00", "2026-04-05T02:30:00", "2026-10-04T02:30:00", "2037-06-01T12:00:00",
             "2037-10-25T02:30:00", "2040-07-01T12:00:00", "9999-12-31T23:00:00"]
    INSTANTS = ["0001-01-01T00:00:00", "1850-06-01T12:00:00", "1901-12-13T20:45:52", "1901-12-13T20:45:51", "1970-01-01T00:00:00",
                "2026-03-29T00:59:59", "2026-03-29T01:00:00", "2026-10-25T00:59:59.5", "2026-10-25T01:00:00", "2037-10-25T01:00:00",
                "2040-07-01T12:00:00", "9999-12-31T12:00:00"]
    steps = []
    for z in pytz.all_timezones:
        steps.append(("POST", "/pytzx/localize", {"zone": z, "walls": WALLS}))
        steps.append(("POST", "/pytzx/fromutc", {"zone": z, "instants": INSTANTS}))
    for z in ["Europe/Paris", "America/New_York", "Australia/Sydney", "Asia/Kolkata", "EST", "UTC", "utc", "fixed:330", "fixed:-90",
              "Europe/Dublin", "Africa/Casablanca", "Pacific/Chatham", "America/Sao_Paulo", "Antarctica/Troll"]:
        steps.append(("POST", "/pytzx/methods", {"zone": z, "walls": WALLS}))
        steps.append(("POST", "/pytzx/localize", {"zone": z, "walls": WALLS, "add": 24, "flags": [False, True, None, 0, 1, 2, "", "x"]}))
    for n in ["Europe/Paris", "europe/paris", "EUROPE/PARIS", "UTC", "utc", "uTc", "Etc/UTC", "GMT", "Etc/GMT+5", "Etc/GMT_plus_5",
              "Etc/GMT_minus_14", "US/Eastern", "EST", "EST5EDT", "Zulu", "", "Nope", "Europe/Paris ", " UTC", "Europe//Paris",
              "../etc/passwd", "Asia/Calcutta", "America/Argentina/Buenos_Aires", "ROK", "Factory", "localtime", "posixrules",
              "zone.tab", "UCT", "Universal"]:
        steps.append(("POST", "/pytzx/zone", {"name": n}))
        steps.append(("POST", "/pytzx/zone", {"name": n, "bytes": True}))
    for v in [None, 3, 3.5, ["Europe/Paris"]]:
        steps.append(("POST", "/pytzx/zone", {"name": v}))
    for w in ["aware", "aware_utc", "naive_normalize", "naive_normalize_utc", "foreign_normalize", "zoneinfo_normalize",
              "fromutc_other", "fromutc_sibling", "fromutc_utc_other", "fromutc_utc_naive", "nonexistent", "ambiguous", "unknown",
              "unknown_plus", "unknown_type", "unknown_none", "nonascii", "bytes_nonascii", "fixed_big", "fixed_neg_big", "fixed_str",
              "localize_date", "localize_kw", "localize_bad_kw", "localize_missing", "normalize_is_dst", "utc_utcoffset_is_dst",
              "min", "max", "max_utc", "min_utc", "is_dst_two", "is_dst_one", "is_dst_str", "attr"]:
        steps.append(("GET", f"/pytzx/errors/{w}", None))
    for m in [0, 1, -1, 60, -60, 330, -330, 1439, -1439, 1440, -1440, 90, 545]:
        steps.append(("GET", f"/pytzx/fixed?minutes={m}", None))
    steps += [("GET", "/pytzx/lists", None), ("GET", "/pytzx/compare", None)]
    steps += [("GET", f"/pytzx/model?zone={z}", None) for z in ("Europe/Paris", "UTC", "America/St_Johns", "Asia/Kathmandu", "EST")]
    sfw = ["0001-01-01T00:00:00", "0005-03-01T07:05:09.000123", "0099-12-31T23:59:59", "0999-12-31T23:59:59", "1000-01-01T00:00:00",
           "1899-12-31T12:00:00", "2021-01-03T13:00:00", "2024-12-30T12:00:00", "2026-10-17T00:30:00", "2026-03-29T03:15:00", "9999-12-31T23:59:59.999999"]
    sff = ["%Y %G %C %F %y %g", "%c", "%x %X %D %T %R %r", "%a %A %b %B %h %m %d %e %j", "%H %I %k %l %M %S %f %p %P",
           "%u %w %U %W %V", "%z %:z %Z", "%n%t%%", "100%", "%%Y", "plain"]
    for z in (None, "Europe/Paris", "utc", "fixed:330", "fixed:-90", "America/St_Johns"):
        steps.append(("POST", "/pytzx/strftime", {"zone": z, "walls": sfw, "formats": sff}))
    for lib in ("pytz", "dateutil"):
        for zone, start in (("Europe/Paris", "2026-03-28T22:00:00"), ("Europe/Paris", "2026-10-24T23:00:00"),
                            ("America/New_York", "2026-03-07T23:00:00"), ("America/New_York", "2026-10-31T23:00:00"),
                            ("Australia/Lord_Howe", "2026-04-04T23:00:00"), ("UTC", "2026-01-01T00:00:00")):
            for expr in ("*/30 * * * *", "0 2 * * *", "30 2 * * *", "0 * * * *", "15 1,2,3 * * *"):
                for prev in (False, True):
                    steps.append(("POST", "/pytzx/cron", {"lib": lib, "zone": zone, "start": start, "expr": expr, "prev": prev, "n": 8}))
    return steps


STEPS += _pytz_cases()


def _dutz_cases() -> list:
    """fixtures/libapp/dutzx.py: gettz over the system's zone names (and pytz's), tzinfos over walls and instants, tzstr."""
    import os
    import pytz
    WALLS = ["0001-01-01T00:30:00", "1850-06-01T12:00:00", "1901-12-13T20:45:52", "1945-07-01T12:00:00", "1970-01-01T00:00:00",
             "2000-03-26T02:30:00", "2026-03-08T02:30:00", "2026-03-29T02:30:00", "2026-03-29T01:59:59.999999", "2026-10-25T02:30:00",
             "2026-10-25T01:59:59", "2026-11-01T01:30:00", "2026-04-05T02:30:00", "2026-10-04T02:30:00", "2037-10-25T02:30:00",
             "2038-01-19T03:14:08", "2040-07-01T12:00:00", "9999-12-31T23:00:00"]
    INSTANTS = ["0001-01-01T00:00:00", "1850-06-01T12:00:00", "1901-12-13T20:45:52", "1970-01-01T00:00:00", "2026-03-29T00:59:59",
                "2026-03-29T01:00:00", "2026-10-25T00:59:59.5", "2026-10-25T01:00:00", "2026-11-01T05:30:00", "2026-11-01T06:30:00",
                "2037-10-25T01:00:00", "2040-07-01T12:00:00", "9999-12-31T12:00:00"]
    names = sorted((set(pytz.all_timezones) | set(os.listdir("/usr/share/zoneinfo"))) - {"iso3166.tab"})
    steps = []
    for i in range(0, len(names), 20):
        steps.append(("POST", "/dutzx/gettz", {"names": names[i:i + 20], "walls": WALLS[5:12]}))
    steps.append(("POST", "/dutzx/gettz", {"names": ["UTC", "GMT", "utc", "Z", "", ":", ":UTC", "EST5EDT", "UTC+3", "GMT-2", "EST5EDT,M3.2.0,M11.1.0",
                                                      "CET-1CEST,M3.5.0,M10.5.0/3", "Nope", "nope1", "US/Eastern", "America/New York",
                                                      "Europe/Paris/", "../zoneinfo/Europe/Paris", "/usr/share/zoneinfo/Asia/Tokyo",
                                                      "/nonexistent", "CET", "CEST", "XYZ", "+03", "<+03>-3", "Etc/GMT+5", "posixrules",
                                                      "localtime", "zone.tab", "tzdata.zi", "leapseconds"], "walls": WALLS}))
    TZS = ["Europe/Paris", "America/New_York", "Australia/Sydney", "Asia/Kolkata", "Africa/Casablanca", "Europe/Dublin",
           "Pacific/Chatham", "America/Sao_Paulo", "Antarctica/Troll", "EST", "UTC", "utc", "UTC", "local", "off:None/3600",
           "off:BRST/-10800", "off:X/0", "str:EST5EDT", "str:CET-1CEST,M3.5.0,M10.5.0/3", "str:UTC+3", "str:AEST-10AEDT,M10.1.0,M4.1.0/3",
           "str:EST5EDT4,95/2,300/2", "str:IST-5:30", "str:EST+5EDT,4,1,0,7200,10,-1,0,7200,3600", "stdlib", "US/Eastern",
           "Europe/London", "America/St_Johns", "Asia/Tehran", "Pacific/Apia", "America/Godthab", "Europe/Moscow"]
    for t in TZS:
        steps.append(("POST", "/dutzx/walls", {"tz": t, "walls": WALLS}))
        steps.append(("POST", "/dutzx/instants", {"tz": t, "instants": INSTANTS}))
    steps.append(("GET", "/dutzx/objects", None))
    steps.append(("POST", "/dutzx/tzstr", {"strs": ["EST5EDT", "EST5", "EST", "UTC", "UTC0", "UTC+3", "GMT-3", "GMT+3", "EST5EDT,M3.2.0,M11.1.0",
        "EST5EDT,M3.2.0/2,M11.1.0/2", "CET-1CEST,M3.5.0,M10.5.0/3", "AEST-10AEDT,M10.1.0,M4.1.0/3", "NZST-12NZDT,M9.5.0,M4.1.0/3",
        "EST5EDT4,95/2,300/2", "EST5EDT4,J95/2,J300/2", "IST-5:30", "IST-0530", "EST+5EDT,4,1,0,7200,10,-1,0,7200,3600",
        "EST+5EDT,4,1,0,7200,10,-1,0,7200", "<+03>-3", "", "5", "EST5EDT,M3.2.0,M11.1.0,X", "EST5EDT;M3.2.0;M11.1.0",
        "EST 5 EDT", "ABCDEFGHIJ5", "EST5EDT,M3.5.0/26,M10.5.0/25", "EST5EDT,M3.5.0/1:30:30,M10.5.0/0130", "EST5EDT,0,365",
        "EST5EDT,J60,J300", "EST5EDT,J1,J365"]}))
    steps.append(("POST", "/dutzx/tzstr", {"strs": ["UTC+3", "GMT-3", "EST5EDT"], "posix": True}))
    for w in ["fromutc_other", "fromutc_naive", "fromutc_date", "utc_fromutc_other", "offset_fromutc", "exists_naive", "ambiguous_naive",
              "gettz_bytes", "gettz_abs", "gettz_abs_missing", "gettz_abs_bad", "gettz_colon", "gettz_space", "gettz_none", "tzstr_bad",
              "tzoffset_kw", "enfold_bad", "enfold_zero", "unhashable", "missing", "max", "min"]:
        steps.append(("GET", f"/dutzx/errors/{w}", None))
    steps.append(("GET", "/dutzx/compare", None))
    return steps


STEPS += _dutz_cases()


def _duparse_cases() -> list:
    """fixtures/libapp/duparsex.py: parse() over picked and generated strings with each option set, isoparse."""
    import random
    HAND = ["2026-10-16", "2026-10-16T10:20:30", "2026-10-16 10:20:30.123456", "2026-10-16T10:20:30Z", "2026-10-16T10:20:30+02:00",
            "2026-10-16T10:20:30-0530", "Thu Sep 25 10:36:28 BRST 2003", "Thu Sep 25 10:36:28 2003", "Thu Sep 25 10:36:28", "Thu Sep 10:36:28",
            "Thu 10:36:28", "Sep 10:36:28", "10:36:28", "10:36", "Sep 2003", "Sep", "2003", "10h36m28.5s", "10h36m28s", "10h36m", "10h",
            "10 h 36", "10 h 36.5", "36 m 5", "36 m 5 s", "36 m 05", "36 m 05 s", "10h am", "10h pm", "10am", "10pm", "10:00 am", "10:00 pm",
            "10:00am", "10:00pm", "10:00a.m", "10:00p.m", "10:00a.m.", "10:00p.m.", "October", "31-Dec-00", "0:01:02", "12h 01m02s am",
            "0:01:02 on July 4, 1976", "0:01:02 on July 4, 1976", "July 4, 1976 12:01:02 am", "Mon Jan  2 04:24:27 1995",
            "04.04.95 00:22", "Jan 1 1999 11:23:34.578", "950404 122212", "3rd of May 2001", "5th of March 2001", "1st of May 2003",
            "0099-01-01T00:00:00", "0031-01-01T00:00:00", "20080227T21:26:01.123456789", "13NOV2017", "0003-03-04", "December.0031.30",
            "090107", "2015 09 25", "2015-15-May", "02/03/2015", "03/02/2015", "2003/09/25", "25/09/2003", "1999/12/31 23:59:59",
            "Tue, 31 Dec 2002 23:59:59 GMT", "Wed, 01 Jan 2003 00:00:00 +0000", "Mon, 15 Aug 2005 15:52:01 +0000 (GMT)",
            "2003-09-25T10:49:41.5-03:00", "2003-09-25T10:49:41-03:00", "2003-09-25T10:49:41", "2003-09-25T10:49", "2003-09-25T10",
            "2003-09-25", "20030925T104941.5-0300", "20030925T104941-0300", "20030925T104941", "20030925T1049", "20030925T10",
            "20030925", "2003-09-25 10:49:41,502", "199709020908", "19970902090807", "09-25-2003", "25-09-2003", "10-09-2003",
            "10-09-03", "2003.09.25", "09.25.2003", "25.09.2003", "10.09.2003", "10.09.03", "2003/09/25", "09/25/2003", "25/09/2003",
            "10/09/2003", "10/09/03", "2003 09 25", "09 25 2003", "25 09 2003", "10 09 2003", "10 09 03", "25 09 03", "03 25 Sep",
            "25 03 Sep", "  July   4 ,  1976   12:01:02   am  ", "Wed, July 10, '96", "1996.07.10 AD at 15:08:56 PDT", "1996.July.10 AD 12:08 PM",
            "Tuesday, April 12, 1952 AD 3:30:42pm PST", "November 5, 1994, 8:15:30 am EST", "1994-11-05T08:15:30-05:00",
            "1994-11-05T08:15:30Z", "July 4, 1976", "7 4 1976", "4 jul 1976", "4 Jul 1976", "7-4-76", "19760704", "0:01:02 on July 4, 1976",
            "July 4, 1976 12:01:02 am", "Mon Jan  2 04:24:27 1995", "04.04.95 00:22", "Jan 1 1999 11:23:34.578", "950404 122212",
            "3rd of May 2001", "5th of March 2001", "1st of May 2003", "01h02m03", "01h02", "01h02s", "01m02", "01m02h", "2004 10 Apr 11h30m",
            "Apr 2003", "Feb 29 2003", "Feb 29 2004", "31 Feb", "Feb 30", "2026-02-30", "2026-13-01", "2026-00-01", "13/13/2026",
            "10:61", "25:00", "24:00", "10:00:61", "", " ", "abc", "not a date", "2026 abc", "abc 2026", "1234567", "123", "12345",
            "123456789", "12345678901", "1234567890123", "123456789012345", "99999999999999999999", "2020 99999999999999999999 h",
            "12:99999999999", "5 99999999999999999999999999999.5 h", "1111111111111111111111111111111.5 m", "10:00 +9999",
            "10:00 +25:00", "10:00 +0099", "10:00 -0530", "10:00 +05:30 (IST)", "10:00 +0530 (IST)", "10:00 UTC+3", "10:00 UTC-3", "10:00 GMT+3",
            "10:00 z", "10:00 Z", "10:00 UTC", "10:00 GMT", "10:00 CEST", "10:00 CET", "10:00 EST", "10:00 BRST", "10:00 XYZ", "10:00 PDT",
            "10:00 ABCDE", "10:00 ABCDEF", "10:00 abc", "10:00 Utc", "nan", "inf", "Infinity", "NaN 10:00", "١٢:٣٠", "²", "10:00 ²",
            "12.5", "12,5", "12,50", "1,234,567", "12.5.2026", "2026.", "Monday", "Mon", "monday 10:00", "Mon 2026-10-16", "Sunday Oct 2026",
            "Oct 2026 Sunday", "Jan-01", "Jan-01-2026", "Jan/01/2026", "01-Jan", "01-Jan-2026", "01/Jan/2026", "of Jan", "Jan of 2026",
            "Jan of 26", "Jan of x", "the 3rd", "10 am pm", "13 pm", "0 am", "12 am", "12 pm", "am", "pm 10", "10 a", "10 p", "noon",
            "midnight", "today", "tomorrow", "2026-10-16T10:20:30.1234567", "2026-10-16T10:20:30.", "10:20:30.12.34", "10:20.5", "10.5h",
            "10.25 h", "10.5 m", "1.5.2026", "T10:00", "2026T", "2026-W42", "2026-W42-5", "2026-289", "\x00 2026 \x00", "2026\t10\t16",
            "2026\n10\n16", "2026 10 16", "2026　10　16", "Ⅻ", "１２:３０", "2026-10-16 10:00 Europe/Paris",
            "10:00:00.000000001", "10:00:00.9999999", "0:00", "00:00:00", "23:59:59.999999", "12:00:00 am", "12:00:00 pm"]
    TOKENS = ["2026", "10", "16", "25", "3", "99", "0", "32", "13", "1999", "05", "Oct", "October", "Sept", "MAY", "Mon", "friday", "12:30",
              "12:30:45.123456", "+0200", "-05:00", "+3", "Z", "UTC", "EST", "BRST", "PM", "am", "a.m.", "of", "the", "at", "on", "and",
              "T", "-", "/", ".", " ", ",", ";", "'", "1st", "3rd", "th", "h", "m", "s", "min", "hours", "1.5", "12,5", "(", ")", "x", "ad",
              "٣", "²", "nan", " "]
    rnd = random.Random(16)
    GEN = []
    for _ in range(3000):
        n = rnd.randint(1, 7)
        sep = rnd.choice(["", " ", " ", "-", "/", ":", "."])
        GEN.append(sep.join(rnd.choice(TOKENS) for _ in range(n)))
    KWS = [{}, {"dayfirst": True}, {"yearfirst": True}, {"dayfirst": True, "yearfirst": True}, {"fuzzy": True},
           {"fuzzy_with_tokens": True}, {"ignoretz": True}, {"tzdict": True}, {"tzfn": True}, {"aware_default": True}]
    steps = []
    for kw in KWS:
        cases = [{"s": s, "kw": kw} for s in HAND]
        for i in range(0, len(cases), 100):
            steps.append(("POST", "/duparsex/parse", {"cases": cases[i:i + 100]}))
    for kw in KWS[:6] + [{"tzdict": True}]:
        cases = [{"s": s, "kw": kw} for s in GEN]
        for i in range(0, len(cases), 150):
            steps.append(("POST", "/duparsex/parse", {"cases": cases[i:i + 150]}))
    steps.append(("POST", "/duparsex/parse", {"cases": [{"s": s, "bytes": True} for s in HAND[:40]]}))
    ISO = ["2026", "2026-10", "202610", "2026-10-16", "20261016", "2026-W42", "2026W42", "2026-W42-5", "2026W425", "2026-W53", "2026-W54",
           "2026-W00", "2026-W42-8", "2026-289", "2026289", "2024-366", "2026-366", "2026-000", "2026-10-16T10", "2026-10-16T10:20",
           "2026-10-16T1020", "2026-10-16T10:20:30", "2026-10-16T102030", "2026-10-16T10:20:30.123", "2026-10-16T10:20:30,123456789",
           "2026-10-16T10:20:30.", "2026-10-16T10:20:30Z", "2026-10-16T10:20:30z", "2026-10-16T10:20:30+02", "2026-10-16T10:20:30+0200",
           "2026-10-16T10:20:30+02:00", "2026-10-16T10:20:30-00:00", "2026-10-16T10:20:30+00", "2026-10-16T10:20:30+2", "2026-10-16T10:20:30+24:00",
           "2026-10-16T10:20:30+02:60", "2026-10-16T24:00", "2026-10-16T24:00:00", "2026-10-16T24:00:01", "9999-12-31T24:00", "2026-10-16 10:20",
           "2026-10-16X10:20", "2026-10-16T10:20:3", "2026-10-16T10:2030", "2026-10-1610:20", "2026-1016", "202610-16", "2026-13-01",
           "2026-02-30", "abcd", "", "20", "2026-", "2026-1", "+2026", " 2026", "2026_10", "20 26", "2026-10-16T", "Z", "z", "+02", "-0530",
           "+05:30", "-00:00", "+00:00", "+0", "+2400", "02", "10:20", "1020", "10:20:30.5", "24:00", "24:00:00.000001", "2026-10-16T10:20:30.123456789012",
           "2026-W01-1", "2027-W01-1", "0001-W01-1", "9999-W52-7", "0001-001", "café", "2026-10-16Té"]
    steps.append(("POST", "/duparsex/iso", {"strs": ISO}))
    steps.append(("POST", "/duparsex/iso", {"strs": ISO, "sep": " "}))
    steps.append(("POST", "/duparsex/iso", {"strs": ISO, "sep": "T"}))
    for w in ["int", "none", "badkw", "badkw_far", "tzinfos_bad", "iso_sep", "iso_sep_digit", "iso_int", "nodefault", "weekday_today",
              "kwargs", "err_attrs", "err_cause"]:
        steps.append(("GET", f"/duparsex/errors/{w}", None))
    return steps


STEPS += _duparse_cases()


def _ctools_cases() -> list:
    """fixtures/libapp/ctoolsx.py: scripts of operations on each cache kind, the decorated functions and methods."""
    import random
    steps = []
    OPS = ["set", "get", "getm", "get1", "del", "in", "pop", "popd", "setdefault", "popitem", "len", "bool", "update", "update_kw", "eq", "clear"]
    KEYS = ["a", "b", "c", "d", 1, 2, 2.0, True, None, "e"]
    VALS = ["x", "yy", "zzz", "", [1, 2], {"k": 1}, 7]
    rnd = random.Random(7)
    for kind in ("Cache", "FIFOCache", "LRUCache", "TTLCache", "TTLWall"):
        for sized in (False, True):
            for maxsize in (0, 1, 3, 5):
                for ttl in ((3600, 0, -1) if kind.startswith("TTL") else (3600,)):
                    if kind == "TTLWall" and sized:
                        continue
                    for _seed in range(3):
                        ops = []
                        for _ in range(40):
                            op = rnd.choice(OPS[:-1] if rnd.random() > 0.03 else OPS)
                            ops.append([op, rnd.choice(KEYS), rnd.choice(VALS)])
                        if kind.startswith("TTL"):
                            ops.append(["expire", None, None])
                        steps.append(("POST", "/ctoolsx/script", {"kind": kind, "maxsize": maxsize, "ttl": ttl, "sized": sized, "ops": ops}))
    for w in ["negative", "too_large", "kw", "missing", "popitem_empty", "popitem_empty_lru", "popitem_empty_ttl", "float_max", "unhashable", "zero"]:
        steps.append(("GET", f"/ctoolsx/errors/{w}", None))
    CALLS = [["square", [2], {}], ["square", [2], {}], ["square", [2], {"scale": 2}], ["square", [3], {}], ["square", [4], {}], ["square", [5], {}],
             ["square", [2], {}], ["lru", [1], {}], ["lru", [2], {}], ["lru", [1], {}], ["lru", [3], {}], ["lru", [2], {}], ["lru", "clear", {}],
             ["lru", [1], {}], ["custom", [1], {"b": 5}], ["custom", [1], {"b": 7}], ["locked", [1], {}], ["locked", [1], {}], ["uncached", [1], {}],
             ["uncached", [1], {}], ["expiring", [1], {}], ["expiring", [1], {}], ["sized", ["ab"], {}], ["sized", ["ab"], {}], ["sized", ["abcd"], {}],
             ["sized", ["abcd"], {}], ["sized", ["x"], {}], ["func_lru", [1], {}], ["func_lru", [1], {}], ["func_lru", [2], {}], ["func_lru", [3], {}],
             ["func_lru", [1], {}], ["func_lru_bare", [1], {}], ["func_lru_bare", [1], {}], ["func_ttl", [1], {}], ["func_ttl", [1], {}],
             ["func_fifo", [2], {}], ["func_fifo", [2], {}], ["func_fifo", "clear", {}], ["func_fifo", [2], {}], ["square", [[1]], {}],
             ["square", [], {}], ["lru", [{"a": 1}], {}]]
    steps.append(("POST", "/ctoolsx/decorated", {"calls": CALLS}))
    steps.append(("POST", "/ctoolsx/decorated", {"calls": CALLS}))
    MCALLS = [["a", "find", [1]], ["a", "find", [1]], ["b", "find", [1]], ["a", "find", [2]], ["a", "find", [3]], ["a", "find", [1]],
              ["a", "label", [1]], ["b", "label", [1]], ["a", "label", [1]], ["a", "locked", [5]], ["a", "locked", [5]], ["a", "find", [[1]]],
              ["a", "label", [[1]]], ["a", "find", []], ["b", "locked", [1]]]
    steps += [("POST", "/ctoolsx/methods", {"calls": MCALLS, "size": n}) for n in (0, 1, 2, 5)]
    return steps


STEPS += _ctools_cases()
