# `py2axum check`

```bash
py2axum check examples/bookshelf/app --root examples/bookshelf
```

`app` is the package that holds the FastAPI application; `--root` is the directory that would be on
`sys.path` when you run it (`uvicorn app.main:app` is run from `examples/bookshelf`). `check` runs the whole
translation without writing anything and answers route by route:

```
py2axum check examples/bookshelf/app

native       POST /auth/login               examples/bookshelf/app/routers/auth.py:26
native       POST /auth/register            examples/bookshelf/app/routers/auth.py:14
native       GET /books                     examples/bookshelf/app/routers/books.py:39
native       POST /books                    examples/bookshelf/app/routers/books.py:59
refused      GET /books/export.zip          examples/bookshelf/app/routers/export.py:18
             └ examples/bookshelf/app/routers/export.py:28: library call `zipfile.ZipInfo()` is not supported (not in the py2axum library map) [P2A0201]
               help: Use a supported library or the standard library for the same job (see Libraries and Standard library). Or leave the route to Python: --python-side '/books/export.zip'.
native       GET /books/stats               examples/bookshelf/app/routers/books.py:68
...
native       WEBSOCKET /ws/books/{book_id}  examples/bookshelf/app/routers/live.py:43

What would make the most routes native (in this order):
  +1   →   13/13  library zipfile [P2A0201]

Blockers (routes touched, routes for which it is the only blocker):
     1    1  library zipfile  (examples/bookshelf/app/routers/export.py:28)

12/13 routes native (92.3 %), 0 python-side, 1 refused — generation would fail
explain a code: py2axum check --explain P2A0201
hint: --python-side auto leaves the refused routes to a Python process next to the binary
```

- **native**: compiled into the binary.
- **refused**: something this route reaches is outside the [supported subset](../supported.md), here the
  `zipfile` module used by the CSV export. py2axum never guesses: a construct it cannot reproduce exactly
  stops the translation, with the `file:line` that causes it.
- **python-side**: left to a Python process running next to the binary ([Hybrid mode](hybrid.md)).

Each refusal carries a stable [error code](../reference/errors.md) and a `help:` line: what to do about this
construct, and the `--python-side` flag that leaves this route to Python. `py2axum check --explain P2A0201`
prints why a class of construct is refused and what to do.

*What would make the most routes native* orders the refused constructs greedily: supporting (or rewriting) the
first one makes the most routes native, then the next one given the first, and so on, with the count of native
routes after each step. The blockers table gives, for each construct, the routes it touches and those for which
it is the only blocker.

`--format json` gives the same verdicts to scripts (each route with its `code` and `suggestion`, plus the
`unblock` list), `--format markdown` renders them as tables for a pull request comment or a CI job summary, and
`--fail-under PCT` makes `check` fail in CI below a share of native routes: see
[Command line](../reference/cli.md).

## When something is refused

Every refusal names the construct, its `file:line` and its [error code](../reference/errors.md). In order of
preference:

1. **Leave it to Python**: `--python-side auto`, or `--python-side PATH` for chosen routes. Correct by
   construction, at the cost of a Python process for those paths.
2. **Rewrite the line** with something the [supported subset](../supported.md) covers, when it is incidental
   (a library call that the standard library or a supported library does as well).
3. **Report it**: a minimal reproduction of a construct real applications use is the most useful
   contribution ([CONTRIBUTING.md](https://github.com/larrymotalavigne/py2axum/blob/main/CONTRIBUTING.md)).

A library version outside the tested ranges is refused for the whole application: use a version in range, or
`--allow-untested-versions` after checking the behaviour with a conformance run.

For a whole application, step by step (measure, fix or leave to Python, prove, switch reversibly), see
[Migrating an existing application](migration.md).
