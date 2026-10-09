# First application

This page builds the [`examples/bookshelf`](https://github.com/larrymotalavigne/py2axum/tree/main/examples/bookshelf)
application into a binary, runs it, compares it with FastAPI, then shows the same steps for your own
application. [`py2axum check`](check.md) tells you beforehand which routes translate: in this example, all but
the CSV export, which uses the `zipfile` module.

## Generate and build

The export route stays in Python; everything else is compiled:

```bash
py2axum examples/bookshelf/app --root examples/bookshelf --python-side auto \
  -o build/bookshelf --name bookshelf
cargo build --release --manifest-path build/bookshelf/Cargo.toml
```

```
python-side (auto): /books/export.zip (GET export_books) — examples/bookshelf/app/routers/export.py:26: library call `zipfile.ZipFile()` is not supported (not in the py2axum library map)
generated build/bookshelf: 28 functions, 3 models, 12 schemas
```

`build/bookshelf` is an ordinary Cargo project: `src/gen.rs` is your application compiled to Rust (each
handler starts with a `/// METHOD path (from file:line)` comment), `src/dynrt/` is py2axum's runtime, and the
`Cargo.lock` is the one py2axum is tested with. Do not edit it: change the Python and generate again. The
binary is `build/bookshelf/target/release/bookshelf`.

## Run the binary

The binary does not create or migrate tables: run your migrations (Alembic, or here `schema.sql`) first.

```bash
createdb bookshelf
psql bookshelf -f examples/bookshelf/schema.sql
DATABASE_URL=postgresql://localhost/bookshelf PORT=8080 ./build/bookshelf/target/release/bookshelf
```

```bash
curl -s localhost:8080/auth/register -H 'content-type: application/json' \
  -d '{"email": "ada@example.org", "password": "correct horse battery", "display_name": "Ada"}'
curl -s localhost:8080/auth/register -H 'content-type: application/json' -d '{"email": "nope"}'   # FastAPI's 422
```

The application reads its own settings as it would in Python (`os.environ`, pydantic-settings): here
`BOOKSHELF_SECRET` signs the tokens. Without `PY2AXUM_PYTHON_URL`, the route left to Python answers 404: see
[Hybrid mode](hybrid.md) to serve it. The binary's own settings are listed in
[Environment variables](../reference/environment.md).

## Compare the binary with the Python application

Translating is not enough: py2axum's promise is that the binary answers like FastAPI, byte for byte. The
example ships its proof:

```bash
pip install -r examples/bookshelf/requirements.txt httpx websockets
DATABASE_URL=postgresql://localhost/bookshelf examples/bookshelf/compare.sh
```

It translates and builds the example, starts FastAPI on port 9050 and the binary on port 9090 (relaying the
export route to FastAPI) against the same database, and plays
[`scenario.py`](https://github.com/larrymotalavigne/py2axum/blob/main/examples/bookshelf/scenario.py)
on both: registrations, logins, valid and invalid tokens, CRUD, 422s, reviews, the export, WebSocket sessions.

```
ok   201 POST /auth/register
ok   422 POST /auth/register
...
ok   101 WS /ws/books/1?token=eyJhbGciOiJIUzI1NiIs...
ok   403 WS /ws/books/1?token=garbage
...
75/75 identical responses
```

A `DIFF` line prints both responses. To write such a scenario for your own application, and to compare on
recorded production traffic or generated requests, see [Conformance and replay](../advanced/conformance.md).

## Your own application

```bash
cd my-project                                   # where you run `uvicorn api.main:app`
py2axum check api --root .
py2axum check api --root . --python-side auto --fail-under 80     # in CI: exit 1 below 80 % native
py2axum api --root . --python-side auto -o build/api --name api
```

- **Versions.** py2axum reads your `uv.lock` (else `requirements*.txt`, else `pyproject.toml`) to reproduce
  the behaviour of the versions you run: Starlette's CORS, Pydantic's error URLs, CPython's messages. A library
  you import whose locked version is outside the [tested ranges](../reference/versions.md) is refused
  (`--allow-untested-versions` overrides, at your own risk).
- **The database URL.** The binary accepts SQLAlchemy URLs (`postgresql+psycopg://`, `postgresql+asyncpg://`):
  the driver named there decides details that differ between drivers (asyncpg prints UTC instants with `Z`,
  psycopg with `+00:00`), so give the binary the URL your Python app uses.
- **Configuration.** The binary reads the environment, not `.env` files: export the variables (your
  orchestrator does), or `set -a; . ./.env; set +a` locally.
- **JSON output** of `check` (`--json`) gives the same verdicts to scripts; `--report coverage.md` writes a
  detailed Markdown/JSON report. All options: [Command line](../reference/cli.md).

Then [deploy it](../advanced/deployment.md).
