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
  (lists; `contains`/`contained_by`/`overlap`/`any`), deferred columns,
  `deferred(Column(...), group=, raiseload=)` and `mapped_column(deferred=True, deferred_group=,
  deferred_raiseload=)` (left out of what a query loads: reading one then loads it, with the other unloaded
  columns of its group, MissingGreenlet in an async session; with `raiseload` it raises `InvalidRequestError`
  instead; `select(Model.col)` and `session.refresh(obj, ["col"])` read it), and the column loader options
  `load_only(*cols, raiseload=)`, `defer(col, raiseload=)`, `undefer(col)`, `undefer("*")` and
  `undefer_group(name)`: in `select(...).options()`, `session.get(..., options=[...])`, after a relationship
  loader (`selectinload(A.b).load_only(B.x)`, `.options(...)`) or `defaultload(A.b)` (kept for the
  relationship's lazy load), applied as SQLAlchemy applies them (the primary key always loaded; an object of the
  identity map that has not loaded a column gets it from the row; an expired object's deferred columns follow
  the mapping again); the SELECT still names every column (the SQL text differs, the objects do not),
  `composite()` (`composite(mapped_column("x1"), mapped_column("y1"))` typed by the fields of the dataclass of
  `Mapped[Point]`, `composite(Point, x1, y1)`, `composite("x1", "y1")`, `composite(Cls.factory, ...)`,
  `column_template="home_%s"`; `Mapped[Point | None]` gives None when every column is NULL; the value's
  columns are a dataclass's fields in order or its `__composite_values__()`): on an instance the object built
  when the row loads, kept in `__dict__` until a flush or a refresh of its columns (then built again, as
  SQLAlchemy's handlers do), set column by column; on the class its columns, selected as one value and
  compared column by column (`==`, `!=`, `<`... ANDed, `== None` as all NULL). One difference: setting one of
  its columns directly rebuilds the object at the next read (SQLAlchemy keeps the stale one until a flush),
  a SQL column name other than the attribute (`extra_data = Column("metadata", JSON)`: SQL uses the column
  name, rows and `session.get` the attribute key, `Model.attr.name` is the column's name), the type given as `mapped_column(..., type_=T)` (not
  with a positional type too), `T.with_variant(V, "postgresql")` (V; other dialects' variants
  are ignored), `col.op("...")(value)` (the value typed like the column, as SQLAlchemy does), a column type returned by a project function
  (`def _enum(cls, name): return Enum(cls, name=name, ...)`, inlined; its arguments must be literals or names),
  project `TypeDecorator`s
  (`process_bind_param`/`process_result_value` without `self`/`dialect`), the declarative base's
  `type_annotation_map` (a dict literal; a generic is matched as written, `list[int]` taking the entry of `list`
  is refused) and SQLAlchemy's defaults for `Mapped[Decimal]` (`Numeric`) and `Mapped[datetime.time]` (`Time`).
  Methods of mapped classes:
  `__init__` (the class's own constructor, run on the new object, positional arguments included),
  plain, `@property`, `@staticmethod` and `@classmethod` (`cls(...)` builds an instance), and
  `sqlalchemy.ext.hybrid`'s `@hybrid_property` / `@hybrid_method` (Python on an instance, SQL on the class),
  with `@x.expression` / `@x.setter` / `@x.comparator` (or `@x.inplace.expression` / `.setter` / `.comparator`,
  over `@classmethod` or not; the other hybrid modifiers are refused); other decorators (`@validates`...) are
  refused. A hybrid's setter also runs for a keyword of the default constructor (`Vertex(start=Point(3, 4))`).
- `sqlalchemy.ext.hybrid.Comparator`, translated from Python source of py2axum's own written like SQLAlchemy's
  class: subclasses (a custom comparator returned by `@x.comparator`, or a hybrid value object, a plain class or a
  `@dataclass(eq=False)`) get `__init__(expression)`, `__clause_element__()` and the comparison operators
  (`==`, `!=`, `<`, `<=`, `>`, `>=`, reflected ones included) sent to `operate(op, other)` with `operator.eq` & co.;
  an object with `__clause_element__()` is coerced to SQL wherever SQL is expected (`select()`, `where()`,
  `order_by()`, function arguments, the other side of a comparison), also on an `aliased()` class's hybrid.
- Session: identity map (weak, like SQLAlchemy), autoflush, implicit transaction, `get` (scalar, tuple,
  list or dict identities), `add`/`add_all` (a detached object is persistent in the session again; another instance with its key raises `InvalidRequestError`), `delete`/`flush`/`commit`, `rollback` (the objects a flush of the transaction deleted are persistent again, expired), `refresh`, `close` (the objects
  are detached, then the transaction is rolled back; the session stays usable), `obj in session`,
  `Session(engine)` / `AsyncSession(engine)` in an endpoint (`bind=`, `expire_on_commit=`, `autoflush=`; its
  `with` block closes it),
  `expunge`/`expunge_all` (the objects are detached: reading what they have not loaded raises
  `DetachedInstanceError`), `expire_all`, `expire(obj)` (with the `refresh-expire` cascade of `cascade="all"`) and
  `expire(obj, ["col", "rel"])` (only those attributes; their changes not flushed are lost),
  `connection()` (a readiness probe: `close()` on it ends the transaction, then statements raise
  `ResourceClosedError` and `commit()` "This transaction is inactive" until `rollback()`/`close()`),
  `expire_on_commit`, savepoints (`begin_nested()` then `commit()`/`rollback()`), `session.bind`,
  `get_bind()`. A flushed UPDATE that matches no row (deleted by another statement) raises `StaleDataError`
  (`sqlalchemy.orm.exc`), as SQLAlchemy does. Server-generated values (identity, `server_default`, SQL defaults) are fetched with
  `RETURNING` at insert, like `eager_defaults="auto"`; `__mapper_args__ = {"eager_defaults": True}` also fetches
  the SQL `onupdate=` values with the UPDATE (other `__mapper_args__` keys are refused).
- Relationships (many-to-one, one-to-many, many-to-many through `secondary=` naming a module-level `Table`, by
  its variable or its table name), `lazy=` select/selectin/joined/noload/raise, `selectinload()`
  chains, `back_populates`/`backref` (not on a many-to-many relationship: SQLAlchemy keeps both collections in
  step at once, which is refused), cascades (save-update, delete, delete-orphan; none on many-to-many, whose
  association rows are written at flush and deleted with their owner), `passive_deletes`,
  `order_by=` (target columns, `.desc()`, or a string SQLAlchemy evaluates such as `"[Child.a, Child.b.desc()]"`),
  self-referential relationships (adjacency list: one-to-many by default, many-to-one with `remote_side=` naming
  the referenced column; a row made its own parent raises `CircularDependencyError` at flush, as without
  `post_update`), `primaryjoin=` equating a foreign key with the column it references (`widget_id ==
  Entry.widget_id`, `Entry.entry_id == favorite_entry_id`; strings, lambdas and other conditions are refused),
  `post_update=True` on a many-to-one relationship (its foreign key NULL in the INSERT then set by an UPDATE after
  the INSERTs, set to NULL by an UPDATE before the DELETEs: two tables referencing each other).
- Inheritance (`__mapper_args__` with `polymorphic_on` on the base and `polymorphic_identity` on each class): a row
  is built as the class of its discriminator (every mapped subclass is known, even one the code never names), the
  discriminator is set when an object is created, and a subclass's own columns are read on access after a query on
  the base (`MissingGreenlet` in an async session, like SQLAlchemy). Single table (no `__tablename__` on the
  subclass): a query on a subclass reads only the rows of its identities and those of its subclasses (also through
  `select_from`, columns, joins, bulk `update()`/`delete()`). Joined table (the subclass's own table, its primary
  key a foreign key to the parent's under the same name): a subclass reads its tables joined on the key, the unit of
  work writes one INSERT/UPDATE/DELETE per table, ORM bulk `insert(Subclass)` goes row by row, bulk `update()`
  writes the table of the columns it sets (`FROM` the others its criteria name) and `delete()` the subclass's
  table. Loading subclass columns with the query (what an async session can then read without IO):
  `selectin_polymorphic(Base, [Sub, ...])`, also chained (`selectinload(A.rel).selectin_polymorphic([...])`) or
  inside `.options()`; `with_polymorphic(Base, [Sub, ...] | "*")` (its subclass namespaces `poly.Sub.col` in the
  criteria, the subclass tables LEFT OUTER JOINed, also through `A.rel.of_type(poly)` in a join or a
  `selectinload()`); `__mapper_args__ = {"polymorphic_load": "selectin" | "inline"}`; options on a subclass's
  relationship next to a query on its base (`selectinload(Sub.rel)`). `A.rel.of_type(Sub)` joins along a
  relationship to a subclass. `with_polymorphic(Base, [...], aliased=True, flat=...)`: an alias of the base whose
  subclass tables are LEFT OUTER JOINed under aliases of their own (`flat=` only changes the SQL text: aliased
  tables or a subquery, the same rows), two of them joined to each other, `poly.Sub.col` under the alias, the
  subclass columns of its objects loaded with the query (a column the base maps too is the base table's, as in
  SQLAlchemy). Refused: concrete inheritance, `with_polymorphic(selectable=, polymorphic_on=)`, aliased on a
  joined subclass (not the base), a relationship through `poly.Sub`, an alias of a joined subclass, a bulk update
  of columns of two tables.
- Write-only collections (`WriteOnlyMapped[...]` or `lazy="write_only"`, one-to-many or many-to-many, with
  `passive_deletes=True`): never loaded; members given to a new object's constructor, `add()`/`add_all()`/`remove()`
  applied at the next flush (foreign key set or cleared, orphans deleted, association rows written), and
  `select()`, `insert()` (one-to-many, executemany and `returning()` included), `update()` and `delete()` bound to
  the parent. Eager loading and replacing the collection of a persistent object raise `InvalidRequestError` like
  SQLAlchemy; `DynamicMapped` / `lazy="dynamic"` are refused.
- Core tables: a module-level `Table(name, metadata, Column(name, ...), ...)` (constraints and indexes are the
  DDL's business; `TypedColumns` and other `Table()` options are refused) is a value: `t.c.col`, `t.c["a", "b"]`,
  `t.name`, `select(t)` (its columns, rows of values), `insert(t)`, `update(t)`, `delete(t)`, `t.select()`,
  `t.alias()` (`a.c.col`), in joins like a mapped class, and `t.join(right, onclause=None, isouter=, full=)` /
  `t.outerjoin(...)` (chained, the ON clause from the foreign key when omitted), a Join given to `select_from()`
  (it supersedes the FROM entries of its tables, as in SQLAlchemy). `bindparam("name", value=None)` takes its value from the
  execution's parameters (`StatementError` "A value is required for bind parameter" without one); a list of
  dicts runs an `insert()`/`update()`/`delete()` once per dict (executemany): the keys of the first dict are the
  statement's (a later dict lacking one raises before anything runs, its other keys are ignored), keys that are
  not `bindparam()` names are SET / VALUES columns, the rowcounts are summed (an INSERT's is -1 over psycopg).
  Through the `Session`, a list with an ORM entity's `update(...).where(...)` raises SQLAlchemy's
  `InvalidRequestError` (bulk synchronize); `session.connection().execute(stmt, params)` runs it as Core, never
  synchronized. `insert(Model).values(...)` with a list: one INSERT per dict (the ORM's leaves a `None` out),
  `returning()` rows in the order of the dicts.
- Core: `select` (entities, columns, labels, `*cols`), `where`/`filter_by` (the FROM list is the tables of the
  columns and of the WHERE criteria; a nested SELECT with more than one FROM leaves out the enclosing
  statement's, SQLAlchemy's auto-correlation, scalar subqueries of the columns included), joins (`full=True`
  as FULL OUTER JOIN; without an ON clause, from the foreign key between the two sides, `join_from(left,
  right)` taking its left side, a target alias or subquery included; explicit, inferred from
  the single foreign key, relationship, `join(Target, Model.rel)` also to an alias, `join(alias.rel)`, through a
  many-to-many association table; `select_from(A).join(B.rel_to_A)` starts from B like SQLAlchemy),
  relationship criteria (`Child.parent == obj` / `!= obj` / `== None`, `Parent.children.contains(obj)`,
  `any()`/`has()`, `with_parent(obj, Parent.rel)`), `filter_by()` on columns, composites and hybrid properties
  (an unknown name raises SQLAlchemy's `InvalidRequestError`), a named alias as the key of its entity in a row
  (`row.u1`), `join(A.rel.of_type(alias))`, `join(A.rel.and_(criteria))`
  (also from an alias, `alias.rel.of_type(...)`), `add_columns(...)`, `aliased(Model, name="x")`, `aliased(Model, flat=True)`
  (a plain table: the same alias), `aliased(Model, select(Model)....subquery())` or `aliased(Model, cte)` (the entity read
  from the subquery, which must select that entity alone, or a union of such selects),
  `execution_options(populate_existing=, autoflush=, yield_per=)` on a SELECT (also given to
  `session.execute(stmt, execution_options={...})`),
  ORM bulk INSERT (`session.execute(insert(Model), [dicts])`, `scalars(insert(Model).returning(Model), [dicts])`:
  one statement per dict in order, `None` values left out unless `execution_options(render_nulls=True)`,
  `returning(..., sort_by_parameter_order=True)`) and bulk UPDATE by primary key (`execute(update(Model),
  [dicts])`, applied to the loaded objects), their results as SQLAlchemy's (`first()` None, `all()`
  ResourceClosedError...); `insert().values([dicts])` takes the columns of the first dict and those with a
  Python default, a `select()` value is its scalar subquery,
  `aliased`, subqueries, `exists` (`exists(select)`, `select.exists()`,
  and `exists().where(...)`, whose FROM is the tables of its criteria less those of the enclosing statement:
  SQLAlchemy's auto-correlation), `in_` (lists, selects, `tuple_`),
  `like/ilike/startswith/contains`, `icontains/istartswith/iendswith` (`lower(col) LIKE ...` as SQLAlchemy 2),
  `regexp_match` (`~`, `~*` with `flags="i"`; other flags raise), `is_/is_not`, `is_distinct_from`,
  `case` (also `case(*whens)` from a list or generator built at run time; no WHEN is built, then refused by
  PostgreSQL, as with SQLAlchemy), `literal`, `literal_column`, `cast` (also to `Numeric`), `extract`,
  `func.*` (with `FILTER`), window functions (`.over(partition_by=, order_by=)`, a clause or a list/tuple of
  them; frames `rows=`/`range_=`/`groups=` are refused at translation), CTEs (`select(...).cte("name")` or anonymous,
  read in the FROM list, a join, a subquery or another CTE: every CTE goes to the WITH clause of the outermost
  statement, the ones it reads first; `recursive=` and `nesting=` are refused), LATERAL subqueries
  (`.lateral()`, correlated to the FROM list of their statement, joined `ON` `true()` or `literal_column("TRUE")`),
  `correlate(*entities)` (those enclosing tables taken out of the FROM list, even when a single FROM is left) and
  `correlate(None)`, `union_all()`/`union()`/`intersect[_all]()`/`except_[_all]()` (functions or methods) made a
  subquery, a CTE or executed (members with their own ORDER BY / LIMIT, and an ORDER BY / LIMIT of the compound,
  are refused), `postgresql.array([...])` with `any_()`/`all_()`, `bindparam(key, type_=ARRAY(String()))`,
  `group_by/having/order_by/limit/offset/distinct(on)` (an expression built once and
  used in the columns and in `GROUP BY` shares its bound parameters, like SQLAlchemy's bind objects; two
  identical expressions built apart do not, and PostgreSQL rejects the grouping, as it does for SQLAlchemy),
  `with_for_update`,
  `update()`/`delete()` (with `synchronize_session`; `returning`, of columns only for a DELETE), `insert()` (core and postgresql dialect: several rows,
  Python column defaults, `on_conflict_do_update(index_elements= | constraint=, set_=, where=)`,
  `on_conflict_do_nothing` (a `constraint=` name SQLAlchemy would not quote),
  `excluded`, `returning`), `text()` with `:named` parameters (and `text(...).bindparams(name=value)`, also inside
  a `where()`; a list of dicts runs it once per dict, executemany: no rows, `rowcount` summed),
  `+` with a string as `||` like SQLAlchemy, `Result.scalars/all/first/one/scalar/unique/
  mappings/fetchone`, rows with attribute access (`row.total`, `_mapping`, `_asdict()`).
- Table and column names quoted like SQLAlchemy's PostgreSQL dialect (`"user"`, `"order"."end"`); a label used
  as an operand is its expression (`x.label("n") > 3`), `select(...).label("n")` a labeled scalar subquery.
- SQL typing like SQLAlchemy: arithmetic and `FILTER` keep the column type, `func.round`/`avg` untyped
  (Decimal); untyped integers are bound as int2/int4/int8 like psycopg; NUMERIC results are `Decimal`.
- Database errors are SQLAlchemy's classes over psycopg's (`IntegrityError`, `DataError`...), their message
  `(psycopg.errors.<class of the SQLSTATE>) <server message>` (`NumericValueOutOfRange`, `UniqueViolation`...),
  `e.orig` the psycopg exception (its class name and message).
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
- `obj.__dict__` of a mapped object: `_sa_instance_state` then the loaded attributes (a snapshot). After an
  INSERT it holds what SQLAlchemy's does: the columns given, client-side and server-side defaults (fetched by
  `RETURNING`), the key, a foreign key whose relationship was set; a column never set and without an insert
  default (`default=None` included) reads `None` without IO but stays out of it until set, refreshed or returned
  by a later `SELECT` (which loads it from the row). A collection assigned whole sets each member's back
  reference at once; members appended or removed have it set at flush.
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
- `Model.__table__` as a Core table, when the code uses it as one (`t.join(...)`, `t.c.col`, given to `select()`,
  `select_from()`, `join()`...): what a module-level `Table()` supports (above), its columns by their column name,
  rows of values. Decided per value at translation: one value (`t = Model.__table__`) both used as a Core table and
  iterated for introspection, or given to other code, is refused; so is the table of an inheriting class.

## What stays in Python

- `execute(select/delete(...), parameters)`, an INSERT or UPDATE with its own `where()`/`values()` and a parameter
  list (executemany with `bindparam()`), `update(...).returning(...)` with a parameter list: refused at translation.

- `connect_args`: other libpq `options` switches are refused (a literal string, at translation; a
  computed one raises `ValueError` when the engine is created).
- `create_all`, refused at their line: `@declared_attr`, `Sequence`, DDL event listeners
  (`before_create`/`after_create`...), tables in another schema, `Enum(metadata=...)`, TypeDecorators that
  override `load_dialect_impl`/`__init__`, `values_callable` other than `lambda x: [e.value for e in x]`,
  any value the evaluator cannot build, a model module only imported inside another function, any other
  `run_sync` function.

## Differences

- The SQL text of a statement or an expression (`str(select(...))`, `str(col == 1)`, an f-string or `format()` of
  one, `print(stmt)`) raises `TypeError` ("py2axum: the SQL text of a statement or an expression ... is not
  supported"): SQLAlchemy writes it with its default compiler (`:param_1` placeholders, generic types, line breaks
  before `FROM`/`WHERE`), the binary compiles for PostgreSQL only. The text of a column (`str(User.name)`, which
  is `user_account.name`) is supported. The value is known only at run time, hence a run-time refusal.
- **Known difference:** the binary issues one UPDATE per modified object; when several stale rows of the
  same table are flushed together, SQLAlchemy's `StaleDataError` message counts the whole batch
  ("expected to update 2 row(s)"), the binary's the first object ("1 row(s)").
- `parent.children.append(x)` (or `remove(x)`) sets the foreign key and `x.parent` at flush, not before it
  (SQLAlchemy sets `x.parent` immediately through the backref event); assigning the whole collection sets it at once.
- In an async session, reading what needs IO (an expired or deferred column, the primary key of an object
  expired as a whole, a relationship not loaded) raises what the locked SQLAlchemy raises: `MissingGreenlet`
  up to 2.0; since 2.1 a `StatementError` `(sqlalchemy.exc.MissingGreenlet) greenlet_spawn has not been
  called; ...` whose `.orig` (also `__cause__`) is the `MissingGreenlet`: `except MissingGreenlet` does not
  catch it, `except StatementError` (or `SQLAlchemyError`) does.
- **Known difference:** `str()` of a database error or of a `StatementError` stops at its first line
  (`e.args[0]`), without SQLAlchemy's `[SQL: ...]`, `[parameters: ...]` and background-link lines (the binary's
  SQL is not SQLAlchemy's text; for a lazy one-to-many load SQLAlchemy's parameters even name a memory
  address). `e.orig` (also `e.__cause__`) is there, with its own message; `e.statement` and `e.params` are not
  (AttributeError).
- **Known difference:** the class-level attribute of a hybrid with a custom comparator (`@x.comparator`) is the
  comparator object itself, where SQLAlchemy returns a proxy to it: `isinstance(Model.x, Comparator)` is true
  (SQLAlchemy: false) and its `hash()` follows the comparator class (unhashable when it defines `__eq__` only).
  Comparisons, `select()`, `filter_by()` and attribute reads behave alike.
- **Known difference:** a `Comparator` subclass (a custom comparator, a hybrid value object) has the comparison
  operators only: SQLAlchemy's other `ColumnOperators` (`like()`, `in_()`, `+`, ...) raise `AttributeError` or
  `TypeError` on it in the binary.
- **Known difference:** in `create_all`, a cycle of foreign keys partly created already is closed by
  `ALTER TABLE` where SQLAlchemy, which only sorts the tables it creates, may inline the constraint: same
  resulting schema.
