# Command line

py2axum has three commands: generation (`py2axum <package>`), which writes a Cargo project;
`py2axum check <package>`, which runs the same translation without writing anything and reports route by route;
and `py2axum watch <package>`, which regenerates, rebuilds and restarts the binary each time the project changes.

When generation refuses a construct, it prints its [error code](errors.md), the `file:line`, why and what to do:

```text
error[P2A0201]: app/routers/export.py:28: library call `zipfile.ZipInfo()` is not supported (not in the py2axum library map)
  = why: The call goes to a library the binary has no equivalent for: py2axum maps a closed list of library ...
  = help: Use a supported library or the standard library for the same job (see Libraries and Standard ...
  = see: https://larrymotalavigne.github.io/py2axum/reference/errors/#p2a0201
```

## Generating a crate

```bash
py2axum api --root . --python-side auto -o build/api --name api
cargo build --release --manifest-path build/api/Cargo.toml
```

- `package` is the directory of the package that holds the FastAPI application.
- `--root` is the import root, the directory that would be on `sys.path` when you run the application
  (`uvicorn api.main:app` is run from it); by default, the parent of the package.
- `-o`/`--out` is the output directory of the Rust project, `--name` the crate's name (and the binary's;
  default `<package>_axum`). The generated project is an ordinary Cargo project: do not edit it, change the
  Python and generate again ([First application](../getting-started/first-app.md#generate-and-build)).
- `--python-side PATH|lifespan|mount|auto`, repeatable, says what stays in a Python process next to the
  binary ([Hybrid mode](../getting-started/hybrid.md)): a route pattern or a mount prefix (`PATH`), the
  lifespan, the app's last `app.mount()`s (`mount`: they get every request no translated route fully
  matches), or `auto`: every route that does not translate, and such mounts. Without it, generation fails on
  the first refused route.
- `--no-stream` buffers list responses instead of streaming them
  ([Large list responses](../advanced/streaming.md)).
- `--translate-libs none|auto|DIST,...`, `--venv` and `--allow-license` (experimental, off by default) translate pure-Python
  libraries from their source: [Libraries translated from source](../advanced/libraries.md).
- `--allow-untested-versions` translates even if the project locks library versions outside the
  [tested ranges](versions.md), at your own risk.
- `--python X.Y` names the Python the project runs on (3.12 to 3.15; also `PY2AXUM_TARGET_PYTHON`): the binary
  reproduces its messages and behaviours, and `sys.version_info` is folded against it. Default: the project's
  `.python-version`, else this Python when `requires-python` allows it ([Python versions](../python-versions.md)).
  `check` and `watch` take it too.
- `--backend` was removed in 1.0 (it had no effect since 0.4: there is one backend). Passing it is an error
  (`error: --backend was removed in 1.0 ...`, exit status 2): drop it.

The options, their defaults and the exit statuses are a stable interface: see
[Versioning and deprecation](versioning.md).

<!-- py2axum:cli-help -->

## `py2axum check`

```bash
py2axum check api --root .
py2axum check api --root . --python-side auto --fail-under 80     # in CI: exit 1 below 80 % native
```

`--path DIR` (experimental, repeatable) adds an import root next to `--root`: the sibling packages of a monorepo
(`oasst-shared/oasst_shared`, `src/lfx/src`) that the application installs from its own checkout resolve as in its
virtual environment. A module is looked up in `--root` first, then in each `--path` in order.

`check` takes the same options as generation (`package`, `--root`, `--python-side` with `PATH`, `lifespan`,
`mount` or `auto`, `--allow-untested-versions`, `--python`, `--translate-libs`, `--venv`, `--allow-license`)
and computes its verdict with the same code: a route is *python-side* when generation with the same options
would relay it to Python (declared, moved by `auto`, or reading what a lifespan left to Python sets),
*refused* when generation would stop on it, and `generates` is true when generation would succeed. Native +
python-side + refused is every route of the application. It adds:

- `--format text|json|markdown`: the terminal report (default); machine-readable output on stdout, with the
  same verdicts, each refused route's `code` and `suggestion`, and the `unblock` list (`--json` is a synonym;
  its format is published as a JSON Schema, [`check-schema.json`](check-schema.json));
  or Markdown tables for a pull request comment or a CI job summary (`>> "$GITHUB_STEP_SUMMARY"`);
- `--fail-under PCT`: also exit 1 when fewer than `PCT` % of the routes are native;
- `--explain CODE`: print why a class of construct is refused and what to do ([Error codes](errors.md)), then
  exit.

Errors that concern the whole application (and versions outside the tested ranges) are listed apart: they
refuse the generation whatever the routes. Exit status: 0 when generation would succeed (and native routes ≥
`--fail-under`), 1 otherwise. Reading its output: [`py2axum check`](../getting-started/check.md).

<!-- py2axum:check-help -->

## `py2axum watch`

```bash
DATABASE_URL=postgresql://localhost/app py2axum watch api --root . --python-side auto -o build/api --run
```

`watch` takes generation's `package`, `-o`, `--root`, `--name`, `--python-side`, `--no-stream` and
`--allow-untested-versions`, watches the Python files under `--root`, and on each change regenerates, copies
the changed generated files only, builds the debug profile (incremental) and, with `--run`, restarts the binary.
A failed cycle prints the error and keeps the previous binary running. Ctrl-C stops the binary and exits. Guide:
[Watch mode](../getting-started/watch.md).

<!-- py2axum:watch-help -->

## Options in `pyproject.toml`

An application's options can live in its `pyproject.toml`, under `[tool.py2axum]`, so that a plain
`py2axum check` or `py2axum -o build/api` runs with them (from the directory of the file or below it):

```toml
[tool.py2axum]
package = "api"
root = "."
app = "api.main:app"
python-side = ["auto"]
translate-libs = "auto"
name = "api"
fail-under = 90      # check only
```

The keys are the long options of the commands (`python-side` or `python_side`), plus `package` and `out`. A flag
takes `true`, a repeatable option a string or a list, and paths are relative to the `pyproject.toml`. The closest
`pyproject.toml` with a `[tool.py2axum]` table, from the current directory up, is read, and py2axum prints the
options it took from it. They come before the command line's: a value given on the command line wins, and a
repeatable option adds to the file's. A key that only another command takes (`fail-under` while generating, `run`
outside `watch`) is skipped; an unknown key, or `explain`/`json`, is an error (exit status 2).

## The binary's `migrate`

The generated binary applies the project's alembic migrations without Python when an `alembic.ini` was found at
generation ([Database migrations](../advanced/migrations.md)), on `DATABASE_URL`:

- `migrate [upgrade [head|<revision>]]` applies the revisions from where the database is, like `alembic upgrade`;
- `migrate current` prints the database's revision, like `alembic current`;
- `migrate --sql` prints the SQL `alembic upgrade head --sql` prints.

A revision refused at generation (`P2A0510`, or one reading the database) stops `migrate` before anything is
applied: apply it with alembic.
