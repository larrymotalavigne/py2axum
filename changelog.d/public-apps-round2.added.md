- Public apps, round two: rafsaf/minimal-fastapi-postgres-template goes from 0 to 8 native routes out of 8,
  jod35/fastapi-beyond-CRUD from 0 to 21 out of 24 (its Celery routes stay in Python), the official template from 18
  to 19 out of 23 (`jinja2.Template`; its last four routes send e-mail with python-emails). Checked by
  `fixtures/langapp/depinst.py` and `fixtures/sqlmodelapp/bookly.py`.
- Callable instances as dependencies written in place (`Depends(RoleChecker(["admin"]))`) or through a module-level
  `admin_only = Depends(...)` listed in `dependencies=`, their `__call__` also inherited from a project base.
- Project subclasses of `HTTPBearer` with a `__call__` of their own calling `super().__call__(request)`: translated over
  HTTPBearer's source (`py2axum/shims/fastapi_security.py`); `super().m(...)` in a plain class's method.
- FastAPI's parameter markers (`Depends(...)`, `Query(...)`...) evaluated as values (an instance dependency's `__call__`
  defaults) are opaque objects instead of a refusal of the whole application.
- `TrustedHostMiddleware(allowed_hosts=...)` read at startup; `CORSMiddleware`, `TrustedHostMiddleware` and
  `HTTPSRedirectMiddleware` added in a `register(app)` function.
- SQLModel: `Relationship(link_model=)`, plain methods on table classes, `sqlmodel.desc`/`and_`/`or_`/... ; a
  many-to-many pair with `back_populates` keeps its other side in step on `append()`, `extend()`, `remove()` and
  assignment.
- A bare `raise` in an endpoint (no exception being handled) is CPython's RuntimeError instead of a refusal;
  `jinja2.Template(source).render(...)`.
