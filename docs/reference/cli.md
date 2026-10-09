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
- `--report FILE` writes a coverage report (Markdown + JSON) instead of generating; it never stops at the first
  error.
- `--allow-untested-versions` translates even if the project locks library versions outside the
  [tested ranges](versions.md), at your own risk.
- `--backend` is deprecated and has no effect: there is one backend since 0.4. `--backend auto` and
  `--backend dyn` are still accepted until 1.0; `--backend typed` is an error.

<!-- py2axum:cli-help -->

## `py2axum check`

```bash
py2axum check api --root .
py2axum check api --root . --python-side auto --fail-under 80     # in CI: exit 1 below 80 % native
```

`check` takes the same `package`, `--root`, `--python-side` (`PATH`, `lifespan` or `auto`) and
`--allow-untested-versions` as generation, and:

- `--format text|json|markdown`: the terminal report (default); machine-readable output on stdout, with the
  same verdicts, each refused route's `code` and `suggestion`, and the `unblock` list (`--json` is a synonym);
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
