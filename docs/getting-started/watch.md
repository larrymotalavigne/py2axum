# Watch mode

While you port an application, the loop is: change the Python, translate, build, run, look. `py2axum watch` does
it for you each time a file changes:

```bash
py2axum watch examples/bookshelf/app --root examples/bookshelf --python-side auto -o build/bookshelf --run
```

```
[py2axum watch 16:14:51] watching examples/bookshelf (Ctrl-C to stop)
[py2axum watch 16:14:51] initial build
[py2axum watch 16:14:53] translated in 2.2 s
python-side (auto): /books/export.zip (GET export_books) — examples/bookshelf/app/routers/export.py:28: library call `zipfile.ZipInfo()` is not supported (not in the py2axum library map) [P2A0201]
[py2axum watch 16:14:53] 56 generated file(s) changed
[py2axum watch 16:17:17] built in 144.0 s
[py2axum watch 16:17:17] started bookshelf_axum (pid 90848)
listening on http://0.0.0.0:8080
[py2axum watch 16:19:41] changed: examples/bookshelf/app/routers/books.py
[py2axum watch 16:19:43] translated in 2.0 s
[py2axum watch 16:19:43] 1 generated file(s) changed
[py2axum watch 16:19:51] built in 7.7 s
[py2axum watch 16:19:51] started bookshelf_axum (pid 91304)
listening on http://0.0.0.0:8080
```

The first build compiles the dependencies (two to three minutes); after that, a change is served again in about
ten seconds on a laptop: two for the translation, the rest for the incremental build of the generated crate.

- It watches the Python files under `--root` and the files that fix library versions (`uv.lock`,
  `pyproject.toml`, `requirements*.txt`). Editors save in several writes: a burst of changes becomes one cycle
  once the files have been quiet for `--debounce` seconds (0.3 s).
- Each cycle translates into a staging directory, then copies into `-o` **only the files whose content
  changed**. Cargo's fingerprints stay valid: the dependencies (axum, sqlx, tokio...) are built once, later
  builds recompile the generated crate alone, and a change that alters no generated file (a comment in a
  function's body, for instance) skips the build.
- It builds the **debug** profile, which is incremental: seconds instead of the minutes of the release profile
  and its link-time optimisation. `--release` builds the release profile, for a measurement.
- `--run` starts the binary and restarts it after each successful build (SIGTERM, then SIGKILL after
  `--stop-timeout`, 5 s); Ctrl-C (or SIGTERM) stops the binary and the watch. Arguments after `--` are passed
  to the binary; its settings come from the environment, as always: `DATABASE_URL=postgresql://localhost/bookshelf py2axum watch ...`.
- A cycle that fails prints the error and **keeps the previous binary running**: a refused construct (with its
  [error code](../reference/errors.md), why and what to do), or a compile error of the generated crate (which
  would be a py2axum bug: please report it).
- `--python-side auto` is handy while porting: a refused route is left to Python instead of stopping the cycle,
  and the cycle prints which routes moved to Python and why (and which ones became native again). Run the
  Python application next to it with `PY2AXUM_PYTHON_URL` set ([Hybrid mode](hybrid.md)) to serve them.
- `--no-build` only regenerates, for an editor or another tool that builds the crate.

Files are polled (every `--interval` seconds, 0.5 s by default) rather than watched through file system events:
there is no dependency to install, and it works the same on macOS, Linux and on volumes mounted in a container.

All options: [Command line § py2axum watch](../reference/cli.md#py2axum-watch).
