"""Scenario of fixtures/libsrcapp: libraries translated from their source (--translate-libs auto). No database.
Compare the logs too (--logs): a library's frames are in the tracebacks of the 500s. `metric()` of 1e21 or more
computes `10 ** 21` (an int beyond 64 bits: OverflowError in the binary, a documented difference), left out."""

FLOATS = ["0", "1", "-1", "0.5", "999", "1000", "1500", "-1500", "123456789", "1e-9", "1e18", "4.2e20", "1e-30", "inf",
          "-inf", "nan", "3.14159", "x"]

STEPS: list = [("GET", "/health", None)]
STEPS += [("GET", f"/hum/metric/{x}", None) for x in FLOATS]
STEPS += [("GET", f"/hum/metric/{x}?unit=V&precision={p}", None) for x in ("1500", "0.0012", "-4e7") for p in (1, 3, 5, 0)]
STEPS += [("GET", f"/hum/scientific/{x}", None) for x in ("0", "1", "-1", "500", "12345", "0.3", "1e-12", "1e300",
                                                           "-0.001", "nan", "inf", "99999999999", "x")]
STEPS += [("GET", f"/hum/scientific/{x}?precision={p}", None) for x in ("12345", "0.0042") for p in (0, 1, 4)]
STEPS += [("GET", f"/hum/clamp/{x}", None) for x in ("0.5", "-3", "1e10", "nan", "inf")]
STEPS += [("GET", f"/hum/clamp/{x}?floor=1&ceil=10", None) for x in ("0.5", "5", "50", "1", "10")]
STEPS += [("GET", f"/hum/clamp/{x}?fmt={f}&floor=0.01", None) for x in ("0.001", "0.5") for f in ("{:.0%25}", "{:.2f}", "%25.1f",
                                                                                                   "{}x", "{:>8}")]
STEPS += [("GET", "/hum/list" + q, None) for q in ("", "?item=a", "?item=a&item=b", "?item=a&item=b&item=c",
                                                    "?item=&item=", "?item=%C3%A9&item=x&item=y&item=z")]

URIS = ["http://a b/~x", "x%zz%41", "%", "%2", "a%2Gb", "a%41%2fb%7e", "http://h/%C3%A9?q=%20 ", "é", "", "a%%41"]
STEPS += [("GET", f"/rq/{w}?u={u.replace('%', '%25').replace(' ', '%20').replace('?', '%3F')}", None)
          for w in ("requote", "unquote") for u in URIS]


def reset(db: str) -> None:
    pass

INTS = ["0", "1", "2", "3", "4", "9", "10", "11", "12", "13", "21", "22", "101", "111", "112", "1000", "-5", "-1234567",
        "1234567890", "9223372036854775807", "x"]
STEPS += [("GET", f"/hum/size/{n}", None) for n in ("0", "1", "-1", "999", "1000", "1023", "1024", "3000000", "-3000000",
                                                    "1000000000000", "9223372036854775807", "-9223372036854775808", "x")]
STEPS += [("GET", f"/hum/size/{n}?binary=true", None) for n in ("1", "1023", "1024", "-4096", "5000000")]
STEPS += [("GET", f"/hum/size/{n}?gnu=true", None) for n in ("1", "300", "3000", "1024", "-2048", "5000000")]
STEPS += [("GET", f"/hum/size/{n}?binary=true&gnu=true", None) for n in ("0", "1024")]
STEPS += [("GET", f"/hum/size/{n}?fmt={f}", None) for n in ("1500", "1") for f in ("%.3f", "%d", "%.0f", "%s", "%25.2f")]
STEPS += [("GET", f"/hum/size-s/{s}", None) for s in ("42", "1e3", "abc", "1024.5", "-7", "%2012%20", "1_000", "1__0", "_1", "1_", "nan", "١٢٣")]
STEPS += [("GET", f"/hum/words/{n}", None) for n in INTS]

# the standard library translated from Lib/
STEPS += [("GET", f"/std/entity/{n}", None) for n in ("amp", "eacute", "nbsp", "Euro", "nope", "AMP", "lt", "zwj")]
STEPS += [("GET", f"/std/gettext/{m}" + q, None) for m in ("file", "%C3%A9t%C3%A9") for q in ("", "?fallback=true")]
STEPS += [("GET", f"/std/color?r={r}&g={g}&b={b}", None) for r, g, b in ((0, 0, 0), (1, 1, 1), (1, 0, 0), (0.2, 0.4, 0.6),
                                                                          (0.5, 0.5, 0.5), (0.9, 0.1, 0.3), (2, -1, 0.5))]
STEPS += [("GET", f"/std/kw/{w}", None) for w in ("if", "match", "_", "type", "case", "foo", "None", "async")]

# generators that call generators (fixtures/libsrcapp/gens.py)
STEPS += [("GET", f"/gens/doubled/{n}", None) for n in ("0", "1", "3", "4", "6", "-1", "x")]
STEPS += [("GET", "/gens/bag" + q, None) for q in ("", "?w=x", "?w=", "?w=%C3%A9t%C3%A9,b,c")]
STEPS += [("GET", f"/gens/drained/{n}", None) for n in ("0", "3")]
STEPS += [("GET", "/gens/lines" + q, None) for q in ("", "?w=a", "?w=")]
STEPS += [("GET", f"/gens/streamed/{n}", None) for n in ("0", "5")]
