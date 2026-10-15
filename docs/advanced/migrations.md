# Database migrations

A project with an `alembic.ini` (in its package directory or above, up to `--root`) gets its alembic migrations
built into the binary: `<binary> migrate` applies them without Python, so an image that only runs the binary can
also migrate its database (a Kubernetes init container running the same image, for instance).

```bash
DATABASE_URL=postgresql://... ./app migrate            # alembic upgrade head
DATABASE_URL=postgresql://... ./app migrate upgrade a1b2c3   # up to a revision
DATABASE_URL=postgresql://... ./app migrate current    # alembic current
./app migrate --sql                                     # alembic upgrade head --sql
```

`migrate` connects to `DATABASE_URL`, like the server (`postgresql+psycopg://` and `postgresql+asyncpg://` are
accepted), and exits with status 0 once the database is at the target, 1 otherwise.

## How the statements are made

At translation time, py2axum runs alembic's own offline mode (`alembic upgrade head --sql`) on the project's
revisions, one after the other, and keeps each revision's statements: they are alembic's by construction.
`env.py` is not run; its `context.configure()` options that change the statements are read from its source and
must be literals: `version_table`, `version_table_schema`, `transaction_per_migration`. At run time, the binary
reads the version table, finds the database on the upgrade path, and runs the statements of the revisions it lacks,
the version table's updates included, in alembic's transactions: one for the whole run, or one per revision with
`transaction_per_migration=True`; `autocommit_block()` commits and restarts it as alembic does. It logs the lines
alembic logs (`Context impl PostgresqlImpl.`, `Running upgrade a -> b, message`) through the logging sections of
`alembic.ini` (what `fileConfig` in `env.py` installs).

`tests/migrate_check.py` compares the binary with alembic on two databases: the same schema
(`pg_dump --schema-only`), the same rows, the same version table, the same log lines, `migrate --sql` equal to
`alembic upgrade head --sql`.

## Revisions the binary refuses

A revision that offline mode cannot render faithfully is refused at translation time, with its `file:line`; the
binary will not apply it (it says which revision and why, and applies nothing). A database already past it never
needs it: such revisions are usually old, and new ones can be written so that the binary applies them.

- a revision that reads the database: `op.get_bind()`, `op.get_context().bind`, `sa.inspect(...)` (offline mode
  has no connection: its rendering would not be what runs online);
- a revision importing anything but alembic, SQLAlchemy and a few standard modules (`datetime`, `enum`, `uuid`,
  `decimal`, `json`, `re`, ...): rendering it would run project code at translation time;
- `op.bulk_insert` into a JSON, JSONB, ARRAY, binary, interval, pickle or custom (`TypeDecorator`) column: offline
  mode writes these values as literals, which differ from what an online run binds (a `None` in a JSON column is
  SQL `NULL` offline, the JSON `null` online);
- a revision on which alembic's offline mode fails.

`downgrade` is not supported.

## Differences

- A failing statement is reported on one line (`py2axum migrate: <the database's error>`), where alembic prints a
  Python traceback; the database is left as alembic leaves it (the transaction rolled back).
- A database whose version table names a revision that is not on the binary's upgrade path (another branch, a
  revision written after the build) is refused; alembic would compute a path.
- `sa.Enum` in `op.create_table` creates its type without checking whether it exists, like alembic online (measured:
  both fail with `type "..." already exists`).
