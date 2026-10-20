"""Process-level shapes of real projects (a lifespan starting background jobs): `fcntl.flock` on a lock file (a second open file of the
same path conflicts with LOCK_NB: BlockingIOError), sqlalchemy.exc.InterfaceError and
DBAPIError.connection_invalidated in a transient-error classifier."""
import fcntl

from fastapi import APIRouter
from sqlalchemy import text
from sqlalchemy.exc import DBAPIError, InterfaceError, OperationalError

from fixtures.dynapp.db import DbDep

router = APIRouter(prefix="/procx")


def fail(e: Exception):
    return {"err": f"{type(e).__name__}: {e}"}


def is_transient(exc: BaseException) -> bool:
    if isinstance(exc, ConnectionError | TimeoutError):
        return True
    if isinstance(exc, OperationalError | InterfaceError):
        return True
    return isinstance(exc, DBAPIError) and exc.connection_invalidated


@router.get("/flock/{name}")
async def flock(name: str, op: int = 6):
    path = f"/tmp/py2axum-procx-{name}.lock"
    first = open(path, "w")
    out = []
    try:
        out.append(fcntl.flock(first, op))
        second = open(path, "w")
        try:
            fcntl.flock(second, fcntl.LOCK_EX | fcntl.LOCK_NB)
            out.append("second locked")
        except (OSError, BlockingIOError):
            out.append("second refused")
        finally:
            second.close()
        out.append(fcntl.flock(first, fcntl.LOCK_UN))
    except Exception as e:  # noqa: BLE001
        out.append(fail(e))
    finally:
        first.close()
    return {"out": out, "consts": [fcntl.LOCK_SH, fcntl.LOCK_EX, fcntl.LOCK_NB, fcntl.LOCK_UN]}


@router.get("/flock-errors")
async def flock_errors():
    out = []
    for fd, op in [(-1, fcntl.LOCK_EX), ("x", fcntl.LOCK_EX), (0, "x"), (0, 2**40), (987654, fcntl.LOCK_EX)]:
        try:
            out.append(fcntl.flock(fd, op))
        except Exception as e:  # noqa: BLE001
            out.append([fail(e), isinstance(e, BlockingIOError), getattr(e, "errno", None)])
    f = open("/tmp/py2axum-procx-closed.lock", "w")
    f.close()
    try:
        fcntl.flock(f, fcntl.LOCK_EX)
    except Exception as e:  # noqa: BLE001
        out.append(fail(e))
    return out


@router.get("/dbapi")
async def dbapi(db: DbDep):
    try:
        await db.execute(text("SELECT 1 / 0"))
        return {"v": "no error"}
    except DBAPIError as e:
        await db.rollback()
        return {"cls": type(e).__name__, "transient": is_transient(e), "invalidated": e.connection_invalidated,
                "iface": [issubclass(InterfaceError, DBAPIError), isinstance(e, InterfaceError)],
                "blocking": [issubclass(BlockingIOError, OSError), BlockingIOError.__name__]}
