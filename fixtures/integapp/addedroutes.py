"""Routes a function given the application adds (`app.add_route`) before and after a library's router left to
Python (fixtures/extlib/extroutes): Starlette tries the ones added after it only when none of the library's routes
matches, so the binary relays them (a library route may shadow one)."""
from starlette.responses import JSONResponse, PlainTextResponse


async def early(request):
    return JSONResponse({"added": "early"})


async def late(request):
    return JSONResponse({"added": "late"})


async def shadowed(request):
    return PlainTextResponse("never served: the library's route comes first")


def add_early(app):
    app.add_route("/added/early", early, methods=["GET"])


def add_late(app):
    app.add_route("/added/late", late, methods=["GET"])
    app.add_route("/ext/notes", shadowed, methods=["GET"])
