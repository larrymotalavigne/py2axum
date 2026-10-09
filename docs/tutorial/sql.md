# SQL databases

FastAPI applications usually reach PostgreSQL through SQLAlchemy 2.0: declarative models, a session per
request, `select()` statements. The binary embeds its own implementation of the SQLAlchemy session (identity
map, unit of work, relationships, loaders) and of its SQL compiler, on sqlx, for PostgreSQL only. Async and
synchronous sessions are both supported.

Two mapped classes with a relationship, the Pydantic schemas read from them (`from_attributes`), and a
CRUD router: create (an `IntegrityError` turned into a 409), list with pagination and a filter, read with
`selectinload`, partial update (`exclude_unset`), delete, and an aggregate with an outer join. The session
dependency and the engine are in `docs_src/db.py`; the tables are created by the
[lifespan](lifespan.md). Like every example on this site, it is compiled and compared with FastAPI in CI ([how](testing.md)).

```python title="docs_src/db.py"
--8<-- "docs_src/db.py"
```

```python title="docs_src/tutorial/sql.py (models)"
--8<-- "docs_src/tutorial/sql.py:models"
```

```python title="docs_src/tutorial/sql.py (schemas)"
--8<-- "docs_src/tutorial/sql.py:schemas"
```

```python title="docs_src/tutorial/sql.py (routes)"
--8<-- "docs_src/tutorial/sql.py:routes"
```

## What is native

- Declarative models (`Mapped[...]`, `mapped_column`, types, `default=`/`server_default=`/`onupdate=`
  (value, callable or SQL), `unique`, `nullable`, `ForeignKey` (a foreign key without a type takes the
  referenced column's), composite primary keys, `Identity()`, `JSON`/`JSONB` (`none_as_null=`; on `JSONB`, the
  operators `contains` (`@>`), `contained_by` (`<@`), `has_key` (`?`), `has_any` (`?|`), `has_all` (`?&`), their
  argument typed like SQLAlchemy types it: a list given to `has_any`/`has_all` is bound as JSONB, which PostgreSQL
  rejects in both implementations; on both, an index by a str or an int, `col["k"]` / `col[0]`, and its accessors
  `as_string()`, `as_integer()`, `as_float()`, `as_boolean()`, `as_numeric(precision, scale)`, `as_json()`, rendered
  as SQLAlchemy renders them, `CAST(col ->> 'k' AS VARCHAR)`; a JSON path, `col[("a", "b")]`, is refused at
  translation, and a bare index compared or used in arithmetic is refused at run time: compare an accessor),
  `Uuid`/`UUID` and `Mapped[uuid.UUID]` (read as `uuid.UUID`, a str bound to it is cast by PostgreSQL as with
  psycopg; `as_uuid=False` is refused),
  `Numeric` (`Decimal`, or float with `asdecimal=False`), `Enum` columns, `LargeBinary` (bytes), `ARRAY(String)`
  (lists; `contains`/`contained_by`/`overlap`/`any`), `deferred(Column(...))` and `mapped_column(deferred=True)`
  (left out of what a query loads: reading it then is a lazy load, MissingGreenlet in an async session;
  `select(Model.col)` and `session.refresh(obj, ["col"])` read it, and so does the loader option
  `undefer(Model.col)`: in `select(...).options()`, `session.get(..., options=[...])` or after a relationship
  loader, `selectinload(A.b).options(undefer(B.col))`, also into an object of the identity map that has not
  loaded it),
  a SQL column name other than the attribute (`extra_data = Column("metadata", JSON)`: SQL uses the column
  name, rows and `session.get` the attribute key, `Model.attr.name` is the column's name), `T.with_variant(V, "postgresql")` (V; other dialects' variants
  are ignored), `col.op("...")(value)` (the value typed like the column, as SQLAlchemy does), a column type returned by a project function
  (`def _enum(cls, name): return Enum(cls, name=name, ...)`, inlined; its arguments must be literals or names),
  project `TypeDecorator`s
  (`process_bind_param`/`process_result_value` without `self`/`dialect`). Methods of mapped classes:
  plain, `@property`, `@staticmethod` and `@classmethod` (`cls(...)` builds an instance); other decorators
  (`@hybrid_property`, `@validates`...) are refused.
- Session: identity map (weak, like SQLAlchemy), autoflush, implicit transaction, `get` (scalar, tuple,
  list or dict identities), `add`/`add_all`/`delete`/`flush`/`commit`/`rollback`/`refresh`/`close`,
  `connection()` (a readiness probe: `close()` on it ends the transaction, then statements raise
  `ResourceClosedError` and `commit()` "This transaction is inactive" until `rollback()`/`close()`),
  `expire_on_commit`, savepoints (`begin_nested()` then `commit()`/`rollback()`), `session.bind`,
  `get_bind()`. A flushed UPDATE that matches no row (deleted by another statement) raises `StaleDataError`
  (`sqlalchemy.orm.exc`), as SQLAlchemy does. Server-generated values (identity, `server_default`, SQL defaults) are fetched with
  `RETURNING` at insert, like `eager_defaults="auto"`.
- Relationships (many-to-one, one-to-many), `lazy=` select/selectin/joined/noload/raise, `selectinload()`
  chains, `back_populates`/`backref`, cascades (save-update, delete, delete-orphan), `passive_deletes`,
  `order_by=` (target columns, `.desc()`, or a string SQLAlchemy evaluates such as `"[Child.a, Child.b.desc()]"`),
  self-referential relationships (adjacency list: one-to-many by default, many-to-one with `remote_side=` naming
  the referenced column; a row made its own parent raises `CircularDependencyError` at flush, as without
  `post_update`).
- Core: `select` (entities, columns, labels, `*cols`), `where`/`filter_by`, joins (explicit, inferred from
  the single foreign key, relationship), `aliased`, subqueries, `exists` (`exists(select)`, `select.exists()`,
  and `exists().where(...)`, whose FROM is the tables of its criteria less those of the enclosing statement:
  SQLAlchemy's auto-correlation), `in_` (lists, selects, `tuple_`),
  `like/ilike/startswith/contains`, `regexp_match` (`~`, `~*` with `flags="i"`; other flags raise), `is_/is_not`, `is_distinct_from`, `case`, `literal`, `cast`, `extract`,
  `func.*` (with `FILTER`), `group_by/having/order_by/limit/offset/distinct(on)` (an expression built once and
  used in the columns and in `GROUP BY` shares its bound parameters, like SQLAlchemy's bind objects; two
  identical expressions built apart do not, and PostgreSQL rejects the grouping, as it does for SQLAlchemy),
  `with_for_update`,
  `update()`/`delete()` (with `synchronize_session`; `returning`, of columns only for a DELETE), `insert()` (core and postgresql dialect: several rows,
  Python column defaults, `on_conflict_do_update(index_elements= | constraint=, set_=, where=)`,
  `on_conflict_do_nothing` (a `constraint=` name SQLAlchemy would not quote),
  `excluded`, `returning`), `text()` with `:named` parameters (and `text(...).bindparams(name=value)`, also inside
  a `where()`; a list of dicts runs it once per dict, executemany: no rows, `rowcount` summed),
  `+` with a string as `||` like SQLAlchemy, `Result.scalars/all/first/one/scalar/unique/
  mappings`, rows with attribute access (`row.total`, `_mapping`, `_asdict()`).
- SQL typing like SQLAlchemy: arithmetic and `FILTER` keep the column type, `func.round`/`avg` untyped
  (Decimal); untyped integers are bound as int2/int4/int8 like psycopg; NUMERIC results are `Decimal`.
- Database errors are SQLAlchemy's classes over psycopg's (`IntegrityError`, `DataError`...), their message
  `(psycopg.errors.<class of the SQLSTATE>) <server message>` (`NumericValueOutOfRange`, `UniqueViolation`...).
  An integer bound to an `Integer`/`SmallInteger` column is cast like SQLAlchemy's psycopg dialect does
  (`::INTEGER`): out of range, a `DataError`.
- The PostgreSQL session time zone: sqlx forces UTC, the runtime applies the one psycopg would see
  (role/database setting, then server config, or `PY2AXUM_DB_TIMEZONE`), unless the engine sets one.
- Session parameters of `create_async_engine`/`create_engine(connect_args=...)`: psycopg's libpq
  `options` (`-c name=value`, `-cname=value`, `--name=value`, `\` escapes) and asyncpg's
  `server_settings` dict are set on every pool connection; a `TimeZone` among them is the zone timestamptz
  are decoded in (psycopg). Other `connect_args` keys (timeouts, `sslmode`, ...) have no effect.
- The driver named by `DATABASE_URL` (`postgresql+psycopg://` or `postgresql+asyncpg://`): psycopg returns
  timestamptz in the session's zone, asyncpg as `datetime.timezone.utc` (Pydantic writes `Z`); an ORM-enabled
  `insert(Model)` without `returning` reports `rowcount` -1 over psycopg, the rows inserted over asyncpg.
- `obj.__dict__` of a mapped object: `_sa_instance_state` then the loaded attributes (a snapshot).
- `create_async_engine(...)` is the binary's pool (one database, `DATABASE_URL`; its options are ignored
  but `connect_args`, above);
  `async with engine.connect() as conn` (rolled back on exit), `async with engine.begin() as conn` (committed on
  exit, rolled back when the block raises); the same with `with` on a `create_engine(...)` engine.
- `await conn.run_sync(Base.metadata.create_all)` (e.g. in the lifespan): the DDL is compiled at translation
  time by SQLAlchemy itself (it must be installed next to py2axum; 2.x). The mapped classes of the base, from
  the modules the application imports (plus those imported in the calling function), are rebuilt as real
  SQLAlchemy classes from their source by a static evaluator that only calls SQLAlchemy: column types and
  options, `Mapped[...]` annotations and the base's `type_annotation_map`, mixins, `__table_args__`, the
  base's `metadata = MetaData(naming_convention=...)`, module-level `Table(...)` and `Index(...)`, project
  enums, project `TypeDecorator`s (their `impl`), project functions whose body is `return <type>`.
  Python-side defaults never run (only their presence matters: a primary key with a default is not SERIAL).
  At run time the statements are replayed with `checkfirst=True` like SQLAlchemy's PostgreSQL dialect:
  every named enum type absent from `pg_type` is created (even when its table exists), then each table
  absent from `pg_class` with its indexes and comments, then the foreign keys of cycles (`use_alter`) of the
  tables created. The DDL is the one of the SQLAlchemy that ran the translation.
- Synchronous sessions (`create_engine`, `sessionmaker`, `Session` parameters of `def` endpoints, a
  generator dependency `s = maker()` / `try: yield s` / `finally: s.close()` or `with maker() as s:
  yield s`): the same session semantics, run on the async pool (FastAPI's threadpool is not modelled:
  only the observable behaviour is). Reading an expired column or a relationship that is not loaded emits
  the SQL like SQLAlchemy's lazy loader (autoflush first; `ObjectDeletedError` when the row is gone),
  including while a `response_model` or a model built from ORM objects (`Page(items=rows)`) reads the
  attributes, `@property`s of the model included. Awaiting a synchronous session's method
  (`await db.execute(...)`) runs it, then raises CPython's `TypeError`. An application uses one kind of
  session dependency (sync or async), not both.

The session dependency is described in [Dependencies](dependencies.md); large lists read straight from the
session are streamed ([Large list responses](../advanced/streaming.md)). The binary does not run migrations:
run Alembic (or `create_all`, above) before it serves.

- `Model.__table__` for introspection: the table's `name`, its `columns`/`c` (`key in`, `[key]` or index,
  iteration, `len`, `keys()`, `values()`, `get()`), each column's `name`, `key`, `nullable`, `primary_key` and
  `type`; a type supports `isinstance(col.type, JSON)` against any SQLAlchemy type class, `str()` and `repr()`
  (computed by SQLAlchemy at translation time, which needs it installed next to py2axum). Any other attribute
  raises a `TypeError` naming what is supported.

## What stays in Python

- `execute(insert/update/delete/select(...), parameters)` (executemany, ORM bulk INSERT / UPDATE by primary
  key) and `scalars()`/`scalar()` with parameters: refused at translation (only `execute(text(...), parameters)`).

- `connect_args`: other libpq `options` switches are refused (a literal string, at translation; a
  computed one raises `ValueError` when the engine is created).
- `create_all`, refused at their line: `@declared_attr`, `Sequence`, DDL event listeners
  (`before_create`/`after_create`...), tables in another schema, `Enum(metadata=...)`, TypeDecorators that
  override `load_dialect_impl`/`__init__`, `values_callable` other than `lambda x: [e.value for e in x]`,
  any value the evaluator cannot build, a model module only imported inside another function, any other
  `run_sync` function.

## Differences

- **Known difference:** the binary issues one UPDATE per modified object; when several stale rows of the
  same table are flushed together, SQLAlchemy's `StaleDataError` message counts the whole batch
  ("expected to update 2 row(s)"), the binary's the first object ("1 row(s)").
- `parent.children.append(x)` sets the foreign key at flush but not `x.parent` before it
  (SQLAlchemy does it immediately through the backref event).
- **Known difference:** `str()` of a database error stops at the message above, without
  SQLAlchemy's `[SQL: ...]`, `[parameters: ...]` and background-link lines (the binary's SQL is not SQLAlchemy's
  text).
- **Known difference:** in `create_all`, a cycle of foreign keys partly created already is closed by
  `ALTER TABLE` where SQLAlchemy, which only sorts the tables it creates, may inline the constraint: same
  resulting schema.
