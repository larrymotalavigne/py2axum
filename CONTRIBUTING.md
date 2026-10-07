# Contributing to py2axum

Thanks for helping! The most useful contributions are **real constructs that do not translate yet** and
**behaviours that differ from Python**.

## Reporting a construct or a difference

Open an issue with:

1. the smallest Python snippet that shows it (a route is ideal);
2. what FastAPI answers (status, headers, body) and what the binary answers, or py2axum's error message;
3. the versions involved (Python, FastAPI, Pydantic, SQLAlchemy — or your `uv.lock`).

`py2axum <package> --root <root> --report coverage.md` lists, per route, the first blocking construct with
`file:line`: attach the relevant lines.

## Development setup

```bash
uv venv && uv pip install -e ".[dev,conformance]"
python -m pytest                     # transpiler tests (rejections, report)
```

Conformance suites need PostgreSQL (and Redis and RabbitMQ for `dynapp`):

```bash
createdb py2axum_dyn
export DATABASE_URL=postgresql://postgres@127.0.0.1/py2axum_dyn
python -m py2axum fixtures/dynapp --root . --backend dyn --python-side '/dunders/proxied/{name}' \
    -o generated/dynapp_axum --name dynapp_axum
(cd generated/dynapp_axum && RUSTFLAGS="-D warnings" cargo build --release)
python -c "from tests.scenarios import dynapp; dynapp.reset('$DATABASE_URL')"
./scripts_start_dyn.sh
python tests/conformance.py http://127.0.0.1:8200 http://127.0.0.1:8280 --scenario dynapp
```

## The rules of the project

1. **Observable equivalence.** A translated construct behaves like Python: status, headers, content type and
   body bytes (JSON key order, Pydantic error details). Every new behaviour comes with a conformance case in
   a fixture app (`fixtures/`) compared against FastAPI.
2. **Refuse rather than guess.** What the runtime cannot reproduce is refused at compile time with a
   `TranspileError` pointing at `file:line`, and a test in `tests/test_rejects_dyn.py`. Never translate a
   construct approximately and silently.
3. **Document every difference** in [docs/supported.md](docs/supported.md).
4. **Library behaviour follows the project's locked version** (`uv.lock`) when it changed across versions.
5. The generated crate must build with `RUSTFLAGS="-D warnings"`; never edit generated code by hand — fix the
   transpiler or the runtime.

## Where things are

- `py2axum/dyn.py` — the compiler of project code (dyn backend); `libmap.py` — the library map.
- `py2axum/runtime/dynrt/*.rs` — the runtime (one module per area or library).
- `fixtures/dynapp/` — the reference app; add your case to the module that fits (or a new router) and a
  request to `tests/scenarios/dynapp.py`.

## Pull requests

Keep them focused, describe the Python behaviour you reproduce (with the library version), and include the
conformance result. By contributing you agree that your contribution is licensed under the Apache License 2.0.

## Releasing

1. Update `version` in `pyproject.toml` and `CHANGELOG.md`; tag `vX.Y.Z` and create the GitHub release.
2. Run the **Publish to PyPI** workflow on that tag (Trusted Publishing: the PyPI project's trusted
   publisher names this repository, `release.yml` and the `pypi` environment).
