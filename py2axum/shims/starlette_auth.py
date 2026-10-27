"""`starlette.middleware.authentication.AuthenticationMiddleware` and the classes of `starlette.authentication`
(Starlette 1.x) written in the subset py2axum translates: the middleware is a raw ASGI class (its project
subclasses, `backend=` and `on_error=` are the project's), the users and credentials plain classes a project may
derive from. Loaded by `ModuleIndex.load_shim`, never imported. `requires()` (it inspects the endpoint's signature)
is not modelled."""
from starlette.requests import HTTPConnection
from starlette.responses import PlainTextResponse


class AuthenticationError(Exception):
    pass


class AuthenticationBackend:
    async def authenticate(self, conn):
        raise NotImplementedError()


class AuthCredentials:
    def __init__(self, scopes=None):
        self.scopes = [] if scopes is None else list(scopes)


class BaseUser:
    @property
    def is_authenticated(self):
        raise NotImplementedError()

    @property
    def display_name(self):
        raise NotImplementedError()

    @property
    def identity(self):
        raise NotImplementedError()


class SimpleUser(BaseUser):
    def __init__(self, username):
        self.username = username

    @property
    def is_authenticated(self):
        return True

    @property
    def display_name(self):
        return self.username

    @property
    def identity(self):
        return self.username


class UnauthenticatedUser(BaseUser):
    @property
    def is_authenticated(self):
        return False

    @property
    def display_name(self):
        return ""

    @property
    def identity(self):
        return ""


class AuthenticationMiddleware:
    def __init__(self, app, backend, on_error=None):
        self.app = app
        self.backend = backend
        self.on_error = on_error if on_error is not None else self.default_on_error

    async def __call__(self, scope, receive, send):
        if scope["type"] not in ["http", "websocket"]:
            await self.app(scope, receive, send)
            return

        conn = HTTPConnection(scope)
        try:
            auth_result = await self.backend.authenticate(conn)
        except AuthenticationError as exc:
            response = self.on_error(conn, exc)
            if scope["type"] == "websocket":
                await send({"type": "websocket.close", "code": 1000})
            else:
                await response(scope, receive, send)
            return

        if auth_result is None:
            auth_result = AuthCredentials(), UnauthenticatedUser()
        scope["auth"], scope["user"] = auth_result
        await self.app(scope, receive, send)

    @staticmethod
    def default_on_error(conn, exc):
        return PlainTextResponse(str(exc), status_code=400)
