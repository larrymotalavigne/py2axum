- `@router.head(...)`, `@router.options(...)` and `@router.api_route(path, methods=[...])` (a literal list, GET when
  omitted: one route per method) are translated. An `app.mount(...)` followed by routes stays on the Python side
  when none of those routes can match a path under its prefix (the registration order does not matter then).
