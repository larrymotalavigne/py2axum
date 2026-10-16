Stand-ins for libraries installed next to a project: on the `PYTHONPATH` of the reference servers that import them
(`scripts_start_integapp.sh`) and of `tests/settle.py`, never under the translated package, so py2axum sees them as
libraries (it never reads their source).

- `extroutes`: a library that ships an `APIRouter` (`fixtures/integapp/main.py` includes it last: its routes stay
  on the Python side, `--python-side mount`).
