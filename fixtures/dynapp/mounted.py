"""A sub-application mounted last (`app.mount("/mounted", sub)` at the end of main.py): it stays in Python
(--python-side mount). The binary relays to PY2AXUM_PYTHON_URL every request under /mounted that no translated
route fully matches: a HEAD or a POST on the GET route /mounted/native reach the sub-application, as in
Starlette's router (the mount's full match beats the route's partial one)."""

from starlette.applications import Starlette
from starlette.responses import JSONResponse
from starlette.routing import Route


async def echo(request):
    body = (await request.body()).decode()
    return JSONResponse({"sub": request.path_params["rest"], "path": request.url.path, "root_path": request.scope["root_path"],
                         "method": request.method, "body": body}, headers={"x-mounted": "1"})


sub = Starlette(routes=[Route("/{rest:path}", echo, methods=["GET", "POST", "DELETE"])])
