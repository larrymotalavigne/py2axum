- Starlette's `AuthenticationMiddleware` with a project `AuthenticationBackend` (also through a project subclass
  of the middleware, as teamhide/fastapi-boilerplate writes it), `request.user` / `request.auth`, and
  `starlette.authentication`'s `AuthCredentials`, `BaseUser`, `SimpleUser`, `UnauthenticatedUser`,
  `AuthenticationError`: translated from Starlette's source (`py2axum/shims/starlette_auth.py`); checked by
  `fixtures/sqlmodelapp/authx.py`.
