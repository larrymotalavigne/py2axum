"""Scenario of fixtures/qrapp: qrcode 8.2 with Pillow's PilImage, the PNG compared byte for byte (data modes and their
split, versions 1 to 40 and their mode-size bounds, the four levels, the eight masks, IDAT chunks of large images),
and qrcode's exceptions. No database."""
from urllib.parse import quote

STEPS: list = [("GET", "/health", None)]
STEPS += [("GET", "/qr/totp" + q, None) for q in (
    "", "?email=bob%2Btag%40example.org&secret=ABCDEFGHIJKLMNOPQRSTUVWXYZ234567", "?issuer=My%20App&email=%C3%A9l%C3%A8ve%40exemple.fr",
    "?secret=" + "A" * 103)]
DATA = ["hello", "", "0", "12345678901234567890", "123456789012345678901", "HELLO WORLD", "HELLO WORLD $%*+-./:",
        "hello world", "été 😀", "123\n", "0123456789ABCDEFGHIJ0123456789abc", "https://example.com/a?b=1",
        "a\x00b", "  ", "\n"]
STEPS += [("GET", "/qr/data?d=" + quote(d), None) for d in DATA]
STEPS += [("GET", "/qr/data?d=" + quote(d) + "&opt=" + o, None) for d in DATA[:12] for o in ("0", "4", "1")]
STEPS += [("GET", f"/qr/data?d=x&ec={ec}&mask={m}", None) for ec in "LMQH" for m in range(8)]
STEPS += [("GET", f"/qr/data?d={quote(d)}&rep={r}&ec={ec}", None)
          for d, r in (("A", 600), ("7", 1500), ("z", 300), ("é", 700), ("a1", 1000)) for ec in "LH"]
# the mode-size bounds (versions 9/10 and 26/27) and the largest codes
STEPS += [("GET", f"/qr/data?d=b&rep={n}&ec=M&box=1&border=0", None) for n in (213, 214, 230, 1000, 1030, 1100, 2331, 2332)]
STEPS += [("GET", f"/qr/data?d=9&rep={n}&ec=L&box=1", None) for n in (7089, 7090)]
# large images: several IDAT chunks (blocks of max(65536, 4 * width) bytes)
STEPS += [("GET", f"/qr/data?d={d}&rep={n}&box={b}&ec={ec}", None)
          for d, n, b, ec in (("b", 100, 1, "M"), ("b", 100, 2, "M"), ("b", 2000, 25, "M"), ("b", 2331, 40, "M"),
                              ("b", 2331, 100, "M"), ("b", 2953, 100, "L"), ("%C3%A9%F0%9F%98%80a1", 250, 110, "L"),
                              ("Zz9", 900, 64, "Q"))]
STEPS += [("GET", f"/qr/data?d=hello&v={v}&fit={f}", None) for v in (1, 5, 7, 40) for f in ("true", "false")]
STEPS += [("GET", f"/qr/data?d=b&rep=100&v={v}&fit=false", None) for v in (1, 4, 5)]
STEPS += [("GET", "/qr/data?d=hello&colors=" + c, None) for c in ("BLACK,White", "black,white")]
STEPS += [("GET", "/qr/data?d=hello&" + q, None) for q in ("box=0", "border=-1", "border=0", "v=0", "v=41", "mask=8",
                                                           "mask=-1", "ec=L&box=3&border=1&mask=5")]
STEPS += [("GET", f"/qr/bytes?n={n}&byte={b}&ec={ec}", None) for n in (1, 50, 400) for b in (0, 255) for ec in "LMQH"]
STEPS += [("GET", f"/qr/bytes?n={n}&v={v}&fit=false", None) for n, v in ((300, 10), (2000, 30))]
STEPS += [("GET", f"/qr/int/{n}", None) for n in (0, 7, -42, 123456789)]
STEPS += [("GET", "/qr/make" + q, None) for q in ("", "?d=x&box=1", "?d=" + quote("https://example.com/" + "p" * 300))]
STEPS += [("GET", "/qr/twice", None)]
STEPS += [("GET", f"/qr/init?case={c}", None) for c in ("box0", "boxstr", "boxbad", "border", "v0", "v41", "mask8", "maskstr",
                                                        "ec4", "ec4fixed", "positional", "embedded")]


def reset(db: str) -> None:
    pass
