"""More SQLAlchemy: the session's connection, SQL constructs of reporting queries."""
from fastapi import APIRouter, Response
from sqlalchemy import select, text
from sqlalchemy.exc import InvalidRequestError, ResourceClosedError

from .db import DbDep
from .models import Owner

router = APIRouter(prefix="/sqlm")


@router.get("/ready")
async def ready(response: Response, db: DbDep):
    try:
        connection = await db.connection()
        await connection.close()
    except Exception as err:  # noqa: BLE001
        response.status_code = 503
        return {"err": str(err)}
    await db.rollback()  # get_db commits afterwards: a closed connection would make it raise


@router.get("/closed/{op}")
async def closed(op: str, db: DbDep):
    """After `connection().close()`: statements raise until rollback() or close(); commit() raises."""
    c = await db.connection()
    out = [type(c).__name__]
    await c.close()
    await c.close()
    if op == "commit":
        try:
            await db.commit()
        except InvalidRequestError as e:
            out.append([type(e).__name__, str(e)])
        await db.rollback()
        return out
    elif op == "rollback":
        await db.rollback()
    elif op == "close":
        await db.close()
    try:
        out.append(await db.scalar(text("select 7")))
        out.append(len((await db.scalars(select(Owner))).all()))
    except ResourceClosedError as e:
        out.append([type(e).__name__, str(e), isinstance(e, InvalidRequestError)])
    await db.rollback()
    return out


@router.get("/session")
async def session_params(db: DbDep):
    """`create_async_engine(connect_args={"options": ...})` (db.py): the pool's session parameters."""
    when = await db.scalar(text("select timestamptz '2026-06-01 12:00:00+00'"))
    return {
        "tz": await db.scalar(text("show timezone")),
        "app": await db.scalar(text("show application_name")),
        "timeout": await db.scalar(text("show statement_timeout")),
        "when": when,
        "offset": str(when.utcoffset()),
        "local": str(await db.scalar(text("select (timestamptz '2026-06-01 12:00:00+00')::timestamp"))),
    }


@router.get("/concat")
async def concat(db: DbDep):
    """`+` with a string: SQLAlchemy's `||` (a function of unknown type, a String column, either side)."""
    from sqlalchemy import func

    return {
        "func": await db.scalar(select(func.upper("lowercase") + " suffix")),
        "left": await db.scalar(select("prefix " + func.lower("ABC"))),
        "col": (await db.scalars(select(Owner.name + "!").order_by(Owner.id))).all(),
        "num": await db.scalar(select(func.abs(-2) + 3)),
    }


@router.get("/correlate/{case}")
async def correlate(case: str, db: DbDep):
    """Correlation of EXISTS and scalar subqueries in a statement that joins their tables: `rel.has()` / `rel.any()`
    correlate except their target (SQLAlchemy's _criterion_exists), `exists()` and `scalar_subquery()` auto-correlate
    (InvalidRequestError once every table is joined outside), `correlate()` / `correlate_except()`."""
    from sqlalchemy import exists, func

    from .models import Project, Task

    o = Owner(name="corr-owner")
    a, b = Project(name="CorrA", owner=o), Project(name="CorrB")
    db.add_all([a, b, Task(title="c1", project=a), Task(title="c2", project=a), Task(title="c3", project=b)])
    await db.flush()
    t = select(Task.title).join(Task.project).where(Project.name.like("Corr%"))
    p = select(Project.name).join(Project.tasks).where(Project.name.like("Corr%"))
    stmts = {
        "join_has": t.where(Task.project.has(name="CorrA")),
        "join_has_kw_crit": t.where(Task.project.has(Project.name.like("Corr%"), owner_id=o.id)),
        "join_has_other": t.where(Project.owner.has(name="corr-owner")),
        "join_any": p.where(Project.tasks.any(Task.title == "c3")),
        "nested": p.where(Project.tasks.any(Task.project.has(name="CorrA"))),
        "nested_plain": select(Project.name).where(Project.tasks.any(Task.project.has(name="CorrB"))),
        "exists_auto": t.where(exists().where(Project.id == Task.project_id, Project.name == "CorrA")),
        "exists_correlate": t.where(exists().where(Project.id == Task.project_id, Project.name == "CorrA").correlate(Task)),
        "exists_except": t.where(exists().where(Project.id == Task.project_id, Project.name == "CorrB").correlate_except(Project)),
        "exists_except_none": select(Task.title).where(
            exists().where(Project.id == Task.project_id, Project.name == "CorrB").correlate_except(None)),
        "scalar_auto": t.where(select(func.count(Project.id)).where(Project.id == Task.project_id).scalar_subquery() > 0),
        "scalar_except": t.where(select(func.count(Project.id)).where(Project.id == Task.project_id, Project.name == "CorrA")
                                 .correlate_except(Project).scalar_subquery() > 0),
        "scalar_except_after": t.where(select(func.count(Project.id)).where(Project.id == Task.project_id)
                                       .scalar_subquery().correlate_except(Project) > 0),
    }
    stmt = stmts[case]
    try:
        out = sorted((await db.scalars(stmt)).all())
    except InvalidRequestError as e:
        out = [type(e).__name__, "auto-correlation" in str(e)]
    await db.rollback()
    return out
