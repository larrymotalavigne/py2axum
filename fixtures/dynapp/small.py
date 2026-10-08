"""Constructions of the small ATOM apps (coproscan, ao-radar, fr-opendata-mcp, transcript):
response_class= wrapping a returned value, uuid.UUID (Pydantic, parameters, Uuid columns)."""
import uuid
from datetime import datetime

from fastapi import APIRouter, HTTPException, Query, Response
from fastapi.responses import FileResponse, HTMLResponse, PlainTextResponse, RedirectResponse
from pydantic import BaseModel
from sqlalchemy import select

from .db import DbDep
from .models import Analysis, AnalysisDoc

router = APIRouter(prefix="/small")


@router.get("/html", response_class=HTMLResponse, include_in_schema=False)
async def html(response: Response, name: str = "monde"):
    response.headers["X-Page"] = "1"
    response.set_cookie("seen", "oui")
    return "<h1>Bonjour " + name.replace("<", "&lt;") + " é</h1>"


@router.get("/html-kinds/{kind}", response_class=HTMLResponse)
async def html_kinds(kind: str):
    if kind == "bytes":
        return b"<p>octets</p>"
    if kind == "none":
        return None
    if kind == "obj":
        return HTMLResponse("<p>objet</p>", status_code=202)
    if kind == "dict":
        return {"a": 1}  # HTMLResponse.render(dict) -> AttributeError: 500
    return f"<p>{kind}</p>"


@router.post("/html-created", response_class=HTMLResponse, status_code=201)
async def html_created():
    return "<p>créé</p>"


@router.delete("/html-gone", response_class=HTMLResponse, status_code=204)
async def html_gone():
    return "<p>ignored</p>"


@router.get("/html-status", response_class=HTMLResponse)
async def html_status(response: Response):
    response.status_code = 203
    return "<p>203</p>"


@router.get("/llms.txt", response_class=PlainTextResponse)
async def llms_txt() -> str:
    lines = ["# Titre", "", "> résumé"]
    for i in range(3):
        lines.append(f"- outil {i}")
    return "\n".join(lines) + "\n"


@router.get("/raw", response_class=Response)
async def raw():
    return "brut"


@router.get("/go", response_class=RedirectResponse)
async def go(to: str = "/small/html"):
    return to + "?x=é b"


@router.get("/go-found", response_class=RedirectResponse, status_code=302)
async def go_found():
    return "https://example.com/a"


@router.get("/file", response_class=FileResponse)
async def file():
    return "storage-test/fixed.pdf"


# ---------------------------------------------------------------- uuid.UUID


class DocIn(BaseModel):
    name: str
    ref: uuid.UUID | None = None


class AnalysisIn(BaseModel):
    id: uuid.UUID | None = None
    address: str
    docs: list[DocIn] = []


class DocOut(BaseModel):
    name: str
    ref: uuid.UUID | None
    analysis_id: uuid.UUID


class AnalysisOut(BaseModel):
    id: uuid.UUID
    address: str
    findings: dict | None
    created_at: datetime
    docs: list[DocOut] = []


@router.post("/analyses", status_code=201)
async def create_analysis(body: AnalysisIn, db: DbDep):
    a = Analysis(address=body.address, findings={"n": len(body.docs)})
    if body.id is not None:
        a.id = body.id
    db.add(a)
    await db.flush()
    for d in body.docs:
        db.add(AnalysisDoc(analysis_id=a.id, name=d.name, ref=d.ref))
    await db.flush()
    generated = body.id is None
    # a drawn uuid4 is random: only its shape is returned
    return {"id": None if generated else a.id, "shape": [type(a.id).__name__, a.id.version, len(a.id.hex), str(a.id)[14]]}


@router.get("/analyses/{analysis_id}", response_model=AnalysisOut)
async def get_analysis(analysis_id: uuid.UUID, db: DbDep):
    a = await db.get(Analysis, analysis_id)
    if a is None:
        raise HTTPException(status_code=404, detail=f"Analyse {analysis_id} introuvable")
    docs = (await db.execute(select(AnalysisDoc).where(AnalysisDoc.analysis_id == a.id).order_by(AnalysisDoc.name))).scalars().all()
    return AnalysisOut(id=a.id, address=a.address, findings=a.findings, created_at=a.created_at,
                       docs=[DocOut(name=d.name, ref=d.ref, analysis_id=d.analysis_id) for d in docs])


@router.get("/analyses")
async def list_analyses(db: DbDep, ids: list[uuid.UUID] = Query([])):
    rows = (await db.execute(select(Analysis).where(Analysis.id.in_(ids)).order_by(Analysis.address))).scalars().all()
    by_id = {a.id: a.address for a in rows}
    return {"found": [str(a.id) for a in rows], "by_id": by_id, "first": rows[0].id if rows else None,
            "eq": [a.id == ids[0] for a in rows] if ids else []}


@router.get("/uuid-ops/{u}")
async def uuid_ops(u: uuid.UUID, other: uuid.UUID | None = None):
    return {"str": str(u), "hex": u.hex, "version": u.version, "repr": repr(u), "same": u == other,
            "isinstance": isinstance(u, uuid.UUID), "parsed": uuid.UUID(u.hex) == u, "key": {u: 1}}


# ---------------------------------------------------------------- HTTPBasic + Jinja2Templates (transcript's pages)
import hmac  # noqa: E402
import secrets  # noqa: E402
from datetime import date  # noqa: E402
from pathlib import Path  # noqa: E402

from fastapi import Depends, Request  # noqa: E402
from fastapi.security import HTTPBasic, HTTPBasicCredentials  # noqa: E402
from fastapi.templating import Jinja2Templates  # noqa: E402

templates = Jinja2Templates(directory=str(Path(__file__).resolve().parent / "templates" / "pages"))
_basic = HTTPBasic()
_maybe_basic = HTTPBasic(realm="zone é", auto_error=False)
VIEW_USER, VIEW_PASSWORD = "admin", "s3cret"


def require_view(credentials: HTTPBasicCredentials = Depends(_basic)) -> None:
    user_ok = secrets.compare_digest(credentials.username, VIEW_USER)
    pw_ok = bool(VIEW_PASSWORD) and hmac.compare_digest(credentials.password, VIEW_PASSWORD)
    if not (user_ok and pw_ok):
        raise HTTPException(status_code=401, detail="unauthorized", headers={"WWW-Authenticate": "Basic"})


@router.get("/pages", response_class=HTMLResponse)
async def pages(request: Request, db: DbDep, _: None = Depends(require_view)):
    # the analysis without an id in its request has a random uuid4: left out
    items = (await db.execute(select(Analysis).where(Analysis.address != "1 rue A").order_by(Analysis.address))).scalars().all()
    return templates.TemplateResponse(request, "list.html", {"items": items, "when": datetime(2026, 1, 2, 3, 4), "day": date(2026, 5, 6)})


@router.get("/pages/{analysis_id}", response_class=HTMLResponse)
async def page(analysis_id: uuid.UUID, request: Request, db: DbDep, _: None = Depends(require_view)):
    item = await db.get(Analysis, analysis_id)
    if not item:
        raise HTTPException(status_code=404, detail="not found")
    return templates.TemplateResponse(request, "detail.html", {"item": item})


@router.get("/pages-txt")
async def pages_txt(request: Request, y: str | None = None):
    return templates.TemplateResponse(request=request, name="note.txt", context={"x": "<i>", "y": y}, status_code=201, headers={"X-T": "1"})


@router.get("/pages-missing")
async def pages_missing(request: Request):
    from jinja2 import TemplateNotFound
    try:
        return templates.TemplateResponse(request, "nope.html")
    except TemplateNotFound as e:
        return {"missing": str(e)}


@router.get("/basic-opt")
async def basic_opt(creds: HTTPBasicCredentials | None = Depends(_maybe_basic)):
    return None if creds is None else {"user": creds.username, "password": creds.password, "dump": creds.model_dump()}
