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
