"""Arguments the runtime used to drop or ignore (audit of 09/10/2026, after itsdangerous' `return_timestamp=`):
`str.encode(encoding, errors)`, `str(b, encoding)`, `find/count/startswith(sub, start, end)`, `list.index(x, start,
stop)`, `set.update(*others)`, `Match.groupdict(default=)`, Starlette responses given positionally, `**_params` of
email.mime, `Path.read_text/write_text(encoding=)`, csv `restval=`/`restkey=`, Redis `scan_iter(pattern)` and
`set(px=timedelta)`."""
import csv
import io
import json
import re
from datetime import UTC, datetime, timedelta
from typing import Optional, Union
from email.mime.application import MIMEApplication
from email.mime.base import MIMEBase
from email.mime.multipart import MIMEMultipart
from pathlib import Path

from fastapi import APIRouter
from fastapi.responses import FileResponse, HTMLResponse, PlainTextResponse, RedirectResponse

from .rds import CLIENT

router = APIRouter(prefix="/kw")


@router.post("/encode")
async def encode(body: dict):
    out = []
    for enc, errors in (("utf-8", "strict"), ("ascii", "ignore"), ("ascii", "replace"), ("latin-1", "replace"),
                        ("latin_1", "strict"), ("ASCII", "strict"), ("us-ascii", "strict"), ("utf_8", "strict")):
        try:
            b = body["text"].encode(enc, errors)
            out.append([enc, errors, list(b), str(b, "latin-1"), b.decode("latin-1")])
        except UnicodeEncodeError as e:
            out.append([enc, errors, type(e).__name__, str(e)])
    try:
        bad = str(body["text"].encode(), "ascii")
    except UnicodeDecodeError as e:
        bad = str(e)
    return {"out": out, "default": list(body["text"].encode()), "kw": list(body["text"].encode(encoding="latin-1", errors="ignore")),
            "str": str(body["text"].encode("utf-8"), "utf-8"), "bad": bad}


@router.get("/find")
async def find(s: str, sub: str, start: int = 0, end: int | None = None):
    try:
        index = s.index(sub, start, end)
    except ValueError as e:
        index = str(e)
    return {"find": s.find(sub, start, end), "rfind": s.rfind(sub, start, end), "count": s.count(sub, start, end),
            "starts": s.startswith(sub, start, end), "ends": s.endswith(sub, start, end), "index": index,
            "starts_tuple": s.startswith((sub, "zz"), start), "find1": s.find(sub, start)}


@router.post("/seq")
async def seq(body: list[int]):
    t = tuple(body)
    out = []
    for args in ((2,), (2, 1), (2, 3), (2, -2), (2, 0, 2), (2, 10), (2, -100, 100)):
        row = []
        for xs in (body, t):
            try:
                if len(args) == 1:
                    row.append(xs.index(args[0]))
                elif len(args) == 2:
                    row.append(xs.index(args[0], args[1]))
                else:
                    row.append(xs.index(args[0], args[1], args[2]))
            except ValueError as e:
                row.append(str(e))
        out.append(row)
    s = {1}
    s.update([2, 3], (4,), {5})
    m = re.match(r"(?P<a>x)?(?P<b>y)", "y")
    rx = re.compile(r"(?m)^b+|c")
    text = "abbc\nbbx"
    searches = []
    for pos, end in ((0, None), (1, None), (2, None), (4, None), (5, None), (1, 3), (-3, 100), (6, 2), (9, None)):
        m = rx.search(text, pos) if end is None else rx.search(text, pos, end)
        searches.append(None if m is None else [m.group(), m.start(), m.end(), m.span(), m.string == text])
    kwm = rx.search(text, pos=2, endpos=4)
    return {"index": out, "set": sorted(s), "search": searches, "kw": kwm.group() if kwm else None, "groupdict": m.groupdict(default="-"), "groups": m.groups(default="?"),
            "plain": m.groupdict()}


@router.get("/resp/{kind}")
async def resp(kind: str):
    if kind == "text":
        return PlainTextResponse("a;b", 201, {"x-kw": "1"}, "text/csv")
    if kind == "html":
        return HTMLResponse("<p>x</p>", 202)
    if kind == "redirect":
        return RedirectResponse("/kw/resp/text", 303, {"x-kw": "2"})
    return FileResponse("fixtures/dynapp/test_alembic.ini", 200, None, None, None, "réglages.txt")


@router.get("/mime")
async def mime():
    a = MIMEApplication(b"%PDF-1.4", Name="facture.pdf")
    b = MIMEApplication(b"x", "zip", name="a.zip")
    m = MIMEMultipart("mixed", charset="utf-8")
    c = MIMEBase("text", "csv", name="x.csv")
    return {"app": a.get("Content-Type"), "zip": b.get("Content-Type"), "multi": m.get("Content-Type"),
            "base": c.get("Content-Type"), "text": a.as_string()}


@router.post("/path")
async def path(body: dict):
    p = Path("/tmp/py2axum_kw_path.txt")
    n = p.write_text(body["text"], encoding="latin-1")
    raw = list(p.read_bytes())
    back = p.read_text(encoding="latin-1")
    try:
        utf = p.read_text()
    except UnicodeDecodeError as e:
        utf = str(e)
    m = p.write_text(body["text"], "ascii", "replace")
    return {"n": n, "raw": raw, "back": back, "utf": utf, "m": m, "ascii": p.read_text("ascii")}


@router.post("/csv")
async def csv_rows(body: dict):
    rows = list(csv.DictReader(io.StringIO(body["text"]), restval="?", restkey="extra"))
    plain = list(csv.DictReader(io.StringIO(body["text"])))
    # a None key (DictReader's default restkey): "null" for json.dumps and FastAPI's encoder
    return {"rows": rows, "plain": plain, "dumps": json.dumps({None: 1, True: 2, "a": [None]})}


@router.post("/redis")
async def redis_scan():
    c = CLIENT
    for k in ("kwscan:1", "kwscan:2", "kwother:1"):
        await c.set(k, "1")
    await c.set("kwpx", "1", px=timedelta(seconds=50))
    keys = sorted([k async for k in c.scan_iter("kwscan:*")])
    typed = sorted([k async for k in c.scan_iter("kw*", 100, "string")])
    ttl = await c.ttl("kwpx")
    await c.delete("kwscan:1", "kwscan:2", "kwother:1", "kwpx")
    return {"keys": keys, "typed": typed, "px": 0 < ttl <= 50}


@router.post("/isinstance")
async def isinstance_union(body: dict):
    """`isinstance(x, int | float)` raised TypeError (a parameterized generic) in the binary: a production app's 2FA
    login answered 500 (`isinstance(iat, int | float)` reading its temporary token, 09/10/2026)."""
    out = [[isinstance(v, int | float), isinstance(v, str | None), isinstance(v, Optional[int]),
            isinstance(v, Union[str, list]), isinstance(v, (int | None, dict))] for v in body["vals"] + [None]]
    iat = 1760000000.123456
    when = datetime.fromtimestamp(iat, UTC) if isinstance(iat, int | float) else None
    return {"out": out, "when": when.isoformat()}
