# Migrating an existing application

This guide takes a FastAPI application that runs in production under uvicorn to the binary, without a
flag day: measure, fix or leave to Python, prove the binary answers the same, run both side by side, then switch
in a way you can undo in one step. The [bookshelf example](https://github.com/larrymotalavigne/py2axum/tree/main/examples/bookshelf)
illustrates each step.

Your Python code stays the source of truth throughout: you never edit the generated Rust, and the Python
application keeps running, tested and deployable, until (and after) you switch.

## 1. Measure

From the directory you run uvicorn from:

```bash
pip install py2axum
py2axum check app --root .
```

`check` runs the whole translation without writing anything and gives a verdict per route: **native**,
**refused** (with the construct, its `file:line`, an [error code](../reference/errors.md) and a suggestion), or
**python-side**. For bookshelf:

```
refused      GET /books/export.zip          app/routers/export.py:18
             └ app/routers/export.py:28: library call `zipfile.ZipInfo()` is not supported (not in the py2axum library map) [P2A0201]
               help: Use a supported library or the standard library for the same job (see Libraries and Standard library). Or leave the route to Python: --python-side '/books/export.zip'.

What would make the most routes native (in this order):
  +1   →   13/13  library zipfile [P2A0201]

12/13 routes native (92.3 %), 0 python-side, 1 refused — generation would fail
```

Two kinds of refusals matter differently:

- **Whole application** refusals (listed first) stop the generation whatever the routes: a library version
  outside the [tested ranges](../reference/versions.md), a middleware of a library (it wraps every route), a
  router registered under an `if`... Fix these first: `--python-side` cannot work around them.
- **Route** refusals block only the routes that reach the construct. The summary *What would make the most
  routes native* orders them by how many routes each one unblocks.

`py2axum check --explain P2A0201` prints why a class of construct is refused and what to do. For a pull
request or a CI job summary, `--format markdown` renders the same verdicts as tables; `--format json` gives
them to scripts.

## 2. Fix, or leave to Python

For each refused construct, in order of preference:

1. **Rewrite the line** when the construct is incidental: a library the runtime has an equivalent for
   (`requests` → `httpx.AsyncClient`, `passlib` → `bcrypt`/`pwdlib`, `orjson` → `json`...; `check` names the
   alternative when it knows one), a form the [supported subset](../supported.md) covers. The application stays
   a normal FastAPI application: the change must be one you would accept in Python anyway.
2. **Leave the route to Python** when the construct is essential (a PDF or spreadsheet library, a payment SDK):
   `--python-side '/books/export.zip'`. The binary relays those paths to the Python application (see step 5).
3. **Report it** when a common construct is missing: a minimal reproduction is the most useful contribution.

Pin the list of Python-side routes explicitly in your build (`--python-side` per path) rather than relying on
`--python-side auto`: with `auto`, a route moves to the binary on its own after a py2axum upgrade, and you want
such moves to go through step 4.

While you iterate, [watch mode](watch.md) regenerates, rebuilds and restarts the binary on each save:

```bash
DATABASE_URL=postgresql://localhost/app py2axum watch app --root . --python-side auto -o build/app --run
```

Once the application translates, make it a CI gate so that new code does not regress:

```bash
py2axum check app --root . --python-side '/books/export.zip' --fail-under 90
```

## 3. Build

```bash
py2axum app --root . --python-side '/books/export.zip' -o build/app --name app
cargo build --release --locked --manifest-path build/app/Cargo.toml
```

The first release build takes a few minutes (link-time optimisation); later builds only recompile the
generated crate. The [Dockerfile of the deployment guide](../advanced/deployment.md#building-with-docker) does
the same in a multi-stage build.

## 4. Prove that the binary answers the same

Translation succeeding is not the goal: the binary must answer what FastAPI answers, byte for byte. Check it on
your application, from the most targeted to the broadest ([Conformance and replay](../advanced/conformance.md)):

1. **A scenario**: requests you write, played on uvicorn and on the binary against the same database, every
   response compared (status, headers that matter, body with key order). Bookshelf's
   [`scenario.py`](https://github.com/larrymotalavigne/py2axum/blob/main/examples/bookshelf/scenario.py) and
   [`compare.sh`](https://github.com/larrymotalavigne/py2axum/blob/main/examples/bookshelf/compare.sh) are a
   template: 75 requests and WebSocket sessions, run twice (normal, then list streaming forced).
2. **Replay**: traffic recorded in front of your Python application (anonymized as it is written), replayed on
   both servers from the same database snapshot.
3. **Generation**: requests derived from your OpenAPI schema, valid and invalid.

What a scenario must exercise is **the data states production has**, not only the routes: an empty test
database hides most differences. Make sure your data covers every value of every enum column, the optional
fields both set and null, a user in each subscription or role state, valid *and* invalid tokens for each kind of
link your application signs (magic links, password resets, e-mail confirmations), lists long enough to be
paginated. A route that answers 500 on both sides is a bug of the application: the comparison says "identical",
so read the reference statuses of a new scenario once.

Run the scenario in CI next to `check`: it is what makes a py2axum upgrade safe.

## 5. Run both, side by side

In hybrid mode the binary is the only entry point and relays the Python-side paths to your application under
uvicorn ([Hybrid mode](hybrid.md)):

```bash
uvicorn app.main:app --port 8000 &
PY2AXUM_PYTHON_URL=http://127.0.0.1:8000 ./build/app/target/release/app
```

In a container platform, run the Python application as a sidecar reachable from the binary only
([Deployment § Hybrid](../advanced/deployment.md#hybrid-binary-python-for-the-rest)). State both processes
share must live outside them (database, Redis): an in-memory rate limiter counts per process, as with several
uvicorn workers.

## 6. Switch, reversibly

Keep the Python server deployable and make the choice of server a setting, so that going back is one change
and one rollout, with no rebuild. One way: a single image holding both the binary and the Python application,
and an entry point that picks one.

```dockerfile
# final stage: the Python application image, plus the binary built in an earlier stage
COPY --from=build /src/build/app/target/release/app /usr/local/bin/app
COPY entrypoint.sh /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]
```

```sh
#!/bin/sh
# entrypoint.sh: API_SERVER=axum (the binary in front, Python behind it for the python-side routes)
#                API_SERVER=uvicorn (Python alone, as before the migration)
set -e
if [ "${API_SERVER:-uvicorn}" = "axum" ]; then
  uvicorn app.main:app --host 127.0.0.1 --port 8000 &   # only if some routes stay python-side
  export PY2AXUM_PYTHON_URL=http://127.0.0.1:8000
  exec app
fi
exec uvicorn app.main:app --host 0.0.0.0 --port "${PORT:-8080}"
```

(With several containers, the same idea is a variable that selects the container command, or two Deployments
behind one Service and a switch of the Service's selector.)

Then:

1. Deploy with `API_SERVER=axum`, at a quiet time.
2. Check that the binary serves: its `/version` or health route, a few real requests, the relayed routes.
3. Watch for at least an hour, then a day: 5xx rate (the binary does not log requests: read your ingress or
   proxy logs), error tracking (the [Sentry integration](../advanced/sentry.md) is native), memory (expect a
   fraction of uvicorn's: a few MiB at rest), latency.
4. **On any doubt, set `API_SERVER=uvicorn` and roll out**: Python serves alone again, as before. Then turn the
   failing request into a scenario step (step 4), fix or report, and try again.

Switch one application at a time, and leave a healthy day in production between two.

## 7. Afterwards

- Keep `py2axum check --fail-under` and the conformance scenario in CI: new code that does not translate, or
  translates differently, fails there instead of in production.
- Pin the py2axum version in your build. Upgrading py2axum is like upgrading FastAPI: rerun the scenario (and a
  replay if you have one) before deploying. A newer py2axum may translate routes that were Python-side: move
  them by removing their `--python-side`, through the same steps.
- Python stays the source of truth: develop and test the application as before; the binary is a build artifact.
