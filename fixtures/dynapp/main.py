import base64
import csv
import hashlib
import hmac
import io
import math
import re
import string
import secrets
import uuid
from calendar import monthrange
from types import SimpleNamespace
from pathlib import Path
import os
from dataclasses import dataclass, field

import sentry_sdk
from email.mime.application import MIMEApplication
from email.mime.multipart import MIMEMultipart
from email.mime.text import MIMEText
from email.utils import formataddr
import aiosmtplib
from jinja2 import Environment, FileSystemLoader, select_autoescape
from itsdangerous import BadPayload, BadSignature, BadTimeSignature, SignatureExpired, URLSafeTimedSerializer
from datetime import UTC, datetime
from fastapi import BackgroundTasks, Depends, FastAPI, File, Form, HTTPException, Query, Response, UploadFile, WebSocket
from fastapi.responses import FileResponse, JSONResponse, PlainTextResponse, RedirectResponse
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer, OAuth2PasswordBearer, OAuth2PasswordRequestForm
from jose import ExpiredSignatureError, JWTError, jwt
from jose.exceptions import JWTClaimsError
from sqlalchemy import String, and_, cast, extract, func, select, text, update
from sqlalchemy.orm.attributes import flag_modified
from sqlalchemy.exc import DataError
from sqlalchemy.orm import selectinload

from . import aio, amqp, apiv, bgloop, colls, composite, ddl, decos, dunders, extras, lazyimp, libs, life, mounted, outbound, pk, prom, rds, retrying, small, sqlmore, tracing, wsock
from .db import DbDep
from .enums import Channel, Level, Priority, Status
from .models import Asset, Owner, Project, Secret, Task
from pydantic import ValidationError

from .schemas import (
    Agenda, Booking, Contact, Span, Invoice, TaskLoose, Trip, Label, Mixed, Point, Point3, ContactV1, OwnerOut, Person, TaskTitle, ProjectIn, ProjectOut, StatusChange, TaskBrief, TaskIn, TaskOut, TaskSummary, Slot,
)

app = FastAPI(lifespan=life.lifespan)
app.include_router(libs.router)
app.include_router(ddl.router)
app.include_router(decos.router)
app.include_router(composite.router)
app.include_router(dunders.router)
app.include_router(aio.router)
app.include_router(life.router)
app.include_router(bgloop.router)
app.include_router(pk.router)
app.include_router(rds.router)
app.include_router(amqp.router)
app.include_router(lazyimp.router)
app.include_router(colls.router)
app.include_router(retrying.router)
app.include_router(sqlmore.router)
app.include_router(extras.router)
app.include_router(prom.router)
app.include_router(outbound.router)
app.include_router(apiv.router, prefix=apiv.settings.API_PREFIX, dependencies=[Depends(apiv.require_user)])
app.include_router(tracing.router)
app.include_router(small.router)
app.include_router(wsock.router)


@app.exception_handler(wsock.WsBoom)
async def ws_boom(websocket, exc):
    """an application exception handler on a WebSocket route: called with the WebSocket"""
    await websocket.close(code=4500, reason=str(exc))


@app.websocket("/ws-app/{room}", name="room")
async def ws_room(websocket: WebSocket, room: str):
    await websocket.accept()
    await websocket.send_text(f"room {room}")
    await websocket.close()

oauth2 = OAuth2PasswordBearer(tokenUrl="/token")
maybe_oauth2 = OAuth2PasswordBearer("/token", auto_error=False)
bearer = HTTPBearer(bearerFormat="JWT")
maybe_bearer = HTTPBearer(auto_error=False)


async def current_user(token: str = Depends(oauth2)) -> dict:
    if token != "good":
        raise HTTPException(status_code=401, detail="bad token", headers={"WWW-Authenticate": "Bearer"})
    return {"name": "ada"}


def describe(p: Priority, c: Channel | None, lvl: Level) -> dict:
    return {
        "str": str(p),
        "fmt": f"{p}|{c}|{lvl}",
        "repr": repr(p),
        "value": p.value,
        "name": p.name,
        "eq_value": p == "high",
        "upper": p.upper(),
        "members": [m.value for m in Priority],
        "lookup": Priority("low").name,
        "by_name": Priority["HIGH"].value,
        "in": "low" in [Priority.LOW],
        "level": lvl.value + 1,
        "plain_eq": Status.OPEN == "open",
    }


@app.post("/tasks", response_model=TaskOut, status_code=201)
async def create_task(payload: TaskIn, db: DbDep):
    task = Task(title=payload.title, priority=payload.priority, tags=payload.tags, price=payload.price)
    db.add(task)
    await db.commit()
    await db.refresh(task)
    return task


@app.get("/tasks", response_model=list[TaskOut])
async def list_tasks(db: DbDep, priority: Priority | None = None, status: Status | None = Query(default=None)):
    q = select(Task).order_by(Task.id)
    if priority is not None:
        q = q.where(Task.priority == priority)
    if status is not None:
        q = q.where(Task.status == status)
    return (await db.execute(q)).scalars().all()


@app.get("/tasks/{task_id}", response_model=TaskOut)
async def get_task(task_id: int, db: DbDep):
    task = await db.get(Task, task_id)
    if task is None:
        raise HTTPException(status_code=404, detail="Task not found")
    return task


@app.post("/tasks/{task_id}/status", response_model=TaskOut)
async def change_status(task_id: int, change: StatusChange, db: DbDep):
    task = await db.get(Task, task_id)
    if task is None:
        raise HTTPException(status_code=404, detail="Task not found")
    if task.status == change.status:
        raise HTTPException(status_code=409, detail=f"already {task.status.value}")
    task.status = change.status
    await db.commit()
    await db.refresh(task)
    return task


@app.post("/tasks/{task_id}/lower", response_model=TaskOut)
async def lower_priority(task_id: int, db: DbDep):
    """Bulk `update()`: the columns' onupdate= apply too."""
    await db.execute(update(Task).where(Task.id == task_id).values(priority=Priority.LOW))
    await db.commit()
    task = await db.get(Task, task_id)
    if task is None:
        raise HTTPException(status_code=404, detail="Task not found")
    return task


@app.post("/describe")
async def describe_task(payload: TaskIn):
    return describe(payload.priority, payload.channel, payload.level)


@app.post("/summary", response_model=TaskSummary)
async def summary(payload: TaskIn):
    return TaskSummary(title=payload.title, priority=payload.priority)


@app.get("/stats")
async def stats(db: DbDep):
    rows = (await db.execute(select(Task.priority, Task.status))).all()
    counts: dict[str, int] = {}
    for p, s in rows:
        key = f"{p.value}/{s.value}"
        counts[key] = counts.get(key, 0) + 1
    return {"counts": counts, "total": len(rows), "touched": (await db.execute(select(Task.touched_at))).scalars().first() is not None}


@app.post("/contacts")
async def contact(payload: Contact):
    payload.name = payload.name + "  (vérifié)  "
    return {"name": payload.name, "email": payload.email, "v1": ContactV1(name=f"  {payload.name}  ").name}


@app.post("/contacts/bad-assign")
async def contact_bad_assign(payload: Contact):
    payload.email = "pas-un-email"
    return payload


# ---------------------------------------------------------------- relationships


async def load_project(db, project_id: int) -> Project:
    q = select(Project).where(Project.id == project_id).options(selectinload(Project.tasks))
    project = (await db.execute(q)).scalar_one_or_none()
    if project is None:
        raise HTTPException(status_code=404, detail="Project not found")
    return project


@app.post("/projects", response_model=ProjectOut, status_code=201)
async def create_project(payload: ProjectIn, db: DbDep):
    """save-update cascade: the owner and the tasks are inserted through the relationships."""
    project = Project(name=payload.name, tasks=[Task(title=t) for t in payload.tasks])
    if payload.owner:
        project.owner = Owner(name=payload.owner)
    db.add(project)
    await db.commit()
    return await load_project(db, project.id)


@app.get("/projects/{project_id}", response_model=ProjectOut)
async def get_project(project_id: int, db: DbDep):
    return await load_project(db, project_id)


@app.get("/projects", response_model=list[ProjectOut])
async def list_projects(db: DbDep):
    q = select(Project).order_by(Project.id).options(selectinload(Project.tasks))
    return (await db.execute(q)).scalars().all()


@app.get("/projects/{project_id}/lazy", response_model=ProjectOut)
async def get_project_lazy(project_id: int, db: DbDep):
    """tasks not loaded: the async lazy load fails (MissingGreenlet -> 500) on both servers."""
    return await db.get(Project, project_id)


@app.get("/owners/{owner_id}", response_model=OwnerOut)
async def get_owner(owner_id: int, db: DbDep):
    owner = await db.get(Owner, owner_id)
    if owner is None:
        raise HTTPException(status_code=404, detail="Owner not found")
    return {"id": owner.id, "name": owner.name, "projects": [p.name for p in owner.projects]}


@app.post("/owners-cls/named")
async def owner_named(payload: dict, db: DbDep):
    """@staticmethod / @classmethod of a mapped class, read on the class, on `cls` and on an instance."""
    owner = Owner.named(payload["name"], shout=payload.get("shout", False))
    db.add(owner)
    await db.flush()
    return {"id": owner.id, "name": owner.name, "describe": owner.describe(), "label": Owner.label(),
            "via_instance": owner.slug(" A  B "), "named": owner.named("x y").name}


@app.get("/owners-cls/slug")
async def owner_slug(q: str, bad: bool = False):
    if bad:
        try:
            Owner.slug(q, q)
        except TypeError as e:
            return {"err": str(e)}
    return {"slug": Owner.slug(q)}


@app.get("/tasks/{task_id}/project")
async def task_project(task_id: int, db: DbDep, preload: str = ""):
    """many-to-one through the identity map: no IO when the project is loaded and still referenced
    (the identity map is weak: a discarded result is gone, the lazy load then needs IO -> 500)."""
    task = await db.get(Task, task_id)
    if task is None:
        raise HTTPException(status_code=404, detail="Task not found")
    kept = None
    if preload == "keep" and task.project_id is not None:
        kept = await db.get(Project, task.project_id)
    elif preload == "discard" and task.project_id is not None:
        await db.get(Project, task.project_id)
    return {"project": task.project.name if task.project else None, "kept": kept is not None}


@app.post("/tasks/{task_id}/move/{project_id}", response_model=TaskBrief)
async def move_task(task_id: int, project_id: int, db: DbDep):
    task = await db.get(Task, task_id)
    project = await db.get(Project, project_id)
    if task is None or project is None:
        raise HTTPException(status_code=404, detail="not found")
    task.project = project
    before = task.project_id
    await db.commit()
    return {"id": task.id, "title": f"{task.title} (was {before})", "project_id": task.project_id}


@app.post("/projects/{project_id}/drop-first", response_model=ProjectOut)
async def drop_first(project_id: int, db: DbDep):
    """delete-orphan: a task removed from the collection is deleted."""
    project = await load_project(db, project_id)
    if project.tasks:
        project.tasks.remove(project.tasks[0])
    await db.commit()
    return await load_project(db, project_id)


@app.delete("/projects/{project_id}", status_code=204)
async def delete_project(project_id: int, db: DbDep):
    """cascade delete: the project's tasks (loaded by the cascade) are deleted first."""
    project = await db.get(Project, project_id)
    if project is None:
        raise HTTPException(status_code=404, detail="Project not found")
    await db.delete(project)
    await db.commit()


@app.get("/project-sizes")
async def project_sizes(db: DbDep):
    q = select(Project.name, func.count(Task.id)).join(Project.tasks).group_by(Project.name).order_by(Project.name)
    return [{"name": n, "tasks": c} for n, c in (await db.execute(q)).all()]


@app.get("/prices")
async def prices(db: DbDep, above: float = 0):
    """Numeric(asdecimal=False): NUMERIC in the database, float in Python, sums typed by the column."""
    total, top = (await db.execute(select(func.sum(Task.price), func.max(Task.price)))).one()
    rows = (await db.execute(select(Task.title, Task.price).where(Task.price > above).order_by(Task.price))).all()
    return {"total": total, "max": top, "above": [[t, p] for t, p in rows]}


# ---------------------------------------------------------------- session dependency


@app.post("/drafts", status_code=201)
async def create_draft(db: DbDep, fail: bool = False):
    """No commit here: get_db commits after the endpoint, or rolls back if it raises."""
    task = Task(title="draft")
    db.add(task)
    await db.flush()
    if fail:
        raise HTTPException(status_code=409, detail=f"draft {task.id} rejected")
    return {"id": task.id}


@app.get("/drafts/count")
async def count_drafts(db: DbDep, pending: bool = False):
    """autoflush=False: a pending object is not flushed by the query, so it is not counted."""
    if pending:
        db.add(Task(title="draft"))
    n = (await db.execute(select(func.count(Task.id)).where(Task.title == "draft"))).scalar_one()
    if pending:
        await db.rollback()
    return {"drafts": n}


# ---------------------------------------------------------------- SQL expression options


@app.get("/sql-mix")
async def sql_mix(db: DbDep):
    outer = (await db.execute(
        select(Project.name, Owner.name).join(Owner, Project.owner_id == Owner.id, isouter=True).order_by(Project.id)
    )).all()
    jf = (await db.execute(
        select(Task.title).join_from(Project, Task, Project.id == Task.project_id).order_by(Task.id)
    )).scalars().all()
    first_per_project = (await db.execute(
        select(Task.project_id, Task.title).where(Task.project_id.is_not(None))
        .distinct(Task.project_id).order_by(Task.project_id, Task.id.desc())
    )).all()
    locked = (await db.execute(select(Task.id).order_by(Task.id).limit(1).with_for_update(skip_locked=True))).scalars().all()
    by_task = (await db.execute(select(Project.name).join(Project.tasks).filter_by(title="a2"))).scalars().all()
    uniq = (await db.execute(select(Project).join(Project.tasks).order_by(Project.id))).scalars().unique().all()
    return {
        "outer": [list(r) for r in outer], "join_from": jf, "distinct_on": [list(r) for r in first_per_project],
        "locked": len(locked), "filter_by": by_task, "unique": [p.name for p in uniq],
    }


@app.post("/tasks/{task_id}/retitle")
async def retitle(task_id: int, db: DbDep, sync: bool = True):
    task = await db.get(Task, task_id)
    if task is None:
        raise HTTPException(status_code=404, detail="Task not found")
    stmt = update(Task).where(Task.id == task_id).values({Task.title: "renamed", "revision": "bulk"})
    if not sync:
        stmt = stmt.execution_options(synchronize_session=False)
    await db.execute(stmt)
    return {"title": task.title, "revision": task.revision}


@app.get("/projects/{project_id}/via-get", response_model=ProjectOut)
async def project_via_get(project_id: int, db: DbDep, how: str = "options"):
    """session.get(options=[...]) and session.refresh(obj, ["tasks"]) load the collection."""
    if how == "options":
        project = await db.get(Project, project_id, options=[selectinload(Project.tasks)])
    else:
        project = await db.get(Project, project_id)
        if project is not None:
            project.name = project.name + " (local)"
            await db.refresh(project, ["tasks"])
    if project is None:
        raise HTTPException(status_code=404, detail="Project not found")
    return project


# ---------------------------------------------------------------- routing (declaration order, as Starlette)


@app.get("/items/{name}")
async def item_by_name(name: str):
    return {"route": "param", "name": name}


@app.get("/items/special")
async def item_special():
    """Never reached with GET: /items/{name} is declared first."""
    return {"route": "static"}


@app.post("/items/special")
async def item_special_post():
    return {"route": "static-post"}


# ---------------------------------------------------------------- Pydantic methods


@app.post("/people")
async def people(person: Person):
    """populate_by_name, model_dump_json options, model_copy (update= not validated, deep=True)."""
    shallow = person.model_copy(update={"full_name": "x"})
    deep = person.model_copy(deep=True)
    person.tags.append("added")
    return {
        "dump": person.model_dump(),
        "json": person.model_dump_json(exclude_none=True, by_alias=True),
        "shallow": [shallow.full_name, shallow.tags],
        "deep": deep.tags,
    }


@app.get("/tasks/{task_id}/as-title")
async def task_as_title(task_id: int, db: DbDep, attrs: bool = False):
    """model_validate of an ORM object: needs from_attributes (else a ValidationError -> 500)."""
    task = await db.get(Task, task_id)
    if task is None:
        raise HTTPException(status_code=404, detail="Task not found")
    out = TaskTitle.model_validate(task, from_attributes=True) if attrs else TaskTitle.model_validate(task)
    return out.model_dump()


@app.post("/sum")
async def sum_values(values: list[int], scale: int = 1):
    """No Query(): a list parameter is the request body (FastAPI's complex-annotation rule)."""
    return {"total": sum(values) * scale}


@app.post("/tasks/{task_id}/price")
async def set_price(task_id: int, value: float, db: DbDep):
    """A NUMERIC overflow is a DataError (SQLSTATE class 22), catchable as such."""
    task = await db.get(Task, task_id)
    if task is None:
        raise HTTPException(status_code=404, detail="Task not found")
    task.price = value
    try:
        await db.commit()
    except DataError:
        await db.rollback()
        raise HTTPException(status_code=400, detail="price out of range")
    return {"price": task.price}


@app.get("/lazy")
async def lazy(n: int = 4):
    """any/all/next over a generator stop at the first decisive item (side effects show it)."""
    calls = []
    probe = lambda x: (calls.append(x), x > 1)[1]  # noqa: E731
    found = any(probe(x) for x in range(n))
    every = all(probe(x) for x in range(n, 0, -1))
    first = next((x * 10 for x in range(n) if x > 0), None)
    empty = next((x for x in []), "default")
    try:
        assert n < 3, f"n={n} too big"
        checked = "ok"
    except AssertionError as e:
        checked = f"AssertionError: {e}"
    return {"found": found, "every": every, "calls": calls, "first": first, "empty": empty,
            "checked": checked, "head": next(iter([7, 8]))}


@app.post("/agenda")
async def agenda(agenda: Agenda):
    """Field validators taking `info` / v1 `values`."""
    return agenda


@app.post("/agenda/errors")
async def agenda_errors(payload: dict):
    """ValidationError.errors() of a model built in the endpoint: ctx holds the exception object."""
    try:
        Span(**payload)
    except ValidationError as e:
        errs = e.errors()
        return {"n": e.error_count(), "url": [d.get("url") for d in errs], "repr": [repr(d.get("ctx")) for d in errs],
                "types": [type(d["ctx"]["error"]).__name__ for d in errs if "error" in d.get("ctx", {})],
                "short": e.errors(include_url=False, include_context=False)}
    return {"ok": True}


@app.post("/bookings")
async def book(booking: Booking, n: int = Query(ge=0)):
    """Validators run in field order, nested ones first; their errors interleave with type errors."""
    slots = [{"label": s.label, "room": s.room, "seats": s.seats, "name": s.name} for s in booking.slots]
    return {"n": n, "slots": slots, "total": booking.total, "stamped": all(s.stamp is not None for s in booking.slots)}


@app.post("/slots/check")
async def check_slot(data: dict):
    """Model(**data) runs the validators; their ValidationError is caught like any other."""
    try:
        s = Slot(**data)
    except ValidationError:
        return {"valid": False}
    return {"label": s.label, "room": s.room, "seats": s.seats}


@app.get("/slots/{label}", response_model=Slot)
async def show_slot(label: str):
    """The response model's validators run too (a ValueError there is a 500)."""
    return {"start": 1, "label": label, "room": "B"}


@app.post("/mixed")
async def mixed(m: Mixed):
    """Which member each union picked shows in the JSON (1 vs "1" vs 1.0 vs true) and in the flags."""
    return {"m": m, "g_level": isinstance(m.g, Level), "g_prio": isinstance(m.g, Priority),
            "d_point": isinstance(m.d, Point), "d_point3": isinstance(m.d, Point3), "d_label": isinstance(m.d, Label)}


@app.get("/auth/me")
async def me(user: dict = Depends(current_user), n: int = 1):
    """OAuth2PasswordBearer in a dependency: 401 before the query parameter's 422."""
    return {"user": user, "n": n}


@app.get("/auth/token")
async def raw_token(token: str = Depends(oauth2), maybe: str | None = Depends(maybe_oauth2)):
    return {"token": token, "maybe": maybe}


@app.get("/auth/optional")
async def optional_token(token: str | None = Depends(maybe_oauth2)):
    return {"token": token}


@app.get("/auth/creds")
async def creds(c: HTTPAuthorizationCredentials = Depends(bearer)):
    return {"scheme": c.scheme, "credentials": c.credentials, "dump": c.model_dump(), "c": c}


@app.get("/auth/maybe-creds")
async def maybe_creds(c: HTTPAuthorizationCredentials | None = Depends(maybe_bearer)):
    return {"c": c}


JWT_SECRET = os.getenv("DYNAPP_JWT_SECRET", "s3cret")
TTL = int(os.environ.get("DYNAPP_TTL") or "30")


@app.post("/jwt/issue")
async def jwt_issue(body: dict, alg: str = "HS256"):
    """jwt.encode rewrites exp/iat/nbf datetimes of the caller's dict into timestamps (in place)."""
    claims = dict(body)
    claims["exp"] = datetime(2100, 1, 1, 12, 30, 15, 999)
    claims["iat"] = datetime(2020, 6, 1, 8, 0, tzinfo=UTC)
    token = jwt.encode(claims, JWT_SECRET, algorithm=alg)
    return {"token": token, "claims": claims}


@app.get("/jwt/check")
async def jwt_check(token: str, algs: list[str] = Query(default=["HS256"]), key: str = JWT_SECRET):
    try:
        claims = jwt.decode(token, key, algorithms=algs)
    except ExpiredSignatureError as e:
        return {"error": "expired", "msg": str(e)}
    except JWTClaimsError as e:
        return {"error": "claims", "msg": str(e)}
    except JWTError as e:
        return {"error": "jwt", "msg": str(e)}
    return {"claims": claims}


@app.get("/jwt/me")
async def jwt_me(token: str = Depends(oauth2)):
    try:
        payload = jwt.decode(token, JWT_SECRET, algorithms=["HS256"])
    except JWTError:
        raise HTTPException(status_code=401, detail="Could not validate credentials", headers={"WWW-Authenticate": "Bearer"})
    return {"sub": payload.get("sub")}


@app.get("/env")
async def env(name: str = "DYNAPP_PROBE"):
    """Environment read at request time (both servers get the same environment)."""
    present = name in os.environ
    try:
        direct = os.environ[name]
    except KeyError as e:
        direct = f"KeyError {e}"
    return {"present": present, "direct": direct, "get": os.getenv(name), "dflt": os.environ.get(name, "-"),
            "ttl": TTL, "secret_len": len(JWT_SECRET)}


_hits = 0
_registry: dict | None = None


def bump(n: int = 1) -> int:
    global _hits
    _hits += n
    return _hits


def registry() -> dict:
    """Lazy singleton (a session serializer built on first use)."""
    global _registry
    if _registry is None:
        _registry = {"made": bump(100)}
    return _registry


@app.post("/globals")
async def globals_route():
    first = bump()
    reg = registry()
    again = registry() is reg
    return {"first": first, "now": _hits, "reg": reg, "same": again}


@app.get("/sentry")
async def sentry_calls(user_id: int = 1):
    """Without sentry_sdk.init (no DSN), every call is a no-op returning None: the binary does the same."""
    sentry_sdk.set_tag("route", "sentry")
    sentry_sdk.set_user({"id": user_id})
    sentry_sdk.add_breadcrumb(category="test", message=f"user {user_id}")
    event = sentry_sdk.capture_message("hello", level="info")
    try:
        1 / 0
    except ZeroDivisionError as e:
        exc_event = sentry_sdk.capture_exception(e)
    return {"event": event, "exc_event": exc_event}


ROLE_PERMS = {"viewer": {"read"}, "admin": {"*"}}


@dataclass
class Ctx:
    """A plain dataclass: no validation, methods, defaults."""

    owner_id: int
    role: str | None
    tags: list = field(default_factory=list)
    note: str = "-"

    def can(self, perm: str) -> bool:
        if self.role is None:
            return True
        perms = ROLE_PERMS.get(self.role, set())
        return "*" in perms or perm in perms

    @property
    def label(self) -> str:
        return f"{self.owner_id}:{self.role}"


@app.get("/dataclass")
async def dataclass_route(role: str | None = None, perm: str = "write"):
    a = Ctx(1, role)
    b = Ctx(owner_id=1, role=role, tags=[])
    a.tags.append("x")
    c = Ctx(2, "admin", note="n")
    c.note = "changed"
    errors = []
    for args, kwargs in (((), {}), ((1,), {}), ((1, 2, 3, 4, 5), {}), ((1, None), {"owner_id": 3}), ((1, None), {"xyz123": 1})):
        try:
            Ctx(*args, **kwargs)
        except TypeError as e:
            errors.append(str(e))
    return {"can": a.can(perm), "label": a.label, "repr": repr(c), "eq": a == b, "eq2": b == Ctx(1, role),
            "is": isinstance(a, Ctx), "a": a, "c": c, "errors": errors}


@app.post("/its/issue")
async def its_issue(body: dict, salt: str | None = None):
    """The payload part of a token is deterministic (the timestamp and signature are not)."""
    s = URLSafeTimedSerializer(JWT_SECRET, salt=salt) if salt else URLSafeTimedSerializer(JWT_SECRET)
    token = s.dumps(body)
    return {"payload": token.split(".")[0] if not token.startswith(".") else token.split(".")[1], "compressed": token.startswith("."),
            "back": s.loads(token, max_age=60), "parts": len(token.split("."))}


@app.get("/its/check")
async def its_check(token: str, max_age: int | None = None, salt: str = "itsdangerous", old_key: bool = False):
    s = URLSafeTimedSerializer(["old-key", JWT_SECRET] if old_key else JWT_SECRET, salt=salt)
    try:
        return {"data": s.loads(token, max_age=max_age)}
    except SignatureExpired as e:
        msg = str(e)
        return {"error": "expired", "tail": msg.split(" > ")[-1] if " > " in msg else msg.split(" < ")[-1]}
    except BadTimeSignature as e:
        return {"error": "time", "msg": str(e)}
    except BadSignature as e:
        return {"error": "signature", "msg": str(e)}
    except BadPayload as e:
        return {"error": "payload", "msg": str(e)}


@app.post("/secrets")
async def add_secret(body: dict, db: DbDep):
    """TypeDecorator: encrypted on the way in (process_bind_param), decrypted on the way out."""
    sec = Secret(label=body["label"], token=body.get("token"))
    db.add(sec)
    await db.commit()
    return {"id": sec.id, "token": sec.token}


@app.get("/secrets/{secret_id}")
async def get_secret(secret_id: int, db: DbDep):
    sec = await db.get(Secret, secret_id)
    if sec is None:
        raise HTTPException(status_code=404, detail="no secret")
    tokens = (await db.execute(select(Secret.token).where(Secret.id == secret_id))).scalars().all()
    same = (await db.execute(select(Secret).where(Secret.token == sec.token))).scalars().all()
    return {"label": sec.label, "token": sec.token, "column": tokens, "match_by_value": len(same)}


@app.put("/secrets/{secret_id}")
async def set_secret(secret_id: int, body: dict, db: DbDep):
    sec = await db.get(Secret, secret_id)
    sec.token = body.get("token")
    await db.commit()
    await db.refresh(sec)
    return {"token": sec.token}


@app.post("/assets")
async def add_asset(body: dict, db: DbDep):
    asset = Asset(**({"channel": Channel(body["channel"])} if "channel" in body else {}))
    if "b64" in body:
        asset.data = base64.b64decode(body["b64"])
    if "text" in body:
        asset.thumb = body["text"].encode()
    db.add(asset)
    await db.commit()
    return {"id": asset.id, "channel": asset.channel, "name": asset.channel.name, "size": len(asset.data or b"")}


@app.get("/assets/{asset_id}/data")
async def asset_data(asset_id: int, db: DbDep):
    """LargeBinary: bytea read back as bytes, served raw."""
    asset = await db.get(Asset, asset_id)
    if asset is None or not asset.data:
        raise HTTPException(status_code=404, detail="no data")
    return Response(content=bytes(asset.data), media_type="application/octet-stream")


@app.get("/assets/{asset_id}/meta")
async def asset_meta(asset_id: int, db: DbDep):
    asset = await db.get(Asset, asset_id)
    same = (await db.execute(select(Asset.id).where(Asset.data == asset.data))).scalars().all()
    thumbs = (await db.execute(select(Asset.thumb).where(Asset.thumb.is_not(None)).order_by(Asset.id))).scalars().all()
    return {"data": asset.data, "thumb": asset.thumb, "is_bytes": isinstance(asset.data, bytes),
            "head": base64.b64encode((asset.data or b"")[:4]).decode(), "same": same, "thumbs": thumbs}


@app.get("/assets")
async def list_assets(db: DbDep, channel: Channel | None = None):
    q = select(Asset).order_by(Asset.id)
    if channel is not None:
        q = q.where(Asset.channel == channel)
    rows = (await db.execute(q)).scalars().all()
    raw = (await db.execute(text("SELECT channel::text FROM assets ORDER BY id"))).scalars().all()
    return {"ids": [a.id for a in rows], "channels": [a.channel for a in rows], "stored": raw}


EVENTS: list = []


async def tracked(tag: str = "t"):
    """try/finally around the yield: the cleanup runs on success and on error."""
    EVENTS.append(f"open {tag}")
    try:
        yield {"tag": tag}
    finally:
        EVENTS.append(f"close {tag}")


async def counted(x: dict = Depends(tracked)):
    """Code after a bare yield: skipped when the endpoint fails (the exception is raised at the yield)."""
    EVENTS.append("count in")
    yield len(x)
    EVENTS.append("count out")


@app.get("/gendep")
async def gendep(x: dict = Depends(tracked), n: int = Depends(counted), fail: bool = False):
    EVENTS.append("endpoint")
    if fail:
        raise HTTPException(status_code=409, detail="failed")
    return {"x": x, "n": n}


@app.get("/gendep/events")
async def gendep_events():
    out = list(EVENTS)
    EVENTS.clear()
    return out


class Sink:
    """A plain class (an audit sink, an API client...)."""

    PREFIX = "sink"

    def __init__(self, owner: int, *, tag: str = "t"):
        self.owner = owner
        self.items = []
        self.tag = tag

    def add(self, x, *, times: int = 1) -> int:
        for _ in range(times):
            self.items.append(x)
        return len(self.items)

    @property
    def label(self) -> str:
        return f"{self.PREFIX}:{self.owner}:{self.tag}"

    @staticmethod
    def make(owner: int) -> "Sink":
        return Sink(owner, tag="made")


class AppError(Exception):
    def __init__(self, status_code: int, detail: str):
        self.status_code = status_code
        self.detail = detail
        super().__init__(detail)


class Locked(Exception):
    def __init__(self, until: str):
        self.until = until


class SubError(AppError):
    pass


@app.get("/classes")
async def classes():
    s = Sink(3, tag="x")
    s.add("a")
    n = s.add("b", times=2)
    m = Sink.make(4)
    m.extra = "set later"
    errors = []
    for call in (lambda: Sink(), lambda: s.add(), lambda: s.add("a", nope=1), lambda: Sink(1, 2)):
        try:
            call()
        except TypeError as e:
            errors.append(str(e))
    out = {"n": n, "items": s.items, "label": s.label, "made": m.label, "extra": m.extra, "is": isinstance(m, Sink),
           "prefix": s.PREFIX, "errors": errors}
    for exc in (AppError(418, "teapot"), Locked("tomorrow"), SubError(400, "bad")):
        try:
            raise exc
        except AppError as e:
            out[e.detail] = [e.status_code, str(e), list(e.args), isinstance(e, SubError)]
        except Locked as e:
            out["locked"] = [e.until, str(e), list(e.args)]
    return out


@app.post("/trips")
async def trips(trip: Trip):
    return trip


@app.post("/invoices")
async def invoices(inv: Invoice):
    return inv


@app.get("/tasks/{task_id}/loose", response_model=TaskLoose)
async def task_loose(task_id: int, db: DbDep):
    return await db.get(Task, task_id)


@app.post("/upload")
async def upload(file: UploadFile = File(...), caption: str | None = Form(None), order: int = Form(0),
                 primary: bool = Form(False)):
    data = await file.read()
    await file.seek(0)
    again = await file.read(3)
    buf = io.BytesIO()
    buf.write(data)
    buf.seek(0)
    return {"filename": file.filename, "type": file.content_type, "size": file.size, "len": len(data),
            "head": again.decode(), "caption": caption, "order": order, "primary": primary,
            "copy": buf.getvalue() == data, "tell": buf.tell()}


@app.post("/uploads")
async def uploads(files: list[UploadFile] = File(...), note: UploadFile | None = File(None)):
    return {"names": [f.filename for f in files], "sizes": [len(await f.read()) for f in files],
            "note": note.filename if note else None}


STORE = Path("storage-test") / "docs"


@app.post("/files/{name}")
async def files(name: str, body: dict):
    """pathlib + open() in a `with` block + uuid4 (its value is random: only its shape is returned)."""
    STORE.mkdir(parents=True, exist_ok=True)
    p = STORE / name
    with open(p, "wb") as f:
        n = f.write(body["text"].encode())
    with open(p) as f:
        text = f.read()
    u = uuid.uuid4()
    info = {"n": n, "text": text, "str": str(p), "repr": repr(p), "name": p.name, "suffix": p.suffix, "stem": p.stem,
            "parent": str(p.parent), "parts": list(p.parts), "exists": p.exists(), "is_file": p.is_file(),
            "rel": str(p.relative_to(Path("storage-test"))), "with": str(p.with_suffix(".bak")),
            "eq": p == STORE / name, "uuid": [len(u.hex), len(str(u)), str(u)[14]], "join": str(Path("/a") / "b" / ".." / "c")}
    errors = []
    try:
        p.relative_to("elsewhere")
    except ValueError as e:
        errors.append(str(e))
    try:
        STORE.mkdir()
    except FileExistsError as e:
        errors.append(str(e))
    p.unlink()
    try:
        p.unlink()
    except FileNotFoundError as e:
        errors.append(str(e))
    try:
        with open(STORE / "missing.txt") as f:
            f.read()
    except OSError as e:
        errors.append(str(e))
    info["errors"] = errors
    info["gone"] = p.exists()
    return info


KINDS_A = ("gaz", "fioul")
KINDS = KINDS_A + ("bois",)
KIND_PATTERN = "^(" + "|".join(KINDS) + ")$"


@app.get("/misc")
async def misc(kind: str = Query("gaz", pattern=KIND_PATTERN), n: int = 3):
    for i in range(n):
        if i == 5:
            found = "five"
            break
    else:
        found = "none"
    k = 0
    while k < n:
        k += 1
        if k == 10:
            break
    else:
        k = -k
    ns = SimpleNamespace(owner_id=n, user=None)
    ns.extra = [1]
    return {"kind": kind, "found": found, "k": k, "month": monthrange(2024, 2), "dec": monthrange(2023, 12),
            "ns": [ns.owner_id, ns.user, ns.extra, repr(ns)], "keys": list(dict.fromkeys(["b", "a", "b", "c"])),
            "joined": " et ".join(dict.fromkeys(x for x in ["x", "y", "x"])), "first": list(iter([n, n + 1]))}


@app.get("/resp/cookies")
async def resp_cookies(response: Response):
    response.set_cookie(key="s", value="abc.def-_", max_age=3600, httponly=True, secure=False, samesite="lax", path="/")
    response.set_cookie("q", "a b;c\"é")
    response.delete_cookie(key="old", path="/", httponly=True, secure=True, samesite="strict")
    response.status_code = 201
    return {"ok": 1}


@app.get("/resp/redirect")
async def resp_redirect():
    r = RedirectResponse(url="/x?a=é b", status_code=302)
    r.set_cookie("t", "1", max_age=10)
    return r


@app.get("/resp/plain")
async def resp_plain():
    return PlainTextResponse("héllo", headers={"X-A": "1"})


@app.get("/resp/json")
async def resp_json(response: Response):
    response.set_cookie("ignored", "1")  # not merged into a returned Response
    return JSONResponse(status_code=418, content={"é": [1, None, True]})


@app.get("/resp/empty", status_code=204)
async def resp_empty():
    return Response(status_code=204)


@app.get("/resp/file", response_class=FileResponse)
async def resp_file(name: str | None = None):
    return FileResponse("storage-test/fixed.pdf", filename=name)


def record(event: str, suffix: str = "") -> None:
    EVENTS.append(event + suffix)


@app.post("/resp/background")
async def resp_background(tasks: BackgroundTasks):
    tasks.add_task(record, "background", "!")
    EVENTS.append("endpoint")
    return {"queued": True}


TEMPLATES = Path(__file__).parent / "templates" / "email"
jinja = Environment(loader=FileSystemLoader(str(TEMPLATES)), autoescape=select_autoescape(["html", "xml"]))


@app.get("/mail/render")
async def mail_render(name: str = "Ada <&'\">", db: DbDep = None):
    task = await db.get(Task, 2)
    html = jinja.get_template("hello.html").render(
        frontend_url="https://x.example", name=name, amount=12.5, active=True, nothing=None,
        items=[{"label": "a<b", "value": 1}, {"label": "c", "value": 2.0}], task=task)
    return {"html": html}


@app.get("/mail/build")
async def mail_build():
    msg = MIMEMultipart("mixed")
    msg["From"] = formataddr(("Équipe Easy", "noreply@example.com"))
    msg["To"] = "ada@example.com"
    msg["Subject"] = "Votre reçu"
    alt = MIMEMultipart("alternative")
    alt.attach(MIMEText("plain text", "plain"))
    alt.attach(MIMEText("<p>héllo</p>", "html", "utf-8"))
    msg.attach(alt)
    att = MIMEApplication(b"%PDF-1.4 data", _subtype="pdf")
    att.add_header("Content-Disposition", "attachment", filename="reçu.pdf")
    msg.attach(att)
    text = msg.as_string()
    return {"text": text, "from": msg["From"], "plain": formataddr(("Ada", "a@b.c")), "quoted": formataddr(("Ada, L.", "a@b.c"))}


@app.post("/mail/send")
async def mail_send(to: str = "ada@example.com"):
    msg = MIMEMultipart("alternative")
    msg["From"] = formataddr(("Équipe", "noreply@example.com"))
    msg["To"] = to
    msg["Subject"] = "Test d'envoi"
    msg.attach(MIMEText("<p>héllo</p>", "html", "utf-8"))
    try:
        await aiosmtplib.send(msg, hostname="127.0.0.1", port=int(os.getenv("DYNAPP_SMTP_PORT", "8025")),
                              use_tls=False, start_tls=False, timeout=5)
        return {"sent": True}
    except Exception as e:
        return {"sent": False}


LOC_RE = re.compile(r"<loc>\s*([^<\s]+)\s*</loc>", re.IGNORECASE)
DATE_RE = re.compile(r"(?P<d>\d{2})/(?P<m>\d{2})/(?P<y>\d{4})")


@app.post("/stdlib")
async def stdlib_route(body: dict):
    text = body["text"]
    m = DATE_RE.search(text)
    out = {
        "locs": LOC_RE.findall(text),
        "date": [m.group(0), m.group("y"), m.groupdict(), m.start(), m.end(), m.span("m")] if m else None,
        "iso": DATE_RE.sub(r"\g<y>-\g<m>-\2".replace("\\2", r"\g<d>"), text),
        "upper": re.sub(r"[aeiou]", lambda mm: mm.group(0).upper(), "hello world", count=2),
        "split": re.split(r"[,;]\s*", "a, b;c,,d", maxsplit=3),
        "match": bool(re.match(r"he", "hello")), "nomatch": re.match(r"lo", "hello") is None,
        "full": bool(re.fullmatch(r"\w+", "abc")), "dollar": bool(re.search(r"c$", "abc\n")),
        "escape": re.escape("a.b*c"), "iter": [mm.group(1) for mm in re.finditer(r"(\d)", "a1b2c3")],
        "ci": bool(re.search("HELLO", "say hello", re.IGNORECASE)),
    }
    buf = io.StringIO()
    w = csv.writer(buf)
    w.writerow(["id", "name", "note"])
    w.writerow([1, "Ada, L.", 'say "hi"'])
    w.writerow([2.5, None, "multi\nline"])
    dw = csv.DictWriter(buf, fieldnames=["a", "b"], extrasaction="ignore")
    dw.writeheader()
    dw.writerows([{"a": 1, "b": 2, "c": 3}, {"a": "x"}])
    out["csv"] = buf.getvalue()
    rows = list(csv.reader(io.StringIO(buf.getvalue())))
    out["rows"] = rows[:3]
    sample = "nom;prix\npomme;1\npoire;2\n"
    dialect = csv.Sniffer().sniff(sample, delimiters=",;")
    reader = csv.DictReader(io.StringIO(sample), delimiter=dialect.delimiter)
    out["dict"] = [dict(r) for r in reader]
    out["fields"] = reader.fieldnames
    out["math"] = [math.ceil(2.1), math.floor(-2.1), math.pow(2, 10), math.sqrt(16), math.isclose(0.1 + 0.2, 0.3)]
    out["hmac"] = hmac.new(b"key", b"msg", hashlib.sha256).hexdigest()
    out["same"] = hmac.compare_digest("abc", "abc")
    out["chars"] = [string.ascii_uppercase[:3], string.digits[-2:]]
    out["path"] = [os.path.join("a", "b", "c.txt"), os.path.basename("/x/y.pdf"), os.path.splitext("r.tar.gz"),
                   os.path.dirname("/x/y/z"), os.path.exists("/definitely/missing")]
    return out


@app.get("/sqlx")
async def sql_features(db: DbDep, prio: str | None = None):
    base = select(Task).where(Task.priority == Priority.HIGH)
    n_high = (await db.execute(select(func.count()).select_from(base.subquery()))).scalar_one()
    sq = (
        select(Task.project_id, func.count(Task.id).label("n"),
               func.count(Task.id).filter(Task.title != "zz").label("named"))
        .where(Task.project_id.is_not(None))
        .group_by(Task.project_id)
        .subquery()
    )
    per_project = (await db.execute(
        select(sq.c.project_id, sq.c.n, sq.c.named, Project.name.label("pname"))
        .join(Project, sq.c.project_id == Project.id)
        .order_by(sq.c.project_id)
    )).all()
    has = (await db.execute(select(Task.id).where(Task.project.has(name="Alpha")).order_by(Task.id))).scalars().all()
    anyp = (await db.execute(select(Project.id).where(Project.tasks.any(Task.priority == Priority.MEDIUM)).order_by(Project.id))).scalars().all()
    years = (await db.execute(select(extract("year", Task.created_at)).limit(1))).scalars().all()
    casted = (await db.execute(select(cast(Task.id, String)).order_by(Task.id).limit(2))).scalars().all()
    filters = [Task.id > 0]
    if prio:
        filters.append(Task.priority == Priority(prio))
    filtered = (await db.execute(select(func.count()).select_from(select(Task.id).where(and_(*filters)).subquery()))).scalar_one()
    # the latest task of each project: join(subquery, on) (the "greatest-n-per-group" pattern)
    latest_sq = (
        select(Task.project_id, func.max(Task.id).label("max_id"))
        .where(Task.project_id.is_not(None))
        .group_by(Task.project_id)
        .subquery()
    )
    latest = (await db.execute(
        select(Task)
        .join(latest_sq, (Task.project_id == latest_sq.c.project_id) & (Task.id == latest_sq.c.max_id))
        .order_by(Task.id)
    )).scalars().all()
    return {"n_high": n_high, "per_project": [list(r) for r in per_project], "has": has, "any": anyp,
            "year_ok": [y > 2000 for y in years], "cast": casted, "filtered": filtered,
            "latest": [[t.id, t.project_id] for t in latest]}


@app.post("/tasks/{task_id}/tag")
async def tag_task(task_id: int, db: DbDep, tag: str = "t"):
    task = await db.get(Task, task_id)
    task.tags.append(tag)
    flag_modified(task, "tags")
    await db.commit()
    await db.refresh(task)
    return {"tags": task.tags}


@dataclass(frozen=True)
class Window:
    start: int
    end: int


@dataclass
class Annex:
    code: str
    title: str = ""

    def __post_init__(self) -> None:
        if not self.title:
            self.title = self.code.upper()


def describe_call(a: int, b: int = 2, *, c: str = "x") -> dict:
    return {"a": a, "b": b, "c": c}


@app.post("/batch2")
async def batch2(db: DbDep):
    out = {}
    out["strptime"] = [datetime.strptime("05/10/2026 14:30", "%d/%m/%Y %H:%M").isoformat(),
                       datetime.strptime("2026-10-05", "%Y-%m-%d").isoformat()]
    errs = []
    for value, fmt in (("05/10/2026", "%Y-%m-%d"), ("2026-10-05xx", "%Y-%m-%d")):
        try:
            datetime.strptime(value, fmt)
        except ValueError as e:
            errs.append(str(e))
    out["strptime_errors"] = errs
    out["fromts"] = datetime.fromtimestamp(1700000000, tz=UTC).isoformat()
    out["choice_ok"] = secrets.choice("abc") in "abc"
    q = select(Task.id, Task.title).where(Task.id > 0)
    out["only"] = (await db.execute(q.with_only_columns(func.count()))).scalar_one() > 0
    reader = csv.reader(["h1,h2\n", "a,b\n", "c,d\n"])
    headers = next(reader)
    out["csv"] = [headers, [row for row in reader]]
    out["kw_builtins"] = [list(enumerate("ab", start=2)), sum([1, 2], start=10), round(2.675, ndigits=2), int("ff", base=16)]
    zips = [list(zip([1, 2], "ab", strict=True))]
    for a in (([1], [1, 2]), ([1, 2], [1]), ([1], [1], [1, 2]), ([1, 2], [1, 2], [1])):
        try:
            zip(*a, strict=True)
            list(zip(*a, strict=True))
        except ValueError as e:
            zips.append(str(e))
    out["zip_strict"] = zips
    now_d = datetime(2026, 1, 5).date()
    out["hasattr_builtin"] = [hasattr(datetime(2026, 1, 5), "date"), hasattr(now_d, "date"), hasattr("x", "casefold"),
                              hasattr([], "append"), hasattr({}, "nope"), hasattr(1.5, "is_integer")]
    # items without newline (text.splitlines()): one record each, a quoted field continues on the next item
    out["csv_lines"] = [list(csv.reader("A;B\r\n1;2\n\n3;\"x\ny\"".strip().splitlines(), delimiter=";")),
                        list(csv.reader(['a,"b', 'c"', "", "d"])), [r for r in csv.DictReader(["k,v", "1,2"])]]
    w = Window(1, 2)
    try:
        w.start = 5
    except AttributeError as e:
        out["frozen"] = str(e)
    out["hash"] = len({Window(1, 2), Window(1, 2), Window(2, 3)})
    out["annex"] = [Annex("bail").title, Annex("x", "given").title]
    data = {"b": 5, "c": "y"}
    out["spread"] = describe_call(1, **data)
    try:
        describe_call(c="z")
    except TypeError as e:
        out["missing"] = str(e)
    out["imported"] = __import__("datetime").date(2026, 1, 2).isoformat()
    return out


@app.post("/token")
async def token_login(form: OAuth2PasswordRequestForm = Depends()):
    return {"user": form.username, "pw_len": len(form.password), "scopes": form.scopes,
            "grant": form.grant_type, "cid": form.client_id}


@app.get("/mounted/native")
async def mounted_native():
    """Translated, under the prefix of the mount below: GET stays in the binary, other methods go to the mount."""
    return {"side": "route"}


# last registration: left to the Python side (--python-side mount), see mounted.py
app.mount("/mounted", mounted.sub)
