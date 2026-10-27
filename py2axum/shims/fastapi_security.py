"""`fastapi.security.HTTPBearer` (FastAPI 0.137 to 0.162) written in the subset py2axum translates, for the project
subclasses that do more than override `make_not_authenticated_error` / `make_authenticate_headers` (those keep the
binary's own scheme, `FnCompiler.security_subclass`): a `__call__` of their own calling `super().__call__(request)`,
other methods, attributes read on `self`. Loaded by `ModuleIndex.load_shim` as the base of such a class only
(`Frontend._rebase_security_subclasses`), never imported. `self.model` (the OpenAPI model) is not kept."""
from fastapi import HTTPException
from fastapi.security import HTTPAuthorizationCredentials


class HTTPBearer:
    def __init__(self, *, bearerFormat=None, scheme_name=None, description=None, auto_error=True):
        self.scheme_name = scheme_name or self.__class__.__name__
        self.auto_error = auto_error

    def make_authenticate_headers(self):
        return {"WWW-Authenticate": "Bearer"}

    def make_not_authenticated_error(self):
        return HTTPException(status_code=401, detail="Not authenticated", headers=self.make_authenticate_headers())

    async def __call__(self, request):
        authorization = request.headers.get("Authorization")
        # get_authorization_scheme_param
        if not authorization:
            scheme, credentials = "", ""
        else:
            scheme, _, param = authorization.partition(" ")
            credentials = param.strip()
        if not (authorization and scheme and credentials):
            if self.auto_error:
                raise self.make_not_authenticated_error()
            else:
                return None
        if scheme.lower() != "bearer":
            if self.auto_error:
                raise self.make_not_authenticated_error()
            else:
                return None
        return HTTPAuthorizationCredentials(scheme=scheme, credentials=credentials)
