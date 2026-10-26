- prometheus_fastapi_instrumentator 8.x (dynrt/instr.rs): `Instrumentator(...).instrument(app).expose(app)`, its
  middleware placed in the stack, the default metrics labelled with the matching route's template, `/metrics`.
