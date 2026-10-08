"""Pydantic and forms as a real application writes them: `validate_assignment=True` on a base schema whose
subclasses have field and model validators, `Field(min_items=, max_items=)` (pydantic 1 names, still
accepted), `list[UploadFile] = File(default=[])` next to Form() fields."""
from fastapi import APIRouter, File, Form, UploadFile
from pydantic import BaseModel, ConfigDict, Field, ValidationError, field_validator, model_validator

router = APIRouter(prefix="/pydmore")


class Base(BaseModel):
    model_config = ConfigDict(from_attributes=True, str_strip_whitespace=True, validate_assignment=True)


class DomainIn(Base):
    name: str = Field(..., min_length=3, max_length=40, pattern=r"^[a-zA-Z0-9.-]+$")
    catch_all: str | None = None
    port: int = 25

    @field_validator("name")
    @classmethod
    def lower(cls, v: str) -> str:
        v = v.lower()
        if ".." in v:
            raise ValueError("Domain name cannot contain consecutive dots")
        return v

    @field_validator("catch_all", mode="before")
    @classmethod
    def blank(cls, v):
        if isinstance(v, str) and v.strip() == "":
            return None
        return v

    @field_validator("catch_all")
    @classmethod
    def at(cls, v, info):
        if v is not None and "@" not in v:
            raise ValueError(f"not an address for {info.data.get('name')} ({info.field_name})")
        return v

    @model_validator(mode="after")
    def port_range(self):
        if self.port == 0:
            raise ValueError("port 0")
        return self


class Order(BaseModel):
    domains: list[str] = Field(..., min_items=1, max_items=3)
    servers: list[str] = Field(default=[], max_items=2)
    tags: list[str] = Field(default=[], min_items=1, min_length=0)


def _errs(e: ValidationError) -> list:
    return [{"type": x["type"], "loc": x["loc"], "msg": x["msg"], "input": x["input"]} for x in e.errors()]


@router.post("/domains")
async def domain(body: DomainIn, name: str | None = None, catch_all: str | None = None, port: int | None = None):
    """Assignments after validation: each one validated with the field's validators, then the model's."""
    out = {"in": body.model_dump()}
    steps = []
    for field, value in (("name", name), ("catch_all", catch_all), ("port", port)):
        if value is None:
            continue
        try:
            setattr(body, field, value if field != "port" else int(value))
            steps.append([field, "ok", body.model_dump()])
        except ValidationError as e:
            steps.append([field, e.title, _errs(e), body.model_dump()])
    try:
        body.name = "  Spaced.Example  "
        body.port += 1
        steps.append(["aug", body.name, body.port, sorted(body.model_fields_set)])
    except ValidationError as e:
        steps.append(["aug", _errs(e)])
    out["steps"] = steps
    return out


@router.post("/orders")
async def order(body: Order):
    return body


@router.post("/compose")
async def compose(to: str = Form(...), subject: str = Form(""), files: list[UploadFile] = File(default=[]),
                  extra: list[UploadFile] = File(default=[])):
    names = [f.filename for f in files]
    files.append("x")
    return {"to": to, "subject": subject, "names": names, "sizes": [len(await f.read()) for f in files if f != "x"],
            "extra": len(extra)}


# ---- SQL column names other than the attributes, a deferred blob loaded by undefer()
from sqlalchemy import delete, exists, func, select, update  # noqa: E402
from sqlalchemy.exc import SQLAlchemyError  # noqa: E402
from sqlalchemy.orm import selectinload, undefer  # noqa: E402

from .db import DbDep  # noqa: E402
from .models import Notice, NoticeBlob  # noqa: E402


@router.post("/notices")
async def add_notice(body: dict, db: DbDep):
    key = body.get("hash")
    if key is not None:
        blob = await db.get(NoticeBlob, key)
        if blob is None:
            db.add(NoticeBlob(key=key, content=body.get("content", "").encode()))
            await db.flush()
        else:
            blob.refs += 1
    n = Notice(extra_data=body.get("meta"), blob_key=key)
    if "label" in body:
        n.label = body["label"]
    db.add(n)
    await db.flush()
    return {"id": n.id, "label": n.label, "meta": n.extra_data, "blob": n.blob_key}


@router.get("/notices")
async def notices(db: DbDep, label: str | None = None):
    q = select(Notice).order_by(Notice.id.desc())
    if label is not None:
        q = q.where(Notice.label == label)
    rows = (await db.execute(q)).scalars().all()
    cols = (await db.execute(select(Notice.extra_data, Notice.label, Notice.id).order_by(Notice.id))).all()
    stats = (await db.execute(select(Notice.label, func.count(Notice.id).label("n")).group_by(Notice.label)
                              .order_by(Notice.label))).all()
    return {"rows": [{"id": n.id, "label": n.label, "meta": n.extra_data, "blob": n.blob_key} for n in rows],
            "cols": [dict(r._mapping) for r in cols], "stats": [list(r) for r in stats]}


@router.patch("/notices/{nid}")
async def patch_notice(nid: int, body: dict, db: DbDep):
    res = await db.execute(update(Notice).where(Notice.id == nid).values(extra_data=body).returning(Notice.id, Notice.extra_data))
    row = res.first()
    n = await db.get(Notice, nid)
    if n is not None:
        n.label = "patched"
        await db.flush()
    return {"returned": list(row) if row else None, "label": n.label if n else None}


@router.get("/notices/{nid}/blob")
async def notice_blob(nid: int, db: DbDep, how: str = "nested"):
    if how == "nested":
        q = select(Notice).options(selectinload(Notice.blob).options(undefer(NoticeBlob.content)))
    elif how == "plain":
        q = select(Notice).options(selectinload(Notice.blob))
    else:
        q = select(Notice)
    n = (await db.execute(q.where(Notice.id == nid))).scalar_one_or_none()
    if n is None:
        return {"found": False}
    out = {"found": True, "blob": n.blob_key}
    if how == "top" and n.blob_key is not None:
        blob = (await db.execute(select(NoticeBlob).options(undefer(NoticeBlob.content))
                                 .where(NoticeBlob.key == n.blob_key))).scalar_one()
        again = await db.get(NoticeBlob, n.blob_key, options=[undefer(NoticeBlob.content)])
        out["same"] = blob is again
    else:
        blob = n.blob
    if blob is None:
        return out
    try:
        out["content"] = blob.content.decode() if blob.content is not None else None
    except SQLAlchemyError:  # a lazy load in an async session (StatementError around MissingGreenlet)
        out["content"] = "not loaded"
    out["refs"] = blob.refs
    return out


@router.get("/blobs/{key}")
async def get_blob(key: str, db: DbDep, undeferred: bool = False):
    """`session.get` with and without undefer() on an object already in the identity map (unloaded column)."""
    first = await db.get(NoticeBlob, key)
    if first is None:
        return None
    if undeferred:
        await db.execute(select(NoticeBlob).options(undefer(NoticeBlob.content)).where(NoticeBlob.key == key))
    try:
        return {"content": first.content.decode(), "refs": first.refs}
    except SQLAlchemyError:
        return {"error": "not loaded", "refs": first.refs}


@router.delete("/notices")
async def cleanup(db: DbDep, pattern: str = "^x", flags: str | None = None):
    """an e2e sweep: `regexp_match`, then a correlated `~exists().where(...)` in a DELETE and a SELECT."""
    match = Notice.label.regexp_match(pattern, flags=flags) if flags else Notice.label.regexp_match(pattern)
    res = await db.execute(delete(Notice).where(match, ~exists().where(NoticeBlob.key == Notice.blob_key))
                           .returning(Notice.id))
    gone = sorted(res.scalars().all())
    with_blob = (await db.execute(select(func.count()).select_from(Notice)
                                  .where(exists().where(NoticeBlob.key == Notice.blob_key)))).scalar()
    names = (await db.execute(select(Notice.label).where(~Notice.label.regexp_match("^p")).order_by(Notice.label))).scalars().all()
    return {"gone": gone, "with_blob": with_blob, "names": list(names), "col": [Notice.extra_data.key, Notice.extra_data.name]}


class DomainPatch(Base):
    name: str | None = None
    catch_all: str | None = None


def _updates(name: str | None = None, catch_all: str | None = ...) -> dict:
    """`...` as a "not given" sentinel, apart from an explicit None."""
    updates = {}
    if name is not None:
        updates["name"] = name
    if catch_all is not ...:
        updates["catch_all"] = catch_all
    return updates


@router.patch("/domains")
async def patch_domain(body: DomainPatch):
    kwargs = {}
    if body.catch_all is not None or "catch_all" in (body.model_fields_set or set()):
        kwargs["catch_all"] = body.catch_all
    sentinel = ...
    return {"updates": _updates(name=body.name, **kwargs), "same": sentinel is ..., "eq": sentinel == ...,
            "repr": [repr(...), str(sentinel), bool(...), type(...).__name__]}
