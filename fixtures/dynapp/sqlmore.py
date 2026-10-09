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
